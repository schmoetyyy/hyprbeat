//! Per-output layer-shell surface state (§3.1, §8).
//!
//! One `OutputSurface` exists per `wl_output`: a background-layer
//! `zwlr_layer_surface_v1` with a wgpu surface over the same `wl_surface`,
//! a viewport for fractional-scale mapping (D-21) and — outside probe mode —
//! an empty input region so the desktop keeps receiving all pointer input
//! (I-13).

use std::time::Instant;

use smithay_client_toolkit::shell::wlr_layer::LayerSurface;
use wayland_client::protocol::{wl_output::WlOutput, wl_region::WlRegion};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

use crate::render::Renderer;

pub struct OutputSurface {
    // ------------------------------------------------------------------
    // FIELD ORDER IS LOAD-BEARING (drop order):
    // 1. the wgpu surface holds a raw pointer to the wl_surface and must be
    //    dropped BEFORE the layer surface destroys that wl_surface;
    // 2. the explicit `Drop` impl destroys viewport / fractional-scale /
    //    input-region objects before the fields drop.
    // ------------------------------------------------------------------
    pub surface: wgpu::Surface<'static>,
    pub renderer: Renderer,
    pub layer: LayerSurface,

    pub viewport: Option<WpViewport>,
    pub fractional: Option<WpFractionalScaleV1>,
    pub input_region: Option<WlRegion>,

    /// The output this surface covers.
    pub output: WlOutput,
    /// Best-effort human-readable output name (e.g. `DP-1`).
    pub name: String,

    /// Logical size in compositor space, from the layer-surface configure.
    pub logical: (u32, u32),
    /// Backing buffer size = `round(logical × scale)` (D-21).
    pub buffer: (u32, u32),
    /// Current compositor scale (fractional when wp_fractional_scale_v1 is
    /// bound, integer otherwise).
    pub scale: f64,

    /// Surface format the wgpu surface was configured with.
    pub format: wgpu::TextureFormat,
    pub alpha_mode: wgpu::CompositeAlphaMode,

    pub configured: bool,

    // frame-callback bookkeeping (§3.5)
    pub frame_cb_pending: bool,
    pub last_frame_request: Option<Instant>,
    pub last_frame_done: Option<Instant>,
    pub last_render: Option<Instant>,
    /// Whether a callback-starvation warning was already logged (occlusion is
    /// the normal cause and is *good* — free pause; don't spam).
    pub starved_logged: bool,
}

impl Drop for OutputSurface {
    fn drop(&mut self) {
        if let Some(viewport) = &self.viewport {
            viewport.destroy();
        }
        if let Some(fractional) = &self.fractional {
            fractional.destroy();
        }
        if let Some(region) = &self.input_region {
            region.destroy();
        }
        // `layer` (and with it the wl_surface) is destroyed by sctk when the
        // field drops — after the wgpu surface above.
    }
}
