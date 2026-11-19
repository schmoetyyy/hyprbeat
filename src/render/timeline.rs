//! Timeline visualizer view — the finished model + theme to quads (§10
//! row 3, D-05).
//!
//! Pure and I/O-free (I-09): [`TimelineScene`] takes the whole-map
//! [`Timeline`] built by the loader plus a [`Palette`], bakes the
//! size-independent colors once (per target format, the
//! `render::color_for_format` discipline), and maps time to x on the
//! whole-map overview axis (design-round ruling 2026-11-17: the axis
//! shows the entire map — `Timeline::frac` semantics).
//!
//! What deliberately does NOT live here:
//! - **AudioLeadIn** — applied CALLER-side only (Δ-E): the caller owns
//!   song time and passes the already-led-in `time_ms`; this module
//!   never shifts time.
//! - **The parked playhead** — the CALLER parks `time_ms` at the span
//!   start when no audio exists; the view only clamps onto the axis.
//! - **The degraded amber band** — I-12/D-35 stay the phase-2 shader
//!   flag (`render::FLAG_DEGRADED`); the timeline view never draws it.
//!
//! Layout constants are TUNABLE (accepted 2026-11-19: judged on the
//! rendered PNG, not re-litigated in review). Sizes are logical
//! (compositor) pixels unless a `_FRAC` suffix says fraction.

use crate::beatmap::timeline::{EventKind, Timeline};
use crate::config::theme::{Color, Palette};
use crate::render::color_for_format;
use crate::render::pipeline::Quad;

/// Empty space left and right of the axis, as a fraction of logical
/// width.
pub const AXIS_MARGIN_FRAC: f32 = 0.06;
/// Vertical center of the axis band, as a fraction of logical height.
pub const AXIS_CENTER_FRAC: f32 = 0.5;
/// Half the event band's height, in logical px.
pub const AXIS_HALF_HEIGHT_PX: f32 = 110.0;
/// Padding around the band background quad, in logical px.
pub const BAND_PADDING_PX: f32 = 18.0;
/// Alpha of the band background (panel color).
pub const BAND_ALPHA: f32 = 0.55;
/// Width of a circle's tick (circles are instants), in logical px.
pub const CIRCLE_TICK_PX: f32 = 4.0;
/// Minimum visible width of a slider/spinner span, in logical px.
pub const MIN_SPAN_PX: f32 = 4.0;
/// Spinner span height as a fraction of the band height.
pub const SPINNER_HEIGHT_FRAC: f32 = 0.4;
/// Playhead line width, in logical px.
pub const PLAYHEAD_WIDTH_PX: f32 = 6.0;
/// How far the playhead pokes beyond the band, per side, in logical px.
pub const PLAYHEAD_OVERSHOOT_PX: f32 = 26.0;
/// Alpha of the kiai tint quad.
pub const KIAI_ALPHA: f32 = 0.28;
/// Alpha of the event quads (slightly translucent so overlaps read).
pub const EVENT_ALPHA: f32 = 0.92;

/// Size-independent snapshot of the timeline + theme: event spans, kiai
/// windows and format-baked colors. Built once per loaded map;
/// [`TimelineScene::quads`] runs per frame.
#[derive(Debug, Clone)]
pub struct TimelineScene {
    span_start_ms: f64,
    span_end_ms: f64,
    events: Vec<(EventKind, f64, f64)>,
    kiai_ranges: Vec<(f64, f64)>,
    circle: [f32; 4],
    slider: [f32; 4],
    spinner: [f32; 4],
    kiai: [f32; 4],
    band: [f32; 4],
    playhead: [f32; 4],
}

impl TimelineScene {
    /// Bake colors for one target format (`srgb_target` — see
    /// [`crate::render::color_for_format`]); the palette itself is not
    /// kept.
    pub fn new(timeline: &Timeline, pal: &Palette, srgb_target: bool) -> Self {
        let at = |mut c: Color, alpha: f32| {
            c.a = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
            color_for_format(c, srgb_target)
        };
        Self {
            span_start_ms: timeline.span_start_ms,
            span_end_ms: timeline.span_end_ms,
            events: timeline
                .events
                .iter()
                .map(|e| (e.kind, e.start_ms, e.end_ms))
                .collect(),
            kiai_ranges: timeline.kiai_ranges.clone(),
            circle: at(pal.hit_300, EVENT_ALPHA),
            slider: at(pal.slider_track, EVENT_ALPHA),
            spinner: at(pal.hit_50, EVENT_ALPHA),
            kiai: at(pal.accent, KIAI_ALPHA),
            band: at(pal.panel, BAND_ALPHA),
            playhead: at(pal.text, 1.0),
        }
    }

    /// The frame's quads in logical px, painter-ordered: band
    /// background, kiai tints, events, playhead. `time_ms` is the
    /// already-led-in song time (caller-side AudioLeadIn, Δ-E);
    /// non-finite time parks at the span start (I-08: the view never
    /// panics, whatever the clock produced).
    pub fn quads(&self, logical: (f32, f32), time_ms: f64) -> Vec<Quad> {
        let axis_left = logical.0 * AXIS_MARGIN_FRAC;
        let axis_w = logical.0 * (1.0 - 2.0 * AXIS_MARGIN_FRAC);
        let cy = logical.1 * AXIS_CENTER_FRAC;
        let half = AXIS_HALF_HEIGHT_PX;
        // Timeline::frac semantics (clamped overview position), with a
        // non-finite guard the model does not have.
        let x = |t_ms: f64| {
            let d = self.span_end_ms - self.span_start_ms;
            let f = if d > 0.0 && t_ms.is_finite() {
                ((t_ms - self.span_start_ms) / d).clamp(0.0, 1.0)
            } else {
                0.0
            };
            axis_left + axis_w * f as f32
        };

        let mut out = Vec::with_capacity(self.events.len() + self.kiai_ranges.len() + 2);

        // band background
        out.push(Quad {
            pos: [axis_left - BAND_PADDING_PX, cy - half - BAND_PADDING_PX],
            size: [
                axis_w + 2.0 * BAND_PADDING_PX,
                2.0 * half + 2.0 * BAND_PADDING_PX,
            ],
            color: self.band,
        });

        // kiai tints, behind the events
        for &(a, b) in &self.kiai_ranges {
            out.push(Quad {
                pos: [x(a), cy - half],
                size: [x(b) - x(a), 2.0 * half],
                color: self.kiai,
            });
        }

        // events: circles are centered ticks; sliders/spinners are spans
        // (a span narrower than MIN_SPAN_PX is centered on its midpoint
        // and widened, so it stays visible)
        for &(kind, a, b) in &self.events {
            let (xa, xb) = (x(a), x(b));
            match kind {
                EventKind::Circle => out.push(Quad {
                    pos: [xa - CIRCLE_TICK_PX * 0.5, cy - half],
                    size: [CIRCLE_TICK_PX, 2.0 * half],
                    color: self.circle,
                }),
                EventKind::Slider => {
                    let w = (xb - xa).max(MIN_SPAN_PX);
                    out.push(Quad {
                        pos: [(xa + xb) * 0.5 - w * 0.5, cy - half],
                        size: [w, 2.0 * half],
                        color: self.slider,
                    });
                }
                EventKind::Spinner => {
                    let h = 2.0 * half * SPINNER_HEIGHT_FRAC;
                    let w = (xb - xa).max(MIN_SPAN_PX);
                    out.push(Quad {
                        pos: [(xa + xb) * 0.5 - w * 0.5, cy - h * 0.5],
                        size: [w, h],
                        color: self.spinner,
                    });
                }
            }
        }

        // playhead, on top, overshooting the band
        let px = x(time_ms);
        out.push(Quad {
            pos: [
                px - PLAYHEAD_WIDTH_PX * 0.5,
                cy - half - PLAYHEAD_OVERSHOOT_PX,
            ],
            size: [PLAYHEAD_WIDTH_PX, 2.0 * half + 2.0 * PLAYHEAD_OVERSHOOT_PX],
            color: self.playhead,
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beatmap::parse_osu;
    use crate::beatmap::timeline::TimelineEvent;
    use crate::config::theme::midnight;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    /// Scene from raw parts — `span` is `(span_start_ms, span_end_ms)`,
    /// which the model derives as (first start, last end).
    fn scene(
        span: (f64, f64),
        events: &[(EventKind, f64, f64)],
        kiai: &[(f64, f64)],
    ) -> TimelineScene {
        TimelineScene::new(
            &Timeline {
                span_start_ms: span.0,
                span_end_ms: span.1,
                events: events
                    .iter()
                    .map(|&(kind, a, b)| TimelineEvent {
                        kind,
                        start_ms: a,
                        end_ms: b,
                    })
                    .collect(),
                kiai_ranges: kiai.to_vec(),
            },
            &midnight(),
            false,
        )
    }

    /// HAND-COMPUTED geometry on a 1920x1080 surface (axis_left 115.2,
    /// axis_w 1689.6, cy 540, half 110): every quad's box is derived
    /// from the constants, not from the code under test.
    #[test]
    fn quads_map_times_onto_the_axis() {
        let s = scene(
            (0.0, 3000.0),
            &[
                (EventKind::Circle, 0.0, 0.0),
                (EventKind::Slider, 500.0, 1000.0),
                (EventKind::Spinner, 2000.0, 3000.0),
            ],
            &[],
        );
        let q = s.quads((1920.0, 1080.0), 1500.0);
        assert_eq!(q.len(), 1 + 3 + 1, "band + 3 events + playhead");

        let axis_left = 1920.0 * AXIS_MARGIN_FRAC;
        let axis_w = 1920.0 * (1.0 - 2.0 * AXIS_MARGIN_FRAC);
        let cy = 1080.0 * AXIS_CENTER_FRAC;
        let half = AXIS_HALF_HEIGHT_PX;

        // band background hugs the axis plus padding
        assert!(close(q[0].pos[0], axis_left - BAND_PADDING_PX));
        assert!(close(q[0].pos[1], cy - half - BAND_PADDING_PX));
        assert!(close(q[0].size[0], axis_w + 2.0 * BAND_PADDING_PX));
        assert!(close(q[0].size[1], 2.0 * half + 2.0 * BAND_PADDING_PX));

        // circle at frac 0: a centered, full-height tick
        assert!(close(q[1].pos[0], axis_left - CIRCLE_TICK_PX * 0.5));
        assert!(close(q[1].pos[1], cy - half));
        assert!(close(q[1].size[0], CIRCLE_TICK_PX));
        assert!(close(q[1].size[1], 2.0 * half));

        // slider 500..1000 of 0..3000: fracs 1/6 and 1/3
        let xa = axis_left + axis_w / 6.0;
        let xb = axis_left + axis_w / 3.0;
        assert!(close(q[2].pos[0], xa));
        assert!(close(q[2].pos[1], cy - half));
        assert!(close(q[2].size[0], xb - xa));
        assert!(close(q[2].size[1], 2.0 * half));

        // spinner: shorter, vertically centered
        let h = 2.0 * half * SPINNER_HEIGHT_FRAC;
        assert!(close(q[3].size[1], h));
        assert!(close(q[3].pos[1], cy - h * 0.5));

        // playhead at frac 0.5 (time 1500 of 0..3000), overshooting
        assert!(close(
            q[4].pos[0],
            axis_left + axis_w * 0.5 - PLAYHEAD_WIDTH_PX * 0.5
        ));
        assert!(close(q[4].pos[1], cy - half - PLAYHEAD_OVERSHOOT_PX));
        assert!(close(
            q[4].size[1],
            2.0 * half + 2.0 * PLAYHEAD_OVERSHOOT_PX
        ));
    }

    /// The playhead clamps outside the span and parks on non-finite
    /// time; the caller's park-at-span-start rule composes with this.
    #[test]
    fn playhead_clamps_and_parks() {
        let s = scene((0.0, 3000.0), &[(EventKind::Circle, 0.0, 0.0)], &[]);
        let axis_left = 1920.0 * AXIS_MARGIN_FRAC;
        let axis_right = 1920.0 * (1.0 - AXIS_MARGIN_FRAC);
        let ph = |t: f64| {
            let q = s.quads((1920.0, 1080.0), t);
            q.last().unwrap().pos[0] + PLAYHEAD_WIDTH_PX * 0.5
        };
        assert!(close(ph(-999.0), axis_left), "before the span clamps to 0");
        assert!(
            close(ph(999_999.0), axis_right),
            "past the span clamps to 1"
        );
        assert!(close(ph(f64::NAN), axis_left), "non-finite time parks");
    }

    /// Kind colors come from the palette at the baked alphas, in
    /// painter order (this is also the order pin: band, kiai, events,
    /// playhead).
    #[test]
    fn kind_colors_come_from_the_palette() {
        let pal = midnight();
        let s = scene(
            (0.0, 100.0),
            &[
                (EventKind::Circle, 0.0, 0.0),
                (EventKind::Slider, 10.0, 20.0),
                (EventKind::Spinner, 30.0, 40.0),
            ],
            &[(10.0, 20.0)],
        );
        let q = s.quads((1920.0, 1080.0), 0.0);
        // srgb_target = false => straight sRGB floats
        let expect = |c: Color, alpha: f32| {
            let mut c = c;
            c.a = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
            c.to_rgba_srgb()
        };
        assert_eq!(q.len(), 6);
        assert_eq!(q[0].color, expect(pal.panel, BAND_ALPHA));
        assert_eq!(q[1].color, expect(pal.accent, KIAI_ALPHA));
        assert_eq!(q[2].color, expect(pal.hit_300, EVENT_ALPHA));
        assert_eq!(q[3].color, expect(pal.slider_track, EVENT_ALPHA));
        assert_eq!(q[4].color, expect(pal.hit_50, EVENT_ALPHA));
        assert_eq!(q[5].color, expect(pal.text, 1.0));
    }

    /// A 1 ms span in a 10-minute map is ~0.003 px wide on the axis —
    /// MIN_SPAN_PX keeps it visible, centered on its midpoint.
    #[test]
    fn short_spans_keep_a_minimum_width() {
        let s = scene((0.0, 600_000.0), &[(EventKind::Slider, 0.0, 1.0)], &[]);
        let q = s.quads((1920.0, 1080.0), 0.0);
        let axis_left = 1920.0 * AXIS_MARGIN_FRAC;
        assert!(close(q[1].size[0], MIN_SPAN_PX));
        // the span's own sub-pixel width (~0.0028 px) shifts the center
        // by half of itself — real, but far below a visible pixel
        assert!((q[1].pos[0] - (axis_left - MIN_SPAN_PX * 0.5)).abs() < 0.01);
    }

    /// Degenerate maps stay total (the LOADER fails loud on empty maps;
    /// the view must not be the thing that breaks).
    #[test]
    fn empty_map_stays_total() {
        let s = scene((0.0, 0.0), &[], &[]);
        let q = s.quads((1920.0, 1080.0), 123.0);
        assert_eq!(q.len(), 2, "band + parked playhead only");
        assert!(close(
            q[1].pos[0] + PLAYHEAD_WIDTH_PX * 0.5,
            1920.0 * AXIS_MARGIN_FRAC
        ));
    }

    /// The real path end to end: parse the golden flagship fixture,
    /// build the timeline, bake a scene, draw at the parked playhead.
    /// Counts are pinned by beatmap::timeline's golden test (4+4+2
    /// events, one kiai window): 1 band + 1 kiai + 10 events + 1
    /// playhead.
    #[test]
    fn golden_fixture_builds_a_scene() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/golden/v14-standard.osu"
        ))
        .expect("golden fixture is committed under assets/golden");
        let m = parse_osu(&text).unwrap();
        let tl = Timeline::build(&m).unwrap();
        let scene = TimelineScene::new(&tl, &midnight(), true);
        // parked playhead: the no-audio caller passes the span start
        let quads = scene.quads((1920.0, 1080.0), tl.span_start_ms);
        assert_eq!(quads.len(), 13);
        let axis_left = 1920.0 * AXIS_MARGIN_FRAC;
        let ph = quads.last().unwrap();
        assert!(close(ph.pos[0] + PLAYHEAD_WIDTH_PX * 0.5, axis_left));
    }
}
