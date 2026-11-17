//! Whole-map timeline model — the §10 row-3 visualizer's pure core.
//!
//! `Timeline::build` turns a parsed [`Beatmap`] into what the visualizer
//! draws: every hit object as one span on a single whole-map axis
//! (circles are instants, sliders span their computed duration,
//! spinners span to `endTime`), plus the kiai windows from the timing
//! points. Pure and I/O-free (I-09) — the renderer (visualizer commit
//! b) only reads the finished model.
//!
//! ## Slider duration — the CORRECTED §7.3 formula (user ruling 2026-11-17)
//!
//! §7.3 as written in CONTEXT.md reads:
//!
//! > **Length**: slider duration = `length / (SliderMultiplier · SV) ·
//! > beatLength` with SV from the active inherited timing point.
//!
//! That omits the ×100 unit conversion. §5.1 defines `SliderMultiplier`
//! as the base slider velocity in **hundreds of osu-px per beat**
//! ("base slider velocity, 100 osu-px/beat"), so the travel velocity is
//! `SliderMultiplier × 100 × SV` osu-px per beat and ONE slide (span) of
//! a slider lasts
//!
//! ```text
//! span_ms = length / (SliderMultiplier × 100 × SV) × beatLength
//! ```
//!
//! with `SV = -100 / inherited beatLength` (§5.1;
//! [`TimingPoint::sv_multiplier`]) and `beatLength` = ms per beat of the
//! active uninherited point; the whole slider lasts `span_ms × slides`.
//! The corrected arithmetic lands HERE with a hand-computed unit test —
//! not deferred to specimen validation — and the §7.3 line itself is
//! amended in the phase-3 docs commit (commit c), which is the only
//! commit allowed to touch CONTEXT.md.

use std::fmt;

use super::{Beatmap, HitObjectKind, TimingPoint};

/// How an event is drawn on the axis (the renderer styles by kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Circle,
    Slider,
    Spinner,
}

/// One hit object as a span on the whole-map axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimelineEvent {
    pub kind: EventKind,
    /// The object's own `time` field.
    pub start_ms: f64,
    /// End of the span: the object time for circles, `endTime` for
    /// spinners, `time + slides × span_ms` for sliders (corrected §7.3).
    pub end_ms: f64,
}

/// The whole-map timeline (design-round ruling 1: overview, not a
/// scrolling window).
#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    /// First object time.
    pub span_start_ms: f64,
    /// Last object END — a slider/spinner tail, not necessarily the last
    /// object's own end: the max over all events.
    pub span_end_ms: f64,
    /// File order. The .osu wiki promises chronological objects and the
    /// parser deliberately does not sort (§5.1 note), so a broken
    /// promise stays visible here instead of being masked.
    pub events: Vec<TimelineEvent>,
    /// Kiai windows (§5.1 `effects` bit 0) as half-open `[start, end)`
    /// pairs, chronological, clamped to the object span, never empty.
    pub kiai_ranges: Vec<(f64, f64)>,
}

/// Why a timeline could not be built. The parser stores raw values on
/// purpose (its `sv_multiplier` doc defers semantic validation to the
/// caller) — this model is that caller, and it fails loud.
#[derive(Debug, Clone, PartialEq)]
pub enum TimelineError {
    /// A slider exists but the map has no uninherited timing point —
    /// ms-per-beat is unknowable.
    NoUninheritedTimingPoint,
    /// The active uninherited point carries a non-finite or non-positive
    /// ms-per-beat.
    BadBeatLength { time_ms: f32, beat_length: f32 },
    /// The active inherited point carries a beat length whose SV
    /// multiplier is not finite and positive (e.g. `beatLength: 0` →
    /// ±inf, a positive value → negative).
    BadSvMultiplier { time_ms: f32, beat_length: f32 },
    /// `SliderMultiplier` is not finite and positive — the corrected
    /// formula would divide by zero or run the slider backwards.
    BadSliderMultiplier { slider_multiplier: f32 },
}

impl fmt::Display for TimelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoUninheritedTimingPoint => write!(
                f,
                "slider present but the map has no uninherited timing point — \
                 ms-per-beat is unknowable"
            ),
            Self::BadBeatLength {
                time_ms,
                beat_length,
            } => write!(
                f,
                "uninherited timing point at {time_ms} ms has beatLength \
                 {beat_length} — must be finite and positive"
            ),
            Self::BadSvMultiplier {
                time_ms,
                beat_length,
            } => write!(
                f,
                "inherited timing point at {time_ms} ms has beatLength \
                 {beat_length} — SV multiplier must be finite and positive"
            ),
            Self::BadSliderMultiplier { slider_multiplier } => write!(
                f,
                "SliderMultiplier {slider_multiplier} — must be finite and positive"
            ),
        }
    }
}

impl std::error::Error for TimelineError {}

/// The uninherited point active at `time_ms`: the LAST one at or before
/// it (later file lines win same-timestamp ties). `None` when no
/// uninherited point exists at or before `time_ms` — the caller clamps
/// to the first uninherited point, osu!-style, or fails when the map has
/// none at all.
fn active_uninherited(points: &[TimingPoint], time_ms: f32) -> Option<&TimingPoint> {
    let mut active: Option<&TimingPoint> = None;
    for p in points.iter().filter(|p| p.uninherited) {
        if p.time_ms <= time_ms && active.is_none_or(|a| p.time_ms >= a.time_ms) {
            active = Some(p);
        }
    }
    active
}

/// The first uninherited point by time (the clamp target for objects
/// that start before any point).
fn first_uninherited(points: &[TimingPoint]) -> Option<&TimingPoint> {
    points
        .iter()
        .filter(|p| p.uninherited)
        .min_by(|a, b| a.time_ms.total_cmp(&b.time_ms))
}

/// The timing point (any kind) active at `time_ms`: the LAST one at or
/// before it, later file lines winning ties. Its
/// [`TimingPoint::sv_multiplier`] is the SV in force — an uninherited
/// point RESETS the multiplier to 1.0, which its `sv_multiplier` returns
/// exactly, so no special case is needed here.
fn active_point(points: &[TimingPoint], time_ms: f32) -> Option<&TimingPoint> {
    let mut active: Option<&TimingPoint> = None;
    for p in points {
        if p.time_ms <= time_ms && active.is_none_or(|a| p.time_ms >= a.time_ms) {
            active = Some(p);
        }
    }
    active
}

/// ONE slide (span) of a slider, in ms — the corrected §7.3 formula:
/// `length / (SliderMultiplier × 100 × SV) × beatLength` (module docs).
/// Isolated as pure arithmetic so the hand-computed test pins exactly
/// this expression.
pub fn slider_span_ms(
    length_osupx: f64,
    slider_multiplier: f64,
    sv: f64,
    beat_length_ms: f64,
) -> f64 {
    length_osupx / (100.0 * slider_multiplier * sv) * beat_length_ms
}

impl Timeline {
    /// Build the whole-map model. Errors only on semantically broken
    /// timing data; a map with no hit objects builds as an empty `0..0`
    /// span — the LOADER fails loud on that case (nothing to visualize),
    /// the model stays total.
    pub fn build(map: &Beatmap) -> Result<Self, TimelineError> {
        let sm = map.difficulty.slider_multiplier;
        if !sm.is_finite() || sm <= 0.0 {
            return Err(TimelineError::BadSliderMultiplier {
                slider_multiplier: sm,
            });
        }
        let sm = f64::from(sm);

        let mut events = Vec::with_capacity(map.hit_objects.len());
        for o in &map.hit_objects {
            let start = f64::from(o.time_ms);
            let (kind, end) = match &o.kind {
                HitObjectKind::Circle => (EventKind::Circle, start),
                HitObjectKind::Spinner { end_time_ms } => {
                    (EventKind::Spinner, f64::from(*end_time_ms))
                }
                HitObjectKind::Slider(s) => {
                    // Corrected §7.3 (module docs): beatLength from the
                    // active UNinherited point, SV from the active point
                    // of any kind, ×100 in the velocity.
                    let bl_point = active_uninherited(&map.timing_points, o.time_ms)
                        .or_else(|| first_uninherited(&map.timing_points))
                        .ok_or(TimelineError::NoUninheritedTimingPoint)?;
                    if !bl_point.beat_length.is_finite() || bl_point.beat_length <= 0.0 {
                        return Err(TimelineError::BadBeatLength {
                            time_ms: bl_point.time_ms,
                            beat_length: bl_point.beat_length,
                        });
                    }
                    let sv_point = active_point(&map.timing_points, o.time_ms);
                    if let Some(p) = sv_point {
                        let sv = p.sv_multiplier();
                        if !sv.is_finite() || sv <= 0.0 {
                            return Err(TimelineError::BadSvMultiplier {
                                time_ms: p.time_ms,
                                beat_length: p.beat_length,
                            });
                        }
                    }
                    let sv = sv_point.map_or(1.0, |p| p.sv_multiplier());
                    let span = slider_span_ms(
                        f64::from(s.length),
                        sm,
                        f64::from(sv),
                        f64::from(bl_point.beat_length),
                    );
                    (EventKind::Slider, start + span * f64::from(s.slides))
                }
            };
            events.push(TimelineEvent {
                kind,
                start_ms: start,
                end_ms: end,
            });
        }

        let span_start_ms = events
            .iter()
            .map(|e| e.start_ms)
            .fold(f64::INFINITY, f64::min);
        let span_end_ms = events
            .iter()
            .map(|e| e.end_ms)
            .fold(f64::NEG_INFINITY, f64::max);
        let (span_start_ms, span_end_ms) = if events.is_empty() {
            (0.0, 0.0)
        } else {
            (span_start_ms, span_end_ms)
        };

        // Kiai windows: the state is the kiai bit of the last point at or
        // before t; a window opens at a kiai point and closes at the next
        // non-kiai point, or at the map's span end. Points are scanned in
        // TIME order (stable sort — file order breaks same-timestamp
        // ties); an out-of-order file warns once, because the parser
        // keeps file order on purpose so a broken chronology promise
        // surfaces HERE (§5.1 note), not silently.
        let mut kiai_ranges: Vec<(f64, f64)> = Vec::new();
        {
            let mut sorted: Vec<&TimingPoint> = map.timing_points.iter().collect();
            if sorted.windows(2).any(|w| w[0].time_ms > w[1].time_ms) {
                tracing::warn!(
                    "timing points are not chronological — the .osu wiki promises \
                     sorted points; kiai windows and slider lookups run in time order \
                     (file order kept for same-timestamp ties)"
                );
            }
            sorted.sort_by(|a, b| a.time_ms.total_cmp(&b.time_ms));
            let mut open: Option<f64> = None;
            for p in sorted {
                if p.kiai {
                    if open.is_none() {
                        open = Some(f64::from(p.time_ms));
                    }
                } else if let Some(start) = open.take() {
                    kiai_ranges.push((start, f64::from(p.time_ms)));
                }
            }
            if let Some(start) = open {
                kiai_ranges.push((start, span_end_ms));
            }
        }
        // Clamp to the object span and drop empty windows: a kiai point
        // after the last object (the golden fixture's trailing effects=9
        // point does exactly this) opens no visible window.
        let kiai_ranges = kiai_ranges
            .into_iter()
            .map(|(a, b)| (a.max(span_start_ms), b.min(span_end_ms)))
            .filter(|&(a, b)| b > a)
            .collect();

        Ok(Timeline {
            span_start_ms,
            span_end_ms,
            events,
            kiai_ranges,
        })
    }

    /// Whole-map overview position of `time_ms` as `0..=1`, clamped
    /// outside the span (ruling 1: the axis shows the whole map).
    pub fn frac(&self, time_ms: f64) -> f64 {
        let d = self.span_end_ms - self.span_start_ms;
        if d <= 0.0 {
            return 0.0;
        }
        ((time_ms - self.span_start_ms) / d).clamp(0.0, 1.0)
    }

    /// Whether `time_ms` falls inside a kiai window (half-open
    /// `[start, end)`).
    pub fn kiai_at(&self, time_ms: f64) -> bool {
        self.kiai_ranges
            .iter()
            .any(|&(a, b)| a <= time_ms && time_ms < b)
    }

    /// `(circles, sliders, spinners)` — the loader's summary log line.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut c = (0, 0, 0);
        for e in &self.events {
            match e.kind {
                EventKind::Circle => c.0 += 1,
                EventKind::Slider => c.1 += 1,
                EventKind::Spinner => c.2 += 1,
            }
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beatmap::parse_osu;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Synthetic map via the parser (the only `Beatmap` constructor);
    /// invented metadata only (I-03).
    fn map(slider_multiplier: &str, timing: &str, objects: &str) -> Beatmap {
        parse_osu(&format!(
            "osu file format v14\n\
             \n\
             [General]\n\
             AudioFilename: test.mp3\n\
             Mode: 0\n\
             \n\
             [Metadata]\n\
             Version: Synthetic\n\
             \n\
             [Difficulty]\n\
             HPDrainRate: 5\n\
             CircleSize: 4\n\
             OverallDifficulty: 8\n\
             SliderMultiplier: {slider_multiplier}\n\
             SliderTickRate: 1\n\
             \n\
             [TimingPoints]\n\
             {timing}\n\
             \n\
             [HitObjects]\n\
             {objects}\n"
        ))
        .unwrap()
    }

    /// HAND-COMPUTED pin of the corrected §7.3 formula (user ruling
    /// 2026-11-17; module docs). Map: `SliderMultiplier 2.0`, uninherited
    /// point at t=0 with beatLength 500 ms, inherited point at t=1000
    /// with beatLength −50 → SV = −100/−50 = 2.0.
    ///
    /// Slider A (t=2000, length 300, slides 2):
    ///   velocity = 100 × 2.0 × 2.0 = 400 osu-px per beat
    ///   one slide = 300 ÷ 400 = 0.75 beats = 0.75 × 500 = 375 ms
    ///   whole slider = 375 × 2 = 750 ms → span 2000..2750.
    /// The UNCORRECTED §7.3 text (length / (SliderMultiplier · SV) ×
    /// beatLength = 300 ÷ 4 × 500 = 37 500 ms per slide) is 100× too
    /// long — this test pins the ×100.
    ///
    /// Slider B (t=500, before the inherited point → SV 1.0, length 160,
    /// slides 1): one slide = 160 ÷ (100 × 2.0 × 1.0) × 500 = 0.8 × 500
    /// = 400 ms → span 500..900.
    #[test]
    fn slider_duration_is_the_corrected_x100_formula() {
        // Pure-arithmetic pin first: 300/(100·2·2)·500 = 375 exactly.
        assert!(close(slider_span_ms(300.0, 2.0, 2.0, 500.0), 375.0));
        assert!(close(slider_span_ms(160.0, 2.0, 1.0, 500.0), 400.0));

        // Then through the model, parsed from synthetic .osu text.
        let m = map(
            "2.0",
            "0,500,4,2,0,60,1,0\n1000,-50,4,2,0,60,0,0",
            "256,192,500,2,0,L|400:192,1,160\n\
             256,192,2000,2,0,L|400:192,2,300",
        );
        let tl = Timeline::build(&m).unwrap();
        let sliders: Vec<TimelineEvent> = tl
            .events
            .iter()
            .copied()
            .filter(|e| e.kind == EventKind::Slider)
            .collect();
        assert_eq!(sliders.len(), 2);
        assert_eq!((sliders[0].start_ms, sliders[0].end_ms), (500.0, 900.0));
        assert_eq!((sliders[1].start_ms, sliders[1].end_ms), (2000.0, 2750.0));
        assert_eq!((tl.span_start_ms, tl.span_end_ms), (500.0, 2750.0));
    }

    /// Timing-point lookup semantics: an inherited point doubles SV
    /// until the next UNinherited point RESETS it to 1.0; a slider
    /// before the first uninherited point clamps to it.
    ///
    /// Timing: t=500 uninherited 500 ms; t=1500 inherited −50 (SV 2.0);
    /// t=2500 uninherited 400 ms (SV reset). `SliderMultiplier 1.0`.
    /// - slider t=0, length 50: clamped beatLength 500, SV 1.0 →
    ///   50 ÷ 100 × 500 = 250 ms → 0..250
    /// - slider t=2000, length 100: beatLength 500, SV 2.0 →
    ///   100 ÷ 200 × 500 = 250 ms → 2000..2250
    /// - slider t=3000, length 100: beatLength 400, SV 1.0 (reset) →
    ///   100 ÷ 100 × 400 = 400 ms → 3000..3400
    #[test]
    fn timing_lookups_reset_and_clamp() {
        let m = map(
            "1.0",
            "500,500,4,2,0,60,1,0\n1500,-50,4,2,0,60,0,0\n2500,400,4,2,0,60,1,0",
            "256,192,0,2,0,L|100:192,1,50\n\
             256,192,2000,2,0,L|100:192,1,100\n\
             256,192,3000,2,0,L|100:192,1,100",
        );
        let tl = Timeline::build(&m).unwrap();
        let spans: Vec<(f64, f64)> = tl.events.iter().map(|e| (e.start_ms, e.end_ms)).collect();
        assert_eq!(
            spans,
            vec![(0.0, 250.0), (2000.0, 2250.0), (3000.0, 3400.0)]
        );
    }

    /// The golden flagship fixture (the timeline-visualizer fixture per
    /// §5.1) — every expected number below is hand-derived from its
    /// bytes.
    ///
    /// Timing: `SliderMultiplier 1.9`; TP0 t=0 uninherited 375 ms;
    /// TP1 t=1000 inherited −100 → SV 1.0; TP2 t=4000 inherited −50 →
    /// SV 2.0, kiai ON; TP3 t=8000 uninherited 375, kiai OFF; TP4
    /// t=12000 inherited −125 → SV 0.8, kiai ON (past the last object —
    /// clamped away).
    ///
    /// Sliders (span = length ÷ (100 × 1.9 × SV) × 375, × slides):
    /// - t=1500, L, length 200, slides 1, SV 1.0:
    ///   200/190 × 375 = 20/19 × 375 = 7500/19 ≈ 394.7368
    ///   → end ≈ 1894.7368
    /// - t=2250, B, length 180.5, slides 2, SV 1.0:
    ///   180.5/190 = 0.95 → 0.95 × 375 = 356.25 per slide × 2 = 712.5
    ///   → end = 2962.5
    /// - t=5000, P, length 180, slides 1, SV 2.0:
    ///   180/380 × 375 = 9/19 × 375 = 3375/19 ≈ 177.6316
    ///   → end ≈ 5177.6316
    /// - t=5750, C, length 250, slides 1, SV 2.0:
    ///   250/380 × 375 = 25/38 × 375 = 9375/38 ≈ 246.7105
    ///   → end ≈ 5996.7105
    ///
    /// (1.9 is not exactly representable in f32, so the slider ends get
    /// a 1e-4 tolerance — orders of magnitude tighter than any formula
    /// confusion: the uncorrected §7.3 text would be off by ×100.)
    ///
    /// Spinners: 3000..4200 and 7000..8500 (`endTime`, no formula).
    /// Span: 0 (first object) .. 8500 (last spinner end).
    /// Kiai: [4000, 8000) — TP2 opens it, TP3 closes it; TP4's window
    /// starts past the last object and is clamped away.
    #[test]
    fn golden_v14_standard_spinner_slider_and_kiai() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/golden/v14-standard.osu"
        ))
        .expect("golden fixture is committed under assets/golden");
        let m = parse_osu(&text).unwrap();
        let tl = Timeline::build(&m).unwrap();

        assert_eq!((tl.span_start_ms, tl.span_end_ms), (0.0, 8500.0));
        assert_eq!(tl.counts(), (4, 4, 2));

        let slider_ends: Vec<f64> = tl
            .events
            .iter()
            .filter(|e| e.kind == EventKind::Slider)
            .map(|e| e.end_ms)
            .collect();
        assert_eq!(slider_ends.len(), 4);
        assert!((slider_ends[0] - (1500.0 + 7500.0 / 19.0)).abs() < 1e-4);
        assert!((slider_ends[1] - 2962.5).abs() < 1e-4);
        assert!((slider_ends[2] - (5000.0 + 3375.0 / 19.0)).abs() < 1e-4);
        assert!((slider_ends[3] - (5750.0 + 9375.0 / 38.0)).abs() < 1e-4);

        let spinner_spans: Vec<(f64, f64)> = tl
            .events
            .iter()
            .filter(|e| e.kind == EventKind::Spinner)
            .map(|e| (e.start_ms, e.end_ms))
            .collect();
        assert_eq!(spinner_spans, vec![(3000.0, 4200.0), (7000.0, 8500.0)]);

        assert_eq!(tl.kiai_ranges, vec![(4000.0, 8000.0)]);
        assert!(!tl.kiai_at(3999.9));
        assert!(tl.kiai_at(4000.0));
        assert!(tl.kiai_at(7999.9));
        assert!(!tl.kiai_at(8000.0));

        assert_eq!(tl.frac(0.0), 0.0);
        assert_eq!(tl.frac(8500.0), 1.0);
        assert!((tl.frac(4250.0) - 0.5).abs() < 1e-12);
        assert_eq!(tl.frac(-100.0), 0.0);
        assert_eq!(tl.frac(999_999.0), 1.0);
    }

    /// Semantically broken timing fails loud (the parser defers
    /// validation here on purpose).
    #[test]
    fn broken_timing_fails_loud() {
        let slider = "256,192,2000,2,0,L|400:192,1,100";
        // Slider but no uninherited point at all.
        let m = map("1.0", "1000,-50,4,2,0,60,0,0", slider);
        assert_eq!(
            Timeline::build(&m).unwrap_err(),
            TimelineError::NoUninheritedTimingPoint
        );
        // Inherited beatLength 0 → SV = ±inf.
        let m = map("1.0", "0,500,4,2,0,60,1,0\n1000,0,4,2,0,60,0,0", slider);
        assert!(matches!(
            Timeline::build(&m).unwrap_err(),
            TimelineError::BadSvMultiplier { .. }
        ));
        // Uninherited beatLength negative.
        let m = map("1.0", "0,-500,4,2,0,60,1,0", slider);
        assert!(matches!(
            Timeline::build(&m).unwrap_err(),
            TimelineError::BadBeatLength { .. }
        ));
        // SliderMultiplier 0 → corrected formula would divide by zero.
        let m = map("0", "0,500,4,2,0,60,1,0", slider);
        assert!(matches!(
            Timeline::build(&m).unwrap_err(),
            TimelineError::BadSliderMultiplier { .. }
        ));
    }

    /// Degenerate maps build totally: the LOADER is what fails loud on
    /// empty maps; circles need no timing points at all.
    #[test]
    fn empty_and_circle_only_maps_are_total() {
        let m = map("1.0", "0,500,4,2,0,60,1,0", "");
        let tl = Timeline::build(&m).unwrap();
        assert_eq!((tl.span_start_ms, tl.span_end_ms), (0.0, 0.0));
        assert!(tl.events.is_empty() && tl.kiai_ranges.is_empty());
        assert_eq!(tl.frac(123.0), 0.0);

        let m = map("1.0", "", "256,192,100,1,0\n256,192,200,1,0");
        let tl = Timeline::build(&m).unwrap();
        assert_eq!(tl.events.len(), 2);
        assert_eq!((tl.span_start_ms, tl.span_end_ms), (100.0, 200.0));
        assert_eq!(tl.counts(), (2, 0, 0));
    }
}
