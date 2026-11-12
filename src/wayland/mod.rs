//! Wayland client core (§3.1, §8): connection, registry, one background
//! layer-shell surface per output, and the calloop main loop (D-02/D-11).
//!
//! Phase-2 render policy (§3.5): render inside frame callbacks, throttle
//! to `--fps` (default 30) by skipping, and re-arm callbacks with a watchdog
//! so a fully occluded surface costs ~0 CPU (the "free pause"). The FIRST
//! frame after a configure is presented EAGERLY (D-26): a layer surface that
//! has never attached a buffer is never composited, so no callback would
//! ever arrive to draw it — waiting is a chicken-and-egg deadlock.
//!
//! Phase 2 (§10 row 2): the wallpaper animation is driven by the audio clock
//! (I-01) whenever a session exists — [`TimingState`] tracks which time
//! base is live, including the D-16 degraded wall clock and its I-12 amber
//! band. SIGUSR1/SIGUSR2 pause/resume the song (D-33 dev hook).

pub mod latency;
pub mod probe;
pub mod surface;

use std::time::{Duration, Instant};

use calloop::signals::{Signal, Signals};
use calloop::timer::TimeoutAction;
use calloop_wayland_source::WaylandSource;
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use sctk::compositor::{CompositorHandler, CompositorState, FrameCallbackData};
use sctk::delegate_registry;
use sctk::output::{OutputHandler, OutputState};
use sctk::registry::{ProvidesRegistryState, RegistryState, SimpleGlobal};
use sctk::registry_handlers;
use sctk::seat::{
    Capability, SeatHandler, SeatState,
    keyboard::{KeyEvent, KeyboardHandler, Keysym},
    pointer::{PointerEvent, PointerEventKind, PointerHandler},
    relative_pointer::{RelativeMotionEvent, RelativePointerHandler, RelativePointerState},
};
use sctk::shell::{
    WaylandSurface,
    wlr_layer::{
        Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
        LayerSurfaceConfigure,
    },
};
use smithay_client_toolkit as sctk;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::{self, WpViewport},
    wp_viewporter::WpViewporter,
};

use crate::Error;
use crate::audio::{self, mixer::SoundEvent};
use crate::cli::Args;
use crate::render::{self, FrameParams, Gpu, scale};
use probe::ProbeReport;
use surface::OutputSurface;

/// Layer-shell namespace. Deliberately not "hyprpaper" or anything osu-ish
/// (D-09, D-19); this is what `layerrule` targets.
const NAMESPACE: &str = "hyprbeat";
/// Watchdog tick: how often the loop wakes to check the safety timeout,
/// re-arm starved frame callbacks and log stats.
const TICK: Duration = Duration::from_millis(250);
/// If no frame callback arrives within this window, re-request one.
const CALLBACK_WATCHDOG: Duration = Duration::from_millis(250);
/// Occlusion (fullscreen window on top) legitimately stops callbacks; the
/// first overdue re-arm logs a warning, later ones stay quiet.
const STATS_INTERVAL: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Timing (phase 2: I-01 + D-16)
// ---------------------------------------------------------------------------

/// What the wallpaper's animation time comes from.
///
/// * `Audio` — the ONLY valid gameplay time base: the cpal sample counter
///   (§3.3, I-01). After a runtime stream failure the clock inside the
///   engine flips itself to its degraded wall mode and [`App::timing_degraded`]
///   turns on the amber band (I-12).
/// * `DegradedWall` — audio was wanted but could not start (`--audio` failed
///   to open, or the forced `--no-audio` validation path).
/// * `PlainWall` — no audio wanted at all: the phase-1 decorative gradient
///   (NOT degraded — nothing to degrade; the demo autoplay of later phases
///   will always carry a song).
enum TimingState {
    Audio(audio::AudioEngine),
    DegradedWall(audio::clock::Clock),
    PlainWall,
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct FrameStats {
    rendered: u64,
    skipped: u64,
    watchdog_rearms: u64,
    last_report: Option<Instant>,
}

pub struct App {
    // sctk states
    registry_state: RegistryState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    seat_state: SeatState,
    output_state: OutputState,

    // optional protocol globals (D-21: viewporter is the pixel-correct
    // fractional-scale path; both are absent on ancient compositors)
    viewporter: Option<SimpleGlobal<WpViewporter, 1>>,
    fractional_manager: Option<SimpleGlobal<WpFractionalScaleManagerV1, 1>>,
    relative_pointer_state: RelativePointerState,

    // engine — NOTE: `gpu` (its wgpu instance may hold the wl_display
    // handle) must drop BEFORE `conn`
    gpu: Gpu,
    conn: Connection,
    pub outputs: Vec<OutputSurface>,

    // session
    args: Args,
    qh: QueueHandle<App>,
    started: Instant,
    deadline: Option<Instant>,
    signal: calloop::LoopSignal,
    /// Set when an output's setup fails in a place where a trait handler
    /// cannot propagate errors (I-08: surface-level failure must not take
    /// down the whole wallpaper).
    fatal: Option<String>,
    stats: FrameStats,
    probe_report: ProbeReport,
    /// Phase 2: the animation/gameplay time base (I-01, D-16).
    timing: TimingState,
    /// `--latency-test` recorded taps (see `latency.rs`).
    latency_taps: Vec<latency::Tap>,
    /// Deadline of the current latency-test visual flash.
    flash_deadline: Option<Instant>,
    /// Song-finished notice logged exactly once.
    song_finished_logged: bool,

    // probe/latency-test-mode input objects (freed on capability loss / exit)
    probe_keyboard: Option<wl_keyboard::WlKeyboard>,
    probe_pointer: Option<wl_pointer::WlPointer>,
    probe_relative: Option<ZwpRelativePointerV1>,
}

impl App {
    /// Wallpaper animation time in ms — the audio clock when a session
    /// exists (I-01: gameplay time NEVER comes from a wall clock while one is
    /// live), the degraded wall clock when it failed, decorative wall time
    /// only in the no-audio case. Centralized so the phase-4+ game loop
    /// consumes exactly the same source.
    fn anim_ms(&self) -> f64 {
        match &self.timing {
            TimingState::Audio(engine) => engine.clock.time_ms(),
            TimingState::DegradedWall(clock) => clock.time_ms(),
            TimingState::PlainWall => self.started.elapsed().as_secs_f64() * 1000.0,
        }
    }

    /// I-12: is timing running on the degraded wall clock? (Never silent —
    /// the renderer shows the amber band.)
    fn timing_degraded(&self) -> bool {
        match &self.timing {
            TimingState::Audio(engine) => engine.clock.is_degraded(),
            TimingState::DegradedWall(_) => true,
            TimingState::PlainWall => false,
        }
    }

    fn quit(&mut self) {
        self.signal.stop();
    }

    /// Do the phase-2 diagnostic modes want full input (probe §14, or the
    /// `--latency-test` tap channel — D-28 lesson: run on an EMPTY workspace)?
    fn input_mode(&self) -> bool {
        self.args.probe || self.args.latency_test
    }

    fn index_of_surface(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.outputs
            .iter()
            .position(|s| s.layer.wl_surface() == surface)
    }

    fn index_of_layer(&self, layer: &LayerSurface) -> Option<usize> {
        self.outputs.iter().position(|s| &s.layer == layer)
    }

    fn index_of_output(&self, output: &wl_output::WlOutput) -> Option<usize> {
        self.outputs.iter().position(|s| s.output == *output)
    }

    // -- surface lifecycle ---------------------------------------------------

    fn add_output(&mut self, output: wl_output::WlOutput) {
        if self.index_of_output(&output).is_some() {
            return;
        }
        if let Some(err) = self.try_add_output(output) {
            tracing::error!(%err, "cannot set up output surface");
            self.fatal = Some(err);
            self.quit();
        }
    }

    fn try_add_output(&mut self, output: wl_output::WlOutput) -> Option<String> {
        let (name, initial_scale) = self
            .output_state
            .info(&output)
            .map(|info| {
                (
                    info.name.clone().unwrap_or_else(|| output.id().to_string()),
                    scale::clamp_scale(info.scale_factor as f64),
                )
            })
            .unwrap_or_else(|| (output.id().to_string(), 1.0));

        let wl_surface = self.compositor_state.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            wl_surface.clone(),
            Layer::Background,
            Some(NAMESPACE),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(if self.input_mode() {
            KeyboardInteractivity::OnDemand
        } else {
            KeyboardInteractivity::None
        });
        // 0×0 with all anchors = "fill the output"; the configure event tells
        // us the logical size to use.
        layer.set_size(0, 0);

        // I-13: pointer passthrough — an empty input region means desktop
        // clicks never reach the wallpaper. In probe/latency mode we
        // deliberately keep full input so Q-1/Q-2 and the taps can be observed.
        let input_region = if self.input_mode() {
            None
        } else {
            let region = self
                .compositor_state
                .wl_compositor()
                .create_region(&self.qh, ());
            // no region.add(...) — the region is empty
            layer.set_input_region(Some(&region));
            Some(region)
        };

        // Initial commit without a buffer: the compositor answers with the
        // first configure (sctk acks it for us).
        layer.commit();

        // wgpu surface over the raw wl_surface (same pattern as sctk's wgpu
        // example). SAFETY: the wl_surface outlives the wgpu surface —
        // OutputSurface's field order guarantees the drop order.
        let raw_display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            std::ptr::NonNull::new(self.conn.backend().display_ptr() as *mut _)
                .expect("wl_display pointer is null"),
        ));
        let raw_window_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            std::ptr::NonNull::new(wl_surface.id().as_ptr() as *mut _)
                .expect("wl_surface pointer is null"),
        ));
        let gpu_surface = match unsafe {
            self.gpu
                .instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(raw_display_handle),
                    raw_window_handle,
                })
        } {
            Ok(surface) => surface,
            // Surface creation cannot fail recoverably — a half-set-up layer
            // surface would map and then never draw. Report up, unwind this
            // output, let run() exit cleanly (I-08).
            Err(e) => return Some(format!("wgpu refused the Wayland surface: {e}")),
        };
        let caps = gpu_surface.get_capabilities(&self.gpu.adapter);
        let format = render::choose_surface_format(&caps.formats);
        let alpha_mode = render::choose_alpha_mode(&caps.alpha_modes);
        let renderer = render::Renderer::new(&self.gpu.device, format);

        let viewport = self
            .viewporter
            .as_ref()
            .and_then(|g| g.get().ok())
            .map(|vp| vp.get_viewport(&wl_surface, &self.qh, ()));
        let fractional = self
            .fractional_manager
            .as_ref()
            .and_then(|g| g.get().ok())
            .map(|mgr| mgr.get_fractional_scale(&wl_surface, &self.qh, wl_surface.clone()));

        tracing::info!(
            output = %name,
            initial_scale,
            has_viewporter = viewport.is_some(),
            has_fractional_scale = fractional.is_some(),
            format = ?format,
            "background layer surface created"
        );
        self.outputs.push(OutputSurface {
            surface: gpu_surface,
            renderer,
            layer,
            viewport,
            fractional,
            input_region,
            output,
            name,
            logical: (0, 0),
            buffer: (0, 0),
            scale: initial_scale,
            format,
            alpha_mode,
            configured: false,
            frame_cb_pending: false,
            last_frame_request: None,
            last_frame_done: None,
            last_render: None,
            starved_logged: false,
        });
        None
    }

    fn remove_output_surface(&mut self, idx: usize) {
        let s = self.outputs.remove(idx);
        tracing::info!(output = %s.name, "surface gone");
        if self.outputs.is_empty() {
            tracing::warn!("last output disappeared — exiting");
            self.quit();
        }
    }

    /// (Re)configure the wgpu surface + viewport for the current logical
    /// size × scale. Idempotent: no-op when nothing changed (D-21).
    fn resize_surface(&mut self, idx: usize, logical: (u32, u32)) {
        let (changed, buffer) = {
            let s = &mut self.outputs[idx];
            let buffer = scale::buffer_size(logical, s.scale);
            let changed = !s.configured || s.logical != logical || s.buffer != buffer;
            s.logical = logical;
            s.buffer = buffer;
            (changed, buffer)
        };
        if !changed {
            return;
        }
        let device = &self.gpu.device;
        let s = &mut self.outputs[idx];
        s.surface.configure(
            device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: s.format,
                color_space: wgpu::SurfaceColorSpace::Auto,
                alpha_mode: s.alpha_mode,
                width: s.buffer.0,
                height: s.buffer.1,
                // sctk's wgpu example: Wayland present is inherently mailbox
                present_mode: wgpu::PresentMode::Mailbox,
                desired_maximum_frame_latency: 2,
                view_formats: vec![s.format],
            },
        );
        if let Some(viewport) = &s.viewport {
            // map the buffer 1:1 onto the logical rectangle — this is what
            // makes fractional scales pixel-correct (D-21)
            viewport.set_source(0.0, 0.0, buffer.0 as f64, buffer.1 as f64);
            viewport.set_destination(logical.0 as i32, logical.1 as i32);
        } else {
            // integer fallback (may be off by <1px on fractional outputs)
            let integer = s.scale.round().max(1.0) as u32;
            let _ = s.layer.set_buffer_scale(integer);
        }
        s.layer.commit();
        s.configured = true;
        tracing::info!(
            output = %s.name,
            logical = ?logical,
            scale = s.scale,
            buffer = ?s.buffer,
            "surface resized"
        );
    }

    fn request_frame(&mut self, idx: usize) {
        let s = &mut self.outputs[idx];
        if s.frame_cb_pending {
            return;
        }
        s.layer
            .wl_surface()
            .frame(&self.qh, FrameCallbackData(s.layer.wl_surface().clone()));
        s.frame_cb_pending = true;
        s.last_frame_request = Some(Instant::now());
    }

    // -- frame loop ------------------------------------------------------------

    fn on_frame_done(&mut self, idx: usize) {
        let now = Instant::now();
        {
            let s = &mut self.outputs[idx];
            s.frame_cb_pending = false;
            s.last_frame_done = Some(now);
            if s.starved_logged {
                s.starved_logged = false;
                tracing::debug!(output = %s.name, "frame callbacks resumed");
            }
        }

        let due = {
            let s = &self.outputs[idx];
            s.configured
                && scale::should_render(
                    s.last_render.map(|t| t.elapsed().as_secs_f64() * 1000.0),
                    self.args.frame_interval_ms(),
                )
        };

        // Request the next callback BEFORE any commit this turn: the Wayland
        // frame callback is delivered after the commit that follows the
        // request (and wgpu 30's Queue::present attaches + commits), so the
        // callback and the commit must be paired in this order.
        self.request_frame(idx);

        if due {
            if self.render_output(idx) {
                self.outputs[idx].last_render = Some(now);
                self.stats.rendered += 1;
            }
        } else {
            // Throttled skip. Some compositors only fire frame callbacks
            // after a commit; an empty one keeps the loop alive everywhere
            // at ~zero cost. (Q-3: measure on Hyprland whether this is
            // actually needed — §14.)
            self.outputs[idx].layer.wl_surface().commit();
            self.stats.skipped += 1;
        }
    }

    fn render_output(&mut self, idx: usize) -> bool {
        // Phase 2: the frame time IS the audio clock when a song is loaded
        // (I-01) — pause/resume of the audio therefore freezes/resumes the
        // animation with sample accuracy. The music envelope (RMS leaving
        // the DAC) modulates the gradient — the visible "wallpaper synced to
        // music" exit criterion.
        let (anim_ms, level, flash, degraded) = self.frame_time_inputs();
        let (logical, buffer, srgb, testcard) = {
            let s = &self.outputs[idx];
            (
                s.logical,
                s.buffer,
                render::format_is_srgb(s.format),
                self.args.testcard,
            )
        };
        // The shader consumes time as f32; a raw 24/7 millisecond counter
        // would visibly step after ~4.6 h (2^24 ms). All phase-1/2 animations
        // have periods dividing 8000 ms, so wrap first — the wallpaper is
        // meant to run for days.
        let anim_ms = anim_ms.rem_euclid(crate::config::theme::GRADIENT_PERIOD_MS);
        let frame = FrameParams {
            logical: (logical.0 as f32, logical.1 as f32),
            buffer: (buffer.0 as f32, buffer.1 as f32),
            time_ms: anim_ms as f32,
            testcard,
            level,
            flash,
            degraded,
            corners: render::corners_for_frame(anim_ms, srgb),
            accent: render::accent_for_frame(srgb),
        };

        let acquired = self.outputs[idx].surface.get_current_texture();
        let texture = match acquired {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                // frame pacing hiccup or occluded — just skip this one
                return false;
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                // resize race or lost surface: reconfigure once and let the
                // next callback retry
                tracing::warn!(
                    output = %self.outputs[idx].name,
                    "surface outdated/lost, reconfiguring"
                );
                let logical = self.outputs[idx].logical;
                self.resize_surface(idx, logical);
                return false;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                tracing::error!(
                    output = %self.outputs[idx].name,
                    "surface validation error — skipping frame"
                );
                return false;
            }
        };
        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("hyprbeat frame"),
            });
        self.outputs[idx]
            .renderer
            .render(&self.gpu.queue, &mut encoder, &view, &frame);
        self.gpu.queue.submit([encoder.finish()]);
        // wgpu 30: present lives on Queue — it attaches the wl_buffer and
        // commits the surface (so viewport/frame-callback state committed
        // earlier rides along).
        self.gpu.queue.present(texture);
        // `--latency-test`: stamp the first frame submitted after the tap
        // (the flash rides on this very frame — photon leg of the report).
        if self.args.latency_test && self.flash_deadline.is_some() {
            if let (Some(tap), TimingState::Audio(engine)) =
                (self.latency_taps.last_mut(), &self.timing)
            {
                if tap.frame_ns.is_none() {
                    tap.frame_ns = Some(
                        engine
                            .mixer
                            .origin
                            .elapsed()
                            .as_nanos()
                            .min(u64::MAX as u128) as u64,
                    );
                }
            }
        }
        true
    }

    /// Per-frame timing inputs: `(anim_ms, envelope, flash, degraded)`.
    fn frame_time_inputs(&self) -> (f64, f32, f32, bool) {
        let anim_ms = self.anim_ms();
        let level = match &self.timing {
            TimingState::Audio(engine) => engine.mixer.envelope(),
            _ => 0.0,
        };
        let flash = self
            .flash_deadline
            .map(|until| {
                (until
                    .saturating_duration_since(Instant::now())
                    .as_secs_f64()
                    * 1000.0
                    / latency::FLASH_MS)
                    .clamp(0.0, 1.0)
            })
            .unwrap_or(0.0) as f32;
        (anim_ms, level, flash, self.timing_degraded())
    }

    // -- periodic maintenance ------------------------------------------------

    fn on_tick(&mut self) {
        let now = Instant::now();

        // --timeout safety (D-24)
        if let Some(deadline) = self.deadline {
            if now >= deadline {
                tracing::warn!(
                    timeout_s = ?self.args.timeout.map(|d| d.as_secs_f64()),
                    "safety timeout reached, exiting (--timeout 0 disables; phase 7 retires the default)"
                );
                self.quit();
                return;
            }
        }

        // phase 2: runtime audio loss → degrade the clock, keep the
        // wallpaper alive (D-16/I-08). The RT error callback only stored a
        // flag; THIS is the safe context that acts on it.
        if let TimingState::Audio(engine) = &mut self.timing {
            if engine.stream_failed() && !engine.clock.is_degraded() {
                tracing::error!(
                    "audio stream failed at runtime — degrading to the wall clock \
                     (timing now unranked; the amber degraded band turns on, I-12)"
                );
                engine.clock.degrade_to_wall();
            }
        }

        // frame-callback watchdog
        for idx in 0..self.outputs.len() {
            let overdue = self.outputs[idx]
                .last_frame_request
                .is_some_and(|t| now.duration_since(t) > CALLBACK_WATCHDOG)
                || (self.outputs[idx].configured && self.outputs[idx].last_frame_request.is_none());
            if self.outputs[idx].configured && overdue {
                if !self.outputs[idx].starved_logged {
                    tracing::warn!(
                        output = %self.outputs[idx].name,
                        overdue = ?CALLBACK_WATCHDOG,
                        "no frame callbacks — surface occluded or compositor throttling; \
                         re-arming (occlusion is the free pause, §3.5)"
                    );
                    self.outputs[idx].starved_logged = true;
                }
                self.stats.watchdog_rearms += 1;
                if self.outputs[idx].last_render.is_none() {
                    // First-frame deadlock fallback (D-26): no buffer was ever
                    // presented, so no callback can ever arrive. Present one;
                    // the present-commit also flushes the pending request.
                    self.request_frame(idx);
                    if self.render_output(idx) {
                        self.outputs[idx].last_render = Some(now);
                        self.stats.rendered += 1;
                    }
                } else {
                    // Rendered before but starved: either occlusion (the free
                    // pause — must stay ~0 cost, so NO buffer present here) or
                    // a callback that was armed but never flushed. Re-arm
                    // without forcing a SECOND concurrent callback: each
                    // on_frame_done begets exactly one new request, so double
                    // arming persists forever once triggered.
                    if self.outputs[idx].frame_cb_pending {
                        self.outputs[idx].layer.wl_surface().commit();
                    } else {
                        self.request_frame(idx);
                        self.outputs[idx].layer.wl_surface().commit();
                    }
                }
            }
        }

        // stats
        let should_report = self
            .stats
            .last_report
            .is_none_or(|t| now.duration_since(t) > STATS_INTERVAL);
        if should_report {
            // window start: the previous report, or loop start for the first
            // window — avoids the "2 frames / 1e-6 s = 2 000 000 fps" artifact
            let window_start = self.stats.last_report.unwrap_or(self.started);
            let elapsed = now.duration_since(window_start).as_secs_f64().max(1e-6);
            tracing::info!(
                rendered = self.stats.rendered,
                skipped = self.stats.skipped,
                avg_render_fps = (self.stats.rendered as f64 / elapsed),
                watchdog_rearms = self.stats.watchdog_rearms,
                outputs = self.outputs.len(),
                "frame loop"
            );
            // phase 2: the audio line the validation script greps — song time
            // from the clock (I-01), envelope, pause state, RT diagnostics.
            if let TimingState::Audio(engine) = &self.timing {
                let (events_dropped, voices_dropped, clipped) = engine.mixer.diagnostics();
                tracing::info!(
                    song_ms = engine.clock.time_ms() as i64,
                    envelope = engine.mixer.envelope(),
                    paused = engine.clock.is_paused(),
                    degraded = engine.clock.is_degraded(),
                    rate = engine.clock.rate(),
                    frames_per_callback = engine.mixer.frames_per_callback(),
                    events_dropped,
                    voices_dropped,
                    clipped,
                    "audio clock"
                );
                if engine.mixer.finished() && !self.song_finished_logged {
                    self.song_finished_logged = true;
                    tracing::info!(
                        song_ms = engine.clock.time_ms() as i64,
                        "music finished (silence from here; auto-advance arrives in phase 7)"
                    );
                }
            }
            self.stats.rendered = 0;
            self.stats.skipped = 0;
            self.stats.watchdog_rearms = 0;
            self.stats.last_report = Some(now);
        }
    }

    // -- phase 2: audio pause/rebase (§3.3) --------------------------------

    /// SIGUSR1 (D-33): freeze the song. Audio mode stops the mapping (the
    /// device counter keeps flowing — §3.3: "audio device time never
    /// rewinds; the mapping does").
    fn on_pause_signal(&mut self) {
        match &mut self.timing {
            TimingState::Audio(engine) => {
                let song_ms = engine.clock.time_ms();
                engine.clock.pause();
                tracing::warn!(
                    song_ms,
                    "SIGUSR1: paused — clock frozen, mixer silenced, device counter still flowing"
                );
            }
            TimingState::DegradedWall(clock) => {
                clock.pause();
                tracing::warn!("SIGUSR1: paused (degraded wall clock)");
            }
            TimingState::PlainWall => {
                tracing::warn!("SIGUSR1: nothing to pause (no audio session in this run)");
            }
        }
    }

    /// SIGUSR2 (D-33): resume — rebase `song_start` to the current counter so
    /// song time continues exactly at the pause point (§3.3).
    fn on_resume_signal(&mut self) {
        match &mut self.timing {
            TimingState::Audio(engine) => {
                let before = engine.clock.time_ms();
                engine.clock.resume();
                tracing::warn!(
                    song_ms = engine.clock.time_ms() as i64,
                    delta_ms = engine.clock.time_ms() - before,
                    "SIGUSR2: resumed — song_start rebased onto the current counter"
                );
            }
            TimingState::DegradedWall(clock) => {
                clock.resume();
                tracing::warn!("SIGUSR2: resumed (degraded wall clock)");
            }
            TimingState::PlainWall => {
                tracing::warn!("SIGUSR2: nothing to resume (no audio session in this run)");
            }
        }
    }

    // -- phase 2: latency taps ----------------------------------------------

    /// `--latency-test` key tap: fire a hitsound at the audio counter + a
    /// visual flash, and record the enqueue stamp (§3.6 instrument).
    fn on_latency_tap(&mut self) {
        let TimingState::Audio(engine) = &self.timing else {
            tracing::warn!(
                "latency tap ignored — no live audio stream (degraded or plain mode); \
                 the report will note the missing sound leg"
            );
            return;
        };
        let tag = latency::next_tag(self.latency_taps.len());
        let enqueue_ns = engine.mixer.origin.elapsed().as_nanos() as u64;
        let due = engine.clock.counter();
        let queued = engine.mixer.fire(SoundEvent {
            due,
            sound: 0,
            gain: 256, // 1.0
            tag,
        });
        self.latency_taps
            .push(latency::Tap::new(due, tag, enqueue_ns));
        self.flash_deadline =
            Some(Instant::now() + Duration::from_millis(latency::FLASH_MS as u64));
        tracing::info!(
            n = self.latency_taps.len(),
            queued,
            due,
            "latency tap: hitsound scheduled + flash armed"
        );
    }

    fn on_preferred_scale(&mut self, surface: &wl_surface::WlSurface, units: u32) {
        let factor = scale::clamp_scale(scale::factor_from_fractional_units(units));
        let Some(idx) = self.index_of_surface(surface) else {
            return;
        };
        let s = &mut self.outputs[idx];
        if (s.scale - factor).abs() < f64::EPSILON {
            return;
        }
        tracing::info!(output = %s.name, factor, raw_units = units, "fractional scale preferred");
        s.scale = factor;
        let logical = s.logical;
        let configured = s.configured;
        if configured {
            self.resize_surface(idx, logical);
        }
    }
}

// ---------------------------------------------------------------------------
// sctk handler traits
// ---------------------------------------------------------------------------

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        // integer preferred-buffer-scale; only used when the fractional-scale
        // protocol is not bound (D-21 fallback)
        let Some(idx) = self.index_of_surface(surface) else {
            return;
        };
        if self.outputs[idx].fractional.is_some() {
            return;
        }
        let factor = scale::clamp_scale(new_factor as f64);
        let s = &mut self.outputs[idx];
        if (s.scale - factor).abs() < f64::EPSILON {
            return;
        }
        tracing::info!(output = %s.name, factor, "integer scale preferred");
        s.scale = factor;
        let logical = s.logical;
        let configured = s.configured;
        if configured {
            self.resize_surface(idx, logical);
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
        // phase-1 gradient is orientation-agnostic
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        if let Some(idx) = self.index_of_surface(surface) {
            self.on_frame_done(idx);
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        if let Some(idx) = self.index_of_surface(surface) {
            tracing::debug!(output = %self.outputs[idx].name, entered = %output.id(), "surface entered output");
        }
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.add_output(output);
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // refresh integer scale when we have no fractional-scale object
        if let Some(idx) = self.index_of_output(&output) {
            if self.outputs[idx].fractional.is_none() {
                if let Some(info) = self.output_state.info(&output) {
                    let factor = scale::clamp_scale(info.scale_factor as f64);
                    if (self.outputs[idx].scale - factor).abs() > f64::EPSILON {
                        tracing::info!(output = %self.outputs[idx].name, factor, "output scale changed");
                        self.outputs[idx].scale = factor;
                        let logical = self.outputs[idx].logical;
                        if self.outputs[idx].configured {
                            self.resize_surface(idx, logical);
                        }
                    }
                }
            }
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(idx) = self.index_of_output(&output) {
            self.remove_output_surface(idx);
        }
    }
}

impl LayerShellHandler for App {
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(idx) = self.index_of_layer(layer) else {
            return;
        };
        let (w, h) = configure.new_size;
        let logical = if w > 0 && h > 0 {
            (w, h)
        } else {
            // compositor left the size to us (shouldn't happen with all
            // anchors + size 0,0, but never map a 0-sized buffer)
            self.output_state
                .info(&self.outputs[idx].output)
                .and_then(|info| info.logical_size)
                .map(|(w, h)| (w.max(1) as u32, h.max(1) as u32))
                .unwrap_or((1920, 1080))
        };
        self.resize_surface(idx, logical);
        self.request_frame(idx);
        // D-26: the FIRST frame must be presented eagerly. Hyprland does not
        // composite (and so never sends frame callbacks to) a layer surface
        // that has never attached a buffer — waiting for the first callback
        // to draw frame 1 is a chicken-and-egg deadlock (live-observed on the
        // dev machine: rendered=0, watchdog_rearms=20 for 10 s). The present
        // below is also the commit that flushes the callback request armed
        // above — request MUST precede commit (Wayland frame-callback rule).
        if self.render_output(idx) {
            self.outputs[idx].last_render = Some(Instant::now());
            self.stats.rendered += 1;
        }
    }

    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        if let Some(idx) = self.index_of_layer(layer) {
            self.remove_output_surface(idx);
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        // Input handling is phase 4; the probe and the `--latency-test`
        // taps are the phase-1/2 consumers.
        if !self.input_mode() {
            return;
        }
        match capability {
            Capability::Keyboard if self.probe_keyboard.is_none() => {
                match self.seat_state.get_keyboard(_qh, &seat, None) {
                    Ok(kbd) => {
                        tracing::info!("probe: keyboard bound");
                        self.probe_keyboard = Some(kbd);
                    }
                    Err(err) => tracing::warn!(?err, "probe: keyboard bind failed"),
                }
            }
            Capability::Pointer if self.probe_pointer.is_none() => {
                match self.seat_state.get_pointer(_qh, &seat) {
                    Ok(pointer) => {
                        self.probe_pointer = Some(pointer.clone());
                        match self
                            .relative_pointer_state
                            .get_relative_pointer(&pointer, _qh)
                        {
                            Ok(rel) => {
                                tracing::info!("probe: relative pointer created (manager present)");
                                self.probe_relative = Some(rel);
                            }
                            Err(err) => {
                                tracing::warn!(
                                    ?err,
                                    "probe: zwp_relative_pointer_manager_v1 unavailable — Q-2 FAIL"
                                );
                            }
                        }
                    }
                    Err(err) => tracing::warn!(?err, "probe: pointer bind failed"),
                }
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard => {
                if let Some(kbd) = self.probe_keyboard.take() {
                    kbd.release();
                }
            }
            Capability::Pointer => {
                if let Some(pointer) = self.probe_pointer.take() {
                    pointer.release();
                }
                if let Some(rel) = self.probe_relative.take() {
                    rel.destroy();
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw_keys: &[u32],
        _keysyms: &[Keysym],
    ) {
        if self.args.probe && self.index_of_surface(surface).is_some() {
            if !self.probe_report.keyboard_focus_gained {
                tracing::info!("probe: Q-1 keyboard focus GAINED (on_demand works here)");
            }
            self.probe_report.keyboard_focus_gained = true;
        }
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
        if self.args.probe && self.index_of_surface(surface).is_some() {
            if !self.probe_report.keyboard_focus_lost {
                tracing::info!(
                    "probe: keyboard focus LOST (compositor moved focus away — note the time vs your actions)"
                );
            }
            self.probe_report.keyboard_focus_lost = true;
        }
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        if self.args.probe {
            if self.probe_report.key_events == 0 {
                tracing::info!(keysym = ?event.keysym, "probe: FIRST key press delivered");
            }
            self.probe_report.key_events += 1;
            tracing::debug!(keysym = ?event.keysym, "probe: key press");
        }
        if self.args.latency_test {
            self.on_latency_tap();
        }
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: sctk::seat::keyboard::Modifiers,
        _raw_modifiers: sctk::seat::keyboard::RawModifiers,
        _layout: u32,
    ) {
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        if !self.args.probe {
            return;
        }
        for event in events {
            if self.index_of_surface(&event.surface).is_none() {
                continue;
            }
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    if !self.probe_report.pointer_entered {
                        tracing::info!("probe: pointer entered the background surface");
                    }
                    self.probe_report.pointer_entered = true;
                }
                PointerEventKind::Press { button, .. } => {
                    if self.probe_report.pointer_buttons == 0 {
                        tracing::info!(
                            button,
                            "probe: FIRST pointer click delivered to the surface"
                        );
                    }
                    self.probe_report.pointer_buttons += 1;
                }
                _ => {}
            }
        }
    }
}

impl RelativePointerHandler for App {
    fn relative_pointer_motion(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _relative_pointer: &ZwpRelativePointerV1,
        _pointer: &wl_pointer::WlPointer,
        event: RelativeMotionEvent,
    ) {
        if self.args.probe {
            if self.probe_report.relative_motion_events == 0 {
                tracing::info!("probe: Q-2 first relative motion delta received");
            }
            self.probe_report.relative_motion_events += 1;
            self.probe_report.last_relative_delta = event.delta;
        }
    }
}

// -- raw protocol dispatch (protocols sctk does not wrap) -------------------

impl Dispatch<WpViewport, ()> for App {
    fn event(
        _state: &mut App,
        _viewport: &WpViewport,
        _event: wp_viewport::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        // wp_viewport has no events in v1
    }
}

impl Dispatch<wl_region::WlRegion, ()> for App {
    fn event(
        _state: &mut App,
        _region: &wl_region::WlRegion,
        _event: wl_region::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        // wl_region has no events
    }
}

impl Dispatch<WpFractionalScaleV1, wl_surface::WlSurface> for App {
    fn event(
        state: &mut App,
        _fractional: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        surface: &wl_surface::WlSurface,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.on_preferred_scale(surface, scale);
        }
    }
}

// -- sctk delegation ---------------------------------------------------------

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

impl AsMut<SimpleGlobal<WpViewporter, 1>> for App {
    fn as_mut(&mut self) -> &mut SimpleGlobal<WpViewporter, 1> {
        self.viewporter
            .as_mut()
            .expect("viewporter AsMut is only reachable when the global was bound")
    }
}

impl AsMut<SimpleGlobal<WpFractionalScaleManagerV1, 1>> for App {
    fn as_mut(&mut self) -> &mut SimpleGlobal<WpFractionalScaleManagerV1, 1> {
        self.fractional_manager
            .as_mut()
            .expect("fractional manager AsMut is only reachable when the global was bound")
    }
}

wayland_client::delegate_noop!(App: WpViewporter);
wayland_client::delegate_noop!(App: WpFractionalScaleManagerV1);

smithay_client_toolkit::delegate_dispatch2!(App);

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(args: Args) -> Result<(), Error> {
    let conn = Connection::connect_to_env()
        .map_err(|e| format!("cannot connect to a Wayland server ({e}) — run hyprbeat inside Hyprland/sway/niri, or use --headless"))?;

    // The wgpu instance wants the Wayland display for the GLES backend
    // (required for GL presentation on Wayland in wgpu 30).
    let raw_display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
        std::ptr::NonNull::new(conn.backend().display_ptr() as *mut _)
            .expect("wl_display pointer is null"),
    ));
    let display: raw_window_handle::DisplayHandle<'static> = unsafe {
        // SAFETY: the wl_display pointer is owned by `conn`, which outlives
        // the whole session (and the Gpu instance — see App field order).
        raw_window_handle::DisplayHandle::borrow_raw(raw_display_handle)
    };

    // GPU early: fail fast with a clear message if there is no usable adapter.
    let gpu = render::Gpu::new(Some(display))?;

    let (globals, mut event_queue) = registry_queue_init::<App>(&conn)
        .map_err(|e| format!("cannot enumerate Wayland globals: {e}"))?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)
        .map_err(|_| "wl_compositor is not available".to_string())?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|_| "zwlr_layer_shell_v1 is not available — hyprbeat needs Hyprland, sway or another wlr-layer-shell compositor".to_string())?;
    let viewporter = SimpleGlobal::<WpViewporter, 1>::bind(&globals, &qh).ok();
    let fractional_manager =
        SimpleGlobal::<WpFractionalScaleManagerV1, 1>::bind(&globals, &qh).ok();
    if viewporter.is_none() || fractional_manager.is_none() {
        tracing::warn!(
            viewporter = viewporter.is_some(),
            fractional_scale = fractional_manager.is_some(),
            "optional scaling protocol(s) missing — fractional outputs will be slightly blurry"
        );
    }

    let mut event_loop = calloop::EventLoop::<App>::try_new()?;
    let started = Instant::now();

    // D-33 dev hook: SIGUSR1 = pause, SIGUSR2 = resume (§3.3 rebase). The
    // real pause paths arrive with focus-loss/mode switching in phase 7;
    // until then signals give `dev/validate-phase2.sh` a scriptable channel.
    //
    // ORDER IS LOAD-BEARING: calloop's signal source BLOCKS these signals on
    // this thread via signalfd, and threads spawned afterwards inherit the
    // mask. The cpal callback thread (spawned by `build_timing` below) must
    // already block SIGUSR1/2 — otherwise a `kill -USR1` could land on the
    // unmasked audio thread and take the default terminate action, killing
    // the wallpaper. Registration failure is non-fatal — only the validation
    // convenience is lost (I-08 posture).
    match Signals::new(&[Signal::SIGUSR1, Signal::SIGUSR2]) {
        Ok(signals) => {
            let _signal_token = event_loop
                .handle()
                .insert_source(signals, |info, _metadata, app: &mut App| {
                    match info.signal() {
                        Signal::SIGUSR1 => app.on_pause_signal(),
                        Signal::SIGUSR2 => app.on_resume_signal(),
                        _ => {}
                    }
                })
                .map_err(|e| format!("cannot register signal handlers: {e}"));
        }
        Err(err) => {
            tracing::warn!(%err, "cannot install SIGUSR1/2 handlers — pause validation needs them");
        }
    }

    // -- phase 2: the audio session / time base (I-01, D-16) ------------------
    // Started after the signal mask (see the comment above) and before the
    // loop so the first frame already draws on audio clock time. Failures
    // degrade, never abort (a wallpaper must not die with PipeWire).
    let timing = build_timing(&args);
    let timing_ok = matches!(timing, TimingState::Audio(ref e) if !e.clock.is_degraded());

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        compositor_state,
        layer_shell,
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        viewporter,
        fractional_manager,
        relative_pointer_state: RelativePointerState::bind(&globals, &qh),
        gpu,
        conn: conn.clone(),
        outputs: Vec::new(),
        args: args.clone(),
        qh: qh.clone(),
        started,
        deadline: args.timeout.map(|d| started + d),
        signal: event_loop.get_signal(),
        fatal: None,
        stats: FrameStats::default(),
        probe_report: ProbeReport::default(),
        timing,
        latency_taps: Vec::new(),
        flash_deadline: None,
        song_finished_logged: false,
        probe_keyboard: None,
        probe_pointer: None,
        probe_relative: None,
    };

    if app.args.probe {
        probe::print_instructions(&app.args);
    }
    if app.args.latency_test {
        latency::print_instructions(app.args.timeout.map(|d| d.as_secs_f64()));
    }
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        outputs_pending = true,
        testcard = app.args.testcard,
        idle_fps = app.args.idle_fps,
        timeout_s = app.args.timeout.map(|d| d.as_secs_f64()),
        probe = app.args.probe,
        audio_timing = timing_ok,
        latency_test = app.args.latency_test,
        "hyprbeat starting (phase 2: audio clock + music-synced wallpaper)"
    );

    // First roundtrip: output geometry events arrive, OutputHandler fires,
    // one layer surface per output gets created and committed.
    event_queue
        .roundtrip(&mut app)
        .map_err(|e| format!("initial Wayland roundtrip failed: {e}"))?;

    // Wayland dispatch (auto: queue.dispatch_pending into App on readiness)
    let _wayland_token = WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| format!("cannot integrate Wayland into the event loop: {e}"))?;

    // 250 ms maintenance tick: safety timeout, callback watchdog, stats,
    // audio-failure degrade check.
    let timer = calloop::timer::Timer::from_duration(TICK);
    let _timer_token = event_loop
        .handle()
        .insert_source(timer, |_deadline, _metadata, app: &mut App| {
            app.on_tick();
            TimeoutAction::ToDuration(TICK)
        })
        .map_err(|e| format!("cannot register the maintenance timer: {e}"))?;

    // Blocks until app.signal.stop() (timeout / last output gone) or a
    // dispatch error (compositor died).
    event_loop
        .run(None::<Duration>, &mut app, |_| {})
        .map_err(|e| format!("event loop failed: {e}"))?;

    if let Some(fatal) = app.fatal.take() {
        return Err(fatal.into());
    }

    if app.args.probe {
        app.probe_report.print_summary();
    }
    // `--latency-test` exit report (§3.6 budget check).
    if app.args.latency_test {
        match &app.timing {
            TimingState::Audio(engine) => {
                let period_ms = engine.mixer.frames_per_callback() as f64
                    / engine.clock.rate().max(1) as f64
                    * 1000.0;
                latency::LatencyStats::summarize_with_period(
                    &app.latency_taps,
                    &engine.mixer,
                    period_ms,
                )
                .print(period_ms);
            }
            _ => {
                println!(
                    "\nlatency test: no live audio stream — the sound leg was not measurable (degraded run)"
                );
            }
        }
    }
    // Explicit ordering: wgpu surfaces must die before their wl_surfaces.
    // The audio engine (and its cpal Stream) drops with `app` here too.
    app.outputs.clear();
    tracing::info!("hyprbeat stopped cleanly");
    Ok(())
}

/// Build the phase-2 time base from CLI intent (I-01, D-16).
fn build_timing(args: &Args) -> TimingState {
    let wants_audio = args.audio.is_some() || args.latency_test;
    if wants_audio {
        match audio::AudioEngine::start(audio::StartOptions {
            music: args.audio.as_deref(),
            volume: args.volume,
            offset_ms: args.offset_ms,
            lead_in_ms: 0.0,
            buffer_frames: args.buffer_frames,
        }) {
            Ok(engine) => TimingState::Audio(engine),
            Err(err) => {
                tracing::error!(
                    %err,
                    "cannot start the audio engine — degrading to the wall clock \
                     (D-16: timing is unranked and the amber degraded band is ON)"
                );
                TimingState::DegradedWall(audio::clock::Clock::new_wall(args.offset_ms))
            }
        }
    } else if args.no_audio {
        tracing::warn!("--no-audio: forced degraded wall-clock path (D-16 validation instrument)");
        TimingState::DegradedWall(audio::clock::Clock::new_wall(args.offset_ms))
    } else {
        TimingState::PlainWall
    }
}
