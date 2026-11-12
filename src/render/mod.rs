//! wgpu rendering core (§3.5, D-05).
//!
//! Phase 1 draws everything with a single pipeline and a single draw call: a
//! fullscreen triangle whose fragment shader blends the four theme-corner
//! colors and can overlay the scale test card. The instanced-quad pipeline
//! for gameplay sprites replaces this in later phases, but the `Gpu`/`Renderer`
//! split and the uniform discipline stay.

pub mod headless;
pub mod scale;

use crate::Error;
use crate::config::theme::{self, Color};

pub const SHADER: &str = include_str!("shader.wgsl");

/// Size of the uniform block; must match `struct U` in `shader.wgsl`.
pub const UNIFORM_SIZE: usize = 112;

/// Bit layout of the `flags` uniform word (phase 2: the old `testcard` bool
/// grew into a bitfield without changing the block size).
pub const FLAG_TESTCARD: u32 = 1 << 0;
pub const FLAG_DEGRADED: u32 = 1 << 1;

/// An owned Wayland display handle that is `Send + Sync`.
///
/// raw-window-handle deliberately does not mark raw handles as thread-safe;
/// wgpu 30's `InstanceDescriptor::display` however requires
/// `Send + Sync`. winit exposes the same assertion via its
/// `OwnedDisplayHandle`. The claim is sound for us: the `wl_display`
/// belongs to a `wayland_client::Connection`, which is itself `Send + Sync`,
/// and wgpu only reads the pointer (at instance/adapter/surface creation,
/// always on the main thread — D-11).
#[derive(Debug, Clone)]
pub struct OwnedWaylandDisplay {
    handle: raw_window_handle::DisplayHandle<'static>,
}

// SAFETY: see the type-level comment; the pointer's owner (`Connection`) is
// Send + Sync and the pointer is only dereferenced by wgpu on this thread.
unsafe impl Send for OwnedWaylandDisplay {}
unsafe impl Sync for OwnedWaylandDisplay {}

impl raw_window_handle::HasDisplayHandle for OwnedWaylandDisplay {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(self.handle)
    }
}

/// Shared GPU context. One adapter/device for the whole process; every output
/// surface gets its own `wgpu::Surface` + [`Renderer`].
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// Adapter selection. Pass the Wayland display when live so the GLES
    /// backend can present (wgpu 30 requires it for GL on Wayland);
    /// `None` for headless.
    ///
    /// The display handle borrows the `wl_display`, so the returned `Gpu`
    /// (its `instance`) must not outlive the `Connection` — in `App` the
    /// field order (`gpu` before `conn`) enforces that.
    ///
    /// Phase-1 simplification: on multi-GPU boxes we take wgpu's default
    /// pick instead of an output-compatible one. Presenting through the
    /// Wayland WSI works from any Mesa device in practice; if the on-machine
    /// validation (dev/validate-phase1.sh) ever shows a black surface on a
    /// hybrid-GPU setup, revisit by selecting the adapter *after* the first
    /// surface exists (sctk's wgpu example does this).
    pub fn new(display: Option<raw_window_handle::DisplayHandle<'static>>) -> Result<Self, Error> {
        let display = display.map(OwnedWaylandDisplay::new);
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: display.map(|d| Box::new(d) as Box<dyn wgpu::wgt::WgpuHasDisplayHandle>),
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: None,
            ..Default::default()
        }))
        .map_err(|e| {
            format!(
                "no suitable GPU adapter found (Vulkan/GLES): {e}. On NixOS run inside the \
                 flake devshell (vulkan-loader); for software rendering install mesa's lavapipe"
            )
        })?;
        let info = adapter.get_info();
        tracing::info!(backend = ?info.backend, device = %info.name, "GPU adapter selected");
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default()))
            .map_err(|e| format!("failed to create a logical GPU device: {e}"))?;
        Ok(Self {
            instance,
            adapter,
            device,
            queue,
        })
    }
}

impl OwnedWaylandDisplay {
    pub fn new(handle: raw_window_handle::DisplayHandle<'static>) -> Self {
        Self { handle }
    }
}

/// Uniform block. `#[repr(C)]` layout is pinned by a unit test; packing into
/// bytes is manual so no extra dependency is needed (§4: keep the tree small).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Uniforms {
    res_buf: [f32; 2],
    res_log: [f32; 2],
    time_ms: f32,
    flags: u32,
    /// Audio envelope 0..1 (mixer RMS of the last callback) — the visible
    /// "wallpaper synced to music" channel of phase 2.
    level: f32,
    /// Latency-test flash 0..1 (brief bright overlay per tap).
    flash: f32,
    c00: [f32; 4],
    c10: [f32; 4],
    c01: [f32; 4],
    c11: [f32; 4],
    accent: [f32; 4],
}

impl Uniforms {
    fn to_bytes(self) -> [u8; UNIFORM_SIZE] {
        fn put<const N: usize>(buf: &mut [u8; N], off: usize, bytes: [u8; 4]) {
            buf[off..off + 4].copy_from_slice(&bytes);
        }
        let mut b = [0u8; UNIFORM_SIZE];
        for (off, v) in [
            (0usize, self.res_buf[0]),
            (4, self.res_buf[1]),
            (8, self.res_log[0]),
            (12, self.res_log[1]),
            (16, self.time_ms),
            (24, self.level),
            (28, self.flash),
        ] {
            put(&mut b, off, v.to_ne_bytes());
        }
        put(&mut b, 20, self.flags.to_ne_bytes());
        for (i, c) in [self.c00, self.c10, self.c01, self.c11, self.accent]
            .into_iter()
            .enumerate()
        {
            for (k, v) in c.iter().enumerate() {
                put(&mut b, 32 + i * 16 + k * 4, v.to_ne_bytes());
            }
        }
        b
    }
}

/// Everything one rendered frame needs.
#[derive(Debug, Clone, Copy)]
pub struct FrameParams {
    /// Logical surface size (compositor space).
    pub logical: (f32, f32),
    /// Backing buffer size (`round(logical × scale)`, D-21).
    pub buffer: (f32, f32),
    /// Animation time in ms — with a song loaded this IS the audio clock's
    /// song time (I-01); decorative wall time otherwise.
    pub time_ms: f32,
    pub testcard: bool,
    /// Audio envelope 0..1 (music-synced pulse; 0 without audio).
    pub level: f32,
    /// Latency-test flash 0..1.
    pub flash: f32,
    /// Timing degraded — wall clock in use (I-12: must be visible; phase-2
    /// stand-in is the shader's amber band until HUD text lands in phase 8).
    pub degraded: bool,
    /// Corner colors `[tl, tr, bl, br]`, already converted for the target
    /// format (sRGB floats for `*UnormSrgb`, linear otherwise).
    pub corners: [[f32; 4]; 4],
    pub accent: [f32; 4],
}

impl FrameParams {
    fn pack(&self) -> Uniforms {
        let mut flags = 0u32;
        if self.testcard {
            flags |= FLAG_TESTCARD;
        }
        if self.degraded {
            flags |= FLAG_DEGRADED;
        }
        Uniforms {
            res_buf: [self.buffer.0, self.buffer.1],
            res_log: [self.logical.0, self.logical.1],
            time_ms: self.time_ms,
            flags,
            level: self.level,
            flash: self.flash,
            c00: self.corners[0],
            c10: self.corners[1],
            c01: self.corners[2],
            c11: self.corners[3],
            accent: self.accent,
        }
    }
}

/// One pipeline + uniform binding for a specific output format.
pub struct Renderer {
    pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl Renderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("hyprbeat-phase1"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("hyprbeat uniforms"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(UNIFORM_SIZE as u64),
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("hyprbeat pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("hyprbeat gradient"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("hyprbeat uniforms"),
            size: UNIFORM_SIZE as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("hyprbeat uniform bind group"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });
        Self {
            pipeline,
            uniforms,
            bind_group,
        }
    }

    /// Record one fullscreen draw. The caller submits the encoder and (for
    /// live surfaces) presents.
    pub fn render(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        frame: &FrameParams,
    ) {
        queue.write_buffer(&self.uniforms, 0, &frame.pack().to_bytes());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("hyprbeat frame"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// Prefer an sRGB format so theme colors pass through untouched.
pub fn choose_surface_format(available: &[wgpu::TextureFormat]) -> wgpu::TextureFormat {
    for preferred in [
        wgpu::TextureFormat::Bgra8UnormSrgb,
        wgpu::TextureFormat::Rgba8UnormSrgb,
    ] {
        if available.contains(&preferred) {
            return preferred;
        }
    }
    available
        .first()
        .copied()
        .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb)
}

/// The wallpaper is opaque; letting the compositor skip blending is a small
/// but free win when the platform advertises it.
pub fn choose_alpha_mode(available: &[wgpu::CompositeAlphaMode]) -> wgpu::CompositeAlphaMode {
    if available.contains(&wgpu::CompositeAlphaMode::Opaque) {
        wgpu::CompositeAlphaMode::Opaque
    } else {
        available
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Auto)
    }
}

pub fn format_is_srgb(format: wgpu::TextureFormat) -> bool {
    matches!(
        format,
        wgpu::TextureFormat::Rgba8UnormSrgb | wgpu::TextureFormat::Bgra8UnormSrgb
    )
}

/// Convert a theme color for a target format.
///
/// For `*UnormSrgb` targets the GPU linear→sRGB-encodes on store, so the
/// shader must receive LINEAR values; for plain `*Unorm` targets no hardware
/// encoding happens, so pass sRGB-encoded values directly.
/// (Double-encoding was exactly the phase-1 bug this comment exists for.)
pub fn color_for_format(c: Color, srgb_target: bool) -> [f32; 4] {
    if srgb_target {
        c.to_rgba_linear()
    } else {
        c.to_rgba_srgb()
    }
}

/// Corner colors for a frame, driven by the pure theme animation.
pub fn corners_for_frame(t_ms: f64, srgb_target: bool) -> [[f32; 4]; 4] {
    let pal = theme::midnight();
    theme::gradient_corners(&pal, t_ms).map(|c| color_for_format(c, srgb_target))
}

pub fn accent_for_frame(srgb_target: bool) -> [f32; 4] {
    color_for_format(theme::midnight().accent, srgb_target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_layout_is_pinned() {
        assert_eq!(std::mem::size_of::<Uniforms>(), UNIFORM_SIZE);
        // field offsets per the WGSL contract in shader.wgsl
        let u = Uniforms {
            res_buf: [1.0, 2.0],
            res_log: [3.0, 4.0],
            time_ms: 5.0,
            flags: FLAG_TESTCARD | FLAG_DEGRADED,
            level: 6.0,
            flash: 7.0,
            c00: [10.0, 11.0, 12.0, 13.0],
            c10: [20.0, 21.0, 22.0, 23.0],
            c01: [30.0, 31.0, 32.0, 33.0],
            c11: [40.0, 41.0, 42.0, 43.0],
            accent: [50.0, 51.0, 52.0, 53.0],
        };
        let b = u.to_bytes();
        let f32_at = |off: usize| f32::from_ne_bytes(b[off..off + 4].try_into().unwrap());
        assert_eq!(f32_at(0), 1.0);
        assert_eq!(f32_at(12), 4.0);
        assert_eq!(f32_at(16), 5.0);
        assert_eq!(
            u32::from_ne_bytes(b[20..24].try_into().unwrap()),
            FLAG_TESTCARD | FLAG_DEGRADED
        );
        assert_eq!(f32_at(24), 6.0, "level");
        assert_eq!(f32_at(28), 7.0, "flash");
        assert_eq!(f32_at(32), 10.0, "c00");
        assert_eq!(f32_at(48), 20.0, "c10");
        assert_eq!(f32_at(64), 30.0, "c01");
        assert_eq!(f32_at(80), 40.0, "c11");
        assert_eq!(f32_at(96), 50.0, "accent");
        assert_eq!(f32_at(108), 53.0, "last channel");
    }

    #[test]
    fn frame_params_pack_flag_bits() {
        let p = FrameParams {
            logical: (1920.0, 1080.0),
            buffer: (2400.0, 1350.0),
            time_ms: 1234.0,
            testcard: true,
            level: 0.0,
            flash: 0.0,
            degraded: false,
            corners: [[0.0; 4]; 4],
            accent: [1.0; 4],
        };
        assert_eq!(p.pack().flags, FLAG_TESTCARD);
        let p = FrameParams {
            testcard: false,
            degraded: true,
            level: 0.5,
            flash: 0.75,
            ..p
        };
        assert_eq!(p.pack().flags, FLAG_DEGRADED);
        assert_eq!(p.pack().level, 0.5);
        assert_eq!(p.pack().flash, 0.75);
    }
}
