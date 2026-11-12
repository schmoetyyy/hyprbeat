//! `--headless`: render exactly one frame offscreen and write it as a PNG
//! (§8 debug tooling). No compositor, no layer-shell — used by CI and quick
//! iteration, and by `dev/validate-phase1.sh` to check the scale math.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::mpsc;

use crate::Error;
use crate::cli::Args;
use crate::render::{self, FrameParams, Renderer};

/// Deterministic animation time for the snapshot, so the output is stable
/// enough for pixel-diff regression tests later on.
const HEADLESS_TIME_MS: f64 = 1234.0;

pub fn render_png(args: &Args) -> Result<PathBuf, Error> {
    let logical = args.size;
    let scale = args.scale;
    let (bw, bh) = render::scale::buffer_size(logical, scale);

    let gpu = render::Gpu::new(None)?;
    // Rgba8UnormSrgb: guaranteed renderable everywhere, no channel swizzle
    // needed when writing the PNG.
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let renderer = Renderer::new(&gpu.device, format);

    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("hyprbeat headless target"),
        size: wgpu::Extent3d {
            width: bw,
            height: bh,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let frame = FrameParams {
        logical: (logical.0 as f32, logical.1 as f32),
        buffer: (bw as f32, bh as f32),
        time_ms: HEADLESS_TIME_MS as f32,
        testcard: args.testcard,
        corners: render::corners_for_frame(HEADLESS_TIME_MS, true),
        accent: render::accent_for_frame(true),
    };

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("hyprbeat headless"),
        });
    renderer.render(&gpu.queue, &mut encoder, &view, &frame);

    // copy into a CPU-readable staging buffer (rows padded to 256 bytes)
    let bytes_per_row = (bw * 4).next_multiple_of(256);
    let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("hyprbeat headless staging"),
        size: (bytes_per_row * bh) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(bh),
            },
        },
        wgpu::Extent3d {
            width: bw,
            height: bh,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([encoder.finish()]);

    let (tx, rx) = mpsc::channel();
    staging
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("GPU poll failed: {e}"))?;
    rx.recv()
        .map_err(|_| "GPU dropped the mapping callback".to_string())?
        .map_err(|e| format!("buffer mapping failed: {e}"))?;

    let padded = staging
        .slice(..)
        .get_mapped_range()
        .map_err(|e| format!("buffer mapping failed: {e}"))?;
    let mut pixels = Vec::with_capacity((bw * bh * 4) as usize);
    for row in 0..bh {
        let start = (row * bytes_per_row) as usize;
        pixels.extend_from_slice(&padded[start..start + (bw * 4) as usize]);
    }
    drop(padded);
    staging.unmap();

    // I-07: write only where the user pointed us, else the system temp dir.
    let path = args
        .out
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("hyprbeat-headless.png"));
    let file = File::create(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), bw, bh);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("png header: {e}"))?;
    writer
        .write_image_data(&pixels)
        .map_err(|e| format!("png data: {e}"))?;
    writer.finish().map_err(|e| format!("png finish: {e}"))?;

    tracing::info!(
        path = %path.display(),
        logical = ?logical,
        scale,
        buffer = ?(bw, bh),
        "headless frame written"
    );
    Ok(path)
}
