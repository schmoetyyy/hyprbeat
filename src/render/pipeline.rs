//! Instanced solid-quad pipeline (D-05, phase 3) — the visualizer's draw
//! path.
//!
//! One pipeline draws every colored rectangle the timeline view needs:
//! each instance carries its top-left corner, its extent and an RGBA
//! color, so theming is free per-quad (D-05: "colors as instance
//! attributes"). The texture-atlas half of D-05 lands with the gameplay
//! sprites in phase 4; this module is the geometry+color half and stays
//! atlas-free until then (§4: no machinery for an unused feature).
//!
//! Discipline inherited from `render::Uniforms` (render/mod.rs): bytes
//! are packed manually with `to_ne_bytes` so no `bytemuck` dependency
//! enters the tree (it is only a transitive wgpu dep), and the
//! `#[repr(C)]` layout is pinned by a unit test against the WGSL
//! contract in `quad.wgsl`.
//!
//! Composition: the pass LOADS the previous contents (`LoadOp::Load` —
//! the gradient pass cleared), so draw order is the caller's painter's
//! order (band, kiai, events, playhead in `render::timeline`); the
//! caller submits the encoder and presents.

use std::sync::atomic::{AtomicBool, Ordering};

/// The instanced-quad shader (§3.5), committed beside this module.
pub const QUAD_SHADER: &str = include_str!("quad.wgsl");

/// Bytes per quad instance; must match `struct Instance` in `quad.wgsl`
/// (pos vec2 at 0, size vec2 at 8, color vec4 at 16).
pub const QUAD_SIZE: usize = 32;

/// Bytes of the screen uniform (`res_log: vec2<f32>`).
pub const SCREEN_UNIFORM_SIZE: usize = 8;

/// Instance-buffer capacity. A 10-minute marathon map carries roughly
/// 4k hit objects; events + kiai bands + band + playhead still fit with
/// headroom. Anything beyond this is TRUNCATED (the first
/// `MAX_INSTANCES` quads are drawn) with one warn — the wallpaper keeps
/// rendering (I-08 posture), it never dies on a pathological map.
pub const MAX_INSTANCES: usize = 8192;

/// One colored rectangle in logical (compositor) pixels, y-down.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quad {
    /// Top-left corner.
    pub pos: [f32; 2],
    /// Width and height.
    pub size: [f32; 2],
    /// Straight (non-premultiplied) alpha; RGB already converted for
    /// the target format via [`crate::render::color_for_format`].
    pub color: [f32; 4],
}

impl Quad {
    /// Pack into the 32-byte WGSL instance layout. Manual packing in the
    /// `render::Uniforms` style (native endian — every supported target
    /// is little-endian; no bytemuck, §4).
    pub fn to_bytes(self) -> [u8; QUAD_SIZE] {
        let mut b = [0u8; QUAD_SIZE];
        for (off, v) in [
            (0usize, self.pos[0]),
            (4, self.pos[1]),
            (8, self.size[0]),
            (12, self.size[1]),
            (16, self.color[0]),
            (20, self.color[1]),
            (24, self.color[2]),
            (28, self.color[3]),
        ] {
            b[off..off + 4].copy_from_slice(&v.to_ne_bytes());
        }
        b
    }
}

/// The instanced-quad pipeline and its buffers for one output format.
/// Created once per surface beside `render::Renderer`; carries no
/// per-frame state except the warn-once truncation flag.
pub struct QuadPipeline {
    pipeline: wgpu::RenderPipeline,
    screen: wgpu::Buffer,
    instances: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// Whether the over-capacity warning already fired (once per
    /// pipeline, not per frame at 165 Hz).
    warned_truncate: AtomicBool,
}

impl QuadPipeline {
    /// Build the pipeline for a specific output format; the format must
    /// match the view later passed to [`QuadPipeline::draw`].
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("hyprbeat quad shader"),
            source: wgpu::ShaderSource::Wgsl(QUAD_SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("hyprbeat quad screen uniform"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(SCREEN_UNIFORM_SIZE as u64),
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("hyprbeat quad pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: QUAD_SIZE as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 8,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 16,
                    shader_location: 2,
                },
            ],
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("hyprbeat quad pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[Some(instance_layout)],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
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
        let screen = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("hyprbeat quad screen uniform"),
            size: SCREEN_UNIFORM_SIZE as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("hyprbeat quad instance buffer"),
            size: (QUAD_SIZE * MAX_INSTANCES) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("hyprbeat quad screen bind group"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: screen.as_entire_binding(),
            }],
        });
        Self {
            pipeline,
            screen,
            instances,
            bind_group,
            warned_truncate: AtomicBool::new(false),
        }
    }

    /// Record one instanced draw over the existing frame contents
    /// (`LoadOp::Load` — the gradient pass cleared; the caller submits
    /// the encoder and presents). `logical` is the surface size in
    /// compositor units; the quads are specified in the same space. An
    /// empty slice records nothing.
    pub fn draw(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        logical: (f32, f32),
        instances: &[Quad],
    ) {
        let n = instances.len().min(MAX_INSTANCES);
        if n == 0 {
            return;
        }
        if n < instances.len() && !self.warned_truncate.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                count = instances.len(),
                max = MAX_INSTANCES,
                "too many quads for one draw — drawing the first {n}, dropping the rest \
                 (I-08: keep rendering)"
            );
        }
        let instances = &instances[..n];

        let mut screen_bytes = [0u8; SCREEN_UNIFORM_SIZE];
        screen_bytes[0..4].copy_from_slice(&logical.0.to_ne_bytes());
        screen_bytes[4..8].copy_from_slice(&logical.1.to_ne_bytes());
        queue.write_buffer(&self.screen, 0, &screen_bytes);

        let mut bytes = Vec::with_capacity(n * QUAD_SIZE);
        for q in instances {
            bytes.extend_from_slice(&q.to_bytes());
        }
        queue.write_buffer(&self.instances, 0, &bytes);

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("hyprbeat quad pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
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
        pass.set_vertex_buffer(0, self.instances.slice(..));
        pass.draw(0..6, 0..n as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The WGSL contract: stride 32, attributes at offsets 0/8/16 —
    /// mirrors `uniform_layout_is_pinned` in render/mod.rs.
    #[test]
    fn quad_instance_layout_is_pinned() {
        assert_eq!(std::mem::size_of::<Quad>(), QUAD_SIZE);
        assert_eq!(QUAD_SIZE, 32);
        let q = Quad {
            pos: [1.0, 2.0],
            size: [3.0, 4.0],
            color: [5.0, 6.0, 7.0, 8.0],
        };
        let b = q.to_bytes();
        let f32_at = |off: usize| f32::from_ne_bytes(b[off..off + 4].try_into().unwrap());
        assert_eq!(f32_at(0), 1.0, "pos.x");
        assert_eq!(f32_at(4), 2.0, "pos.y");
        assert_eq!(f32_at(8), 3.0, "size.w");
        assert_eq!(f32_at(12), 4.0, "size.h");
        assert_eq!(f32_at(16), 5.0, "color.r");
        assert_eq!(f32_at(20), 6.0, "color.g");
        assert_eq!(f32_at(24), 7.0, "color.b");
        assert_eq!(f32_at(28), 8.0, "color.a");
    }
}
