//! The gameplay session core — tap resolution, miss deadlines, drain
//! (Δ-P/Δ-Q/Δ-R/Δ-T; rulings Q-B/Q-E + rev-C drain corrections).
//!
//! [`GameplayCore`] owns every piece of judgement state for one play
//! through a map: the playable circles (sliders/spinners are phase 6 —
//! inert during play, Δ-AC — but their spans still gate HP drain), the
//! [`ScoreState`], and the [`DrainPlan`]. It is a pure state machine over
//! caller-supplied song time (I-09); commits b–d feed it render snapshots
//! and Wayland input, nothing here does I/O.
//!
//! ## Ordering discipline (pinned by tests)
//!
//! * `tap(at)` first ADVANCES the clock to `at` (miss deadlines +
//!   drain), then resolves the tap. A miss fires only when the clock
//!   is STRICTLY past `t + w50` — `to − t > w50`, the same comparison
//!   the tap check makes in reverse (`|Δ| ≤ w50` is a hit; rev-A
//!   point 3) — so a tap landing at exactly `t + w50` judges Hit50,
//!   and one instant later the note misses for good.
//!   [`judge_delta`](super::judge::judge_delta) is inclusive on both
//!   sides: a tap at exactly `t − w50` is also a Hit50.
//! * Inside `advance`, misses fire first, then the drain over the whole
//!   stepped interval is applied. The total is order-independent unless
//!   an HP clamp is crossed mid-interval; the order is pinned so every
//!   number is deterministic.
//! * Fail latches: the moment HP reaches `0.0` — via a miss or a drain
//!   step, including the drain inside `tap()`'s own advance —
//!   [`failed`](GameplayCore::failed) engages and no later hit raises
//!   HP or score (the counts still record what happened).
//!
//! ## Notelock (Δ-P, Q-B)
//!
//! Notes always resolve in index order. A tap resolves the EARLIEST
//! unjudged note whose 50-window contains the tap time; after
//! advance-first ordering that is by construction the head note, and a
//! later note whose window is open can never be judged while an earlier
//! one is still pending. A tap that resolves nothing (window closed, out
//! of radius, no notes left) is ignored — never a ghost miss; misses
//! fire only at deadlines via `advance`.
//!
//! ## Drain (Δ-R)
//!
//! `active` windows = object span − inferred breaks (Δ-T: gaps ≥
//! [`BREAK_GAP_MS`]) − non-circle intervals; `drain_time_s` for the score
//! multiplier is the span minus breaks ONLY (all objects count). No drain
//! in lead-in/intro/outro (outside the span), during breaks, or during
//! slider/spinner stretches — the HP bar and the multiplier see two
//! different quantities on purpose.

use crate::beatmap::Beatmap;
use crate::beatmap::timeline::{EventKind, Timeline};

use super::judge::{Judgement, Windows, circle_radius_osupx, judge_delta};
use super::score::{ScoreState, difficulty_multiplier};

/// Gap between consecutive objects that infers a break (Δ-T): ≥ 2000 ms.
pub const BREAK_GAP_MS: f64 = 2000.0;

/// One playable circle, widened to f64 for judgement math.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CircleNote {
    pub time_ms: f64,
    /// osu!px.
    pub x: f64,
    /// osu!px.
    pub y: f64,
    /// Raw hitSound bitmask (bit 0 normal, 1 whistle, 2 finish, 3 clap) —
    /// the hitsound layering (Δ-Z, commit d) reads this.
    pub hit_sound: u8,
    pub judgement: Option<Judgement>,
}

/// One tap. `pos: None` = keyboard tap — the live cursor position
/// applies (§7.5 `InputEvent`: key events carry no position; mouse
/// buttons carry theirs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tap {
    pub at_ms: f64,
    pub pos: Option<(f64, f64)>,
}

/// HP-drain geometry + the multiplier's drain time (Δ-R/Δ-T).
#[derive(Debug, Clone, PartialEq)]
pub struct DrainPlan {
    /// The object span `[first start, last end]`.
    pub span: (f64, f64),
    /// Inferred breaks (Δ-T): `[end_i, start_{i+1}]` for gaps ≥
    /// [`BREAK_GAP_MS`]. Chronological, disjoint.
    pub breaks: Vec<(f64, f64)>,
    /// Active drain windows: span − breaks − non-circle intervals,
    /// chronological, disjoint, half-open `[start, end)`.
    pub active: Vec<(f64, f64)>,
    /// (span − breaks) in seconds — the multiplier's drain time; ALL
    /// objects count, unlike `active`.
    pub drain_time_s: f64,
}

impl DrainPlan {
    /// Build from the start-sorted `(start, end)` intervals of ALL
    /// objects and the `(start, end)` intervals of the non-circle objects
    /// (the HP cuts). `intervals` MUST be sorted by start; `cuts` need
    /// not be.
    pub fn new(span: (f64, f64), intervals: &[(f64, f64)], cuts: &[(f64, f64)]) -> Self {
        // Inferred breaks: gaps ≥ BREAK_GAP_MS between consecutive
        // intervals (rev-C ruling: the 2000 ms gap between circle@1000
        // and slider@3000 IS a break — ≥, not >).
        let mut breaks = Vec::new();
        for w in intervals.windows(2) {
            let gap = w[1].0 - w[0].1;
            if gap >= BREAK_GAP_MS {
                breaks.push((w[0].1, w[1].0));
            }
        }
        let break_ms: f64 = breaks.iter().map(|&(a, b)| b - a).sum();
        let drain_time_s = ((span.1 - span.0) - break_ms).max(0.0) / 1000.0;

        // Active = span − merged(breaks ∪ cuts).
        let mut inactive: Vec<(f64, f64)> = breaks.clone();
        inactive.extend_from_slice(cuts);
        let merged = merge_intervals(&inactive);
        let active = subtract_all(span, &merged);

        Self {
            span,
            breaks,
            active,
            drain_time_s,
        }
    }

    /// Total ms of `[t0, t1]` inside the active windows.
    pub fn active_ms_between(&self, t0: f64, t1: f64) -> f64 {
        if t1 <= t0 {
            return 0.0;
        }
        let mut total = 0.0;
        for &(a, b) in &self.active {
            if a >= t1 {
                break;
            }
            let lo = a.max(t0);
            let hi = b.min(t1);
            if hi > lo {
                total += hi - lo;
            }
        }
        total
    }
}

/// Sort by start and merge overlapping/touching intervals.
fn merge_intervals(v: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut sorted: Vec<(f64, f64)> = v.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (s, e) in sorted {
        // Let chain (edition 2024): one arm for "merges into the last
        // interval", one for "starts a new one" — no nested ifs.
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
            continue;
        }
        merged.push((s, e));
    }
    merged
}

/// `base` minus the merged, start-sorted `cuts`.
fn subtract_all(base: (f64, f64), cuts: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    let mut cursor = base.0;
    for &(s, e) in cuts {
        if e <= cursor {
            continue; // cut entirely before the surviving span
        }
        if s >= base.1 {
            break; // cuts are sorted; nothing later can matter
        }
        if s > cursor {
            out.push((cursor, s)); // keep the surviving stretch
        }
        cursor = cursor.max(e);
        if cursor >= base.1 {
            return out; // nothing survives
        }
    }
    if cursor < base.1 {
        out.push((cursor, base.1));
    }
    out
}

/// The judgement state machine for one play through a map.
///
/// Built from a parsed [`Beatmap`] plus its [`Timeline`]: circles become
/// playable notes (time-sorted — timeline events are file order, §5.1
/// note), sliders/spinners contribute only their spans (breaks + drain
/// geometry; phase 6 makes them playable). All positions are osu!px in
/// f64, widened at construction.
#[derive(Debug, Clone)]
pub struct GameplayCore {
    notes: Vec<CircleNote>,
    /// Index of the first unjudged note — notes always resolve in index
    /// order (notelock, module docs), so this is the eligible-target head.
    first_pending: usize,
    windows: Windows,
    /// Circle radius in osu!px (§7.2 CS formula).
    radius: f64,
    hp_drain_rate: f64,
    drain: DrainPlan,
    cursor: (f64, f64),
    last_time_ms: f64,
    score: ScoreState,
}

impl GameplayCore {
    /// Build the core for one play. The 1:1 events↔objects zip is
    /// structural (`Timeline::build` emits one event per hit object, in
    /// file order) — asserted in debug, defensively zipped in release.
    pub fn new(beatmap: &Beatmap, timeline: &Timeline) -> Self {
        debug_assert_eq!(
            timeline.events.len(),
            beatmap.hit_objects.len(),
            "timeline events must map 1:1 onto hit objects"
        );
        let diff = &beatmap.difficulty;

        let mut notes = Vec::new();
        let mut intervals: Vec<(f64, f64)> = Vec::with_capacity(timeline.events.len());
        let mut non_circle: Vec<(f64, f64)> = Vec::new();
        for (ev, obj) in timeline.events.iter().zip(&beatmap.hit_objects) {
            intervals.push((ev.start_ms, ev.end_ms));
            match ev.kind {
                EventKind::Circle => notes.push(CircleNote {
                    time_ms: ev.start_ms,
                    x: f64::from(obj.x),
                    y: f64::from(obj.y),
                    hit_sound: obj.hit_sound,
                    judgement: None,
                }),
                EventKind::Slider | EventKind::Spinner => {
                    non_circle.push((ev.start_ms, ev.end_ms));
                }
            }
        }
        // Time order for notelock (file order kept for equal times).
        notes.sort_by(|a, b| a.time_ms.total_cmp(&b.time_ms));
        intervals.sort_by(|a, b| a.0.total_cmp(&b.0));

        let drain = DrainPlan::new(
            (timeline.span_start_ms, timeline.span_end_ms),
            &intervals,
            &non_circle,
        );
        let score = ScoreState::new(difficulty_multiplier(
            f64::from(diff.hp_drain_rate),
            f64::from(diff.circle_size),
            f64::from(diff.overall_difficulty),
            timeline.events.len() as u32,
            drain.drain_time_s,
        ));

        Self {
            radius: circle_radius_osupx(f64::from(diff.circle_size)),
            windows: Windows::from_od(f64::from(diff.overall_difficulty)),
            hp_drain_rate: f64::from(diff.hp_drain_rate),
            drain,
            cursor: (256.0, 192.0), // playfield-centre default
            last_time_ms: 0.0,
            notes,
            first_pending: 0,
            score,
        }
    }

    /// Advance song time to `to_ms` (monotonic; a no-op when not ahead):
    /// fire miss deadlines in note order, then drain HP over the stepped
    /// interval's ACTIVE windows. Returns the number of misses fired.
    ///
    /// A miss fires only STRICTLY past `t + w50` (rev-A point 3): at
    /// the exact deadline instant the note is still live, so a tap then
    /// judges Hit50 — `to_ms − t ≤ w50` here is the same comparison the
    /// tap check's `|Δ| ≤ w50` makes.
    pub fn advance(&mut self, to_ms: f64) -> u32 {
        if to_ms <= self.last_time_ms {
            return 0;
        }
        let mut misses = 0;
        while let Some(note) = self.notes.get(self.first_pending) {
            if to_ms - note.time_ms <= self.windows.hit50 {
                break; // still inside the tap window — not missed yet
            }
            self.notes[self.first_pending].judgement = Some(Judgement::Miss);
            self.first_pending += 1;
            self.score.apply(Judgement::Miss);
            misses += 1;
        }
        let active_ms = self.drain.active_ms_between(self.last_time_ms, to_ms);
        self.score.drain(active_ms, self.hp_drain_rate);
        self.last_time_ms = to_ms;
        misses
    }

    /// Resolve one tap (Δ-P notelock; the ordering discipline is in the
    /// module docs). `None` = the tap judged nothing — no state changed;
    /// `Some((note_index, judgement))` = the note was hit.
    pub fn tap(&mut self, tap: Tap) -> Option<(usize, Judgement)> {
        self.advance(tap.at_ms);
        let (x, y) = tap.pos.unwrap_or(self.cursor);
        let idx = self.first_pending;
        let note = *self.notes.get(idx)?;
        let delta = tap.at_ms - note.time_ms;
        if delta.abs() > self.windows.hit50 {
            return None; // no open window → ignored (no ghost miss)
        }
        let dx = x - note.x;
        let dy = y - note.y;
        if dx * dx + dy * dy > self.radius * self.radius {
            return None; // outside the circle → ignored
        }
        let judgement = judge_delta(delta, &self.windows)?;
        self.notes[idx].judgement = Some(judgement);
        self.first_pending += 1;
        self.score.apply(judgement);
        self.score.record_hit_error(delta);
        Some((idx, judgement))
    }

    /// All playable notes, time-sorted (the render commit draws circles +
    /// rings from this; tests pin judgements on it).
    pub fn notes(&self) -> &[CircleNote] {
        &self.notes
    }

    /// Live score/combo/accuracy/HP state.
    pub fn score(&self) -> &ScoreState {
        &self.score
    }

    /// Fail latch: HP hit zero at some point this play (a miss or a
    /// drain step — including the drain inside `tap()`'s own advance).
    /// Once latched, hits never raise HP or score again; the play
    /// wiring (commit d) reads this as FAILED (Δ-W).
    pub fn failed(&self) -> bool {
        self.score.failed()
    }

    /// The drain geometry (Δ-R: `active` windows vs the multiplier's
    /// `drain_time_s`).
    pub fn drain(&self) -> &DrainPlan {
        &self.drain
    }

    /// Circle radius in osu!px.
    pub fn radius_osupx(&self) -> f64 {
        self.radius
    }

    /// Judgement windows in force.
    pub fn windows(&self) -> Windows {
        self.windows
    }

    /// Every circle judged (FINISHED detection adds the time check).
    pub fn all_judged(&self) -> bool {
        self.first_pending >= self.notes.len()
    }

    /// Time of the next pending note, if any.
    pub fn next_note_time(&self) -> Option<f64> {
        self.notes.get(self.first_pending).map(|n| n.time_ms)
    }

    /// Update the virtual cursor (osu!px) from a pointer event; the
    /// input commit calls this with sensitivity/invert/clamp already
    /// applied (I-13: PLAYING only, never wallpaper passthrough).
    pub fn set_cursor(&mut self, x: f64, y: f64) {
        self.cursor = (x, y);
    }

    /// The virtual cursor position (osu!px).
    pub fn cursor(&self) -> (f64, f64) {
        self.cursor
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
    /// invented metadata only (I-03). Circles need no timing points
    /// (pinned in timeline.rs), but the shared scaffold keeps one so
    /// slider fixtures work too.
    fn core(hp: &str, cs: &str, od: &str, objects: &str) -> GameplayCore {
        let map = parse_osu(&format!(
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
             HPDrainRate: {hp}\n\
             CircleSize: {cs}\n\
             OverallDifficulty: {od}\n\
             SliderMultiplier: 1.0\n\
             SliderTickRate: 1\n\
             \n\
             [TimingPoints]\n\
             0,500,4,2,0,60,1,0\n\
             \n\
             [HitObjects]\n\
             {objects}\n"
        ))
        .unwrap();
        let timeline = Timeline::build(&map).unwrap();
        GameplayCore::new(&map, &timeline)
    }

    /// Tap helper: explicit position.
    fn tap_at(core: &mut GameplayCore, at_ms: f64, x: f64, y: f64) -> Option<(usize, Judgement)> {
        core.tap(Tap {
            at_ms,
            pos: Some((x, y)),
        })
    }

    /// Δ-P notelock pins (Q-B). OD10 (windows 20/60/100), CS4 (radius
    /// 36.48), A@1000 at (100,100), B@1050 at (200,100).
    ///
    /// * A tap over B while A is pending is ignored — B is never the
    ///   target while an earlier note is unresolved, and the position is
    ///   out of A's radius (distance 100 > 36.48).
    /// * The same tap over A: Δ=45 → Hit100, B untouched (the earlier
    ///   note eats the tap though B's window is also open).
    /// * Once A is resolved, B is the head: Δ=10 → Hit300.
    #[test]
    fn notelock_pins_earliest_note_wins() {
        let mut c = core("5", "4", "10", "100,100,1000,1,0\n200,100,1050,1,0");
        assert_eq!(tap_at(&mut c, 1045.0, 200.0, 100.0), None);
        assert_eq!(c.notes()[0].judgement, None);
        assert_eq!(c.notes()[1].judgement, None);
        assert_eq!(c.score().counts(), (0, 0, 0, 0));
        assert_eq!(
            tap_at(&mut c, 1045.0, 100.0, 100.0),
            Some((0, Judgement::Hit100))
        );
        assert_eq!(c.notes()[1].judgement, None);
        assert_eq!(
            tap_at(&mut c, 1060.0, 200.0, 100.0),
            Some((1, Judgement::Hit300))
        );
    }

    /// Δ-P: early taps and taps with no open window judge nothing — no
    /// ghost miss, no state change. OD10 (w50 = 100): A's window is
    /// [900, 1100].
    #[test]
    fn early_and_midgap_taps_are_ignored() {
        let mut c = core("5", "4", "10", "100,100,1000,1,0\n200,100,3000,1,0");
        // Way early.
        assert_eq!(tap_at(&mut c, 800.0, 100.0, 100.0), None);
        // Exactly the early boundary: t − w50 → Hit50 (Q-A inclusivity).
        assert_eq!(
            tap_at(&mut c, 900.0, 100.0, 100.0),
            Some((0, Judgement::Hit50))
        );
        // Between windows (A judged at 900; B's window opens at 2900).
        assert_eq!(tap_at(&mut c, 2000.0, 200.0, 100.0), None);
        assert_eq!(c.score().counts(), (0, 0, 1, 0));
    }

    /// Misses fire STRICTLY past the deadline `t + w50` (rev-A point
    /// 3). At the exact deadline instant the note is still live; a tap
    /// then is a Hit50 — the tap wins over the deadline in the same
    /// call, one instant later the miss fires. OD5 → w50 = 150.
    #[test]
    fn miss_deadline_strictly_past_boundary() {
        let mut c = core("5", "4", "5", "100,100,1000,1,0");
        // Exactly at the deadline: no miss yet (single circle → zero-width
        // span, so no drain either).
        assert_eq!(c.advance(1150.0), 0); // deadline = 1000 + 150
        assert_eq!(c.notes()[0].judgement, None);
        // One instant later: the miss fires.
        assert_eq!(c.advance(1150.0001), 1);
        assert_eq!(c.notes()[0].judgement, Some(Judgement::Miss));
        assert_eq!(c.score().counts(), (0, 0, 0, 1));
        assert!(close(c.score().hp(), 0.9)); // 1.0 − 0.10

        // Tap at the exact boundary instant wins inside tap()'s own
        // advance-then-judge ordering: no miss fires, the tap is Hit50.
        let mut c2 = core("5", "4", "5", "100,100,1000,1,0\n200,100,2000,1,0");
        assert_eq!(
            tap_at(&mut c2, 1150.0, 100.0, 100.0),
            Some((0, Judgement::Hit50))
        );
        assert_eq!(c2.notes()[0].judgement, Some(Judgement::Hit50));
        assert_eq!(c2.notes()[1].judgement, None); // notelock head moved on
        // The same tap one instant later judges nothing (window closed,
        // the miss already fired inside the advance).
        let mut c3 = core("5", "4", "5", "100,100,1000,1,0\n200,100,2000,1,0");
        assert_eq!(tap_at(&mut c3, 1150.0001, 100.0, 100.0), None);
        assert_eq!(c3.notes()[0].judgement, Some(Judgement::Miss));
    }

    /// Position gate: a tap in the window but outside the radius is
    /// ignored (CS4 → radius 36.48); a keyboard tap (pos None) judges at
    /// the live cursor, whose default is the playfield centre.
    #[test]
    fn position_gate_pins() {
        let mut c = core("5", "4", "5", "100,100,1000,1,0");
        assert!(close(c.radius_osupx(), 36.48));
        // Clearly outside (distance 50 > 36.48).
        assert_eq!(tap_at(&mut c, 1000.0, 150.0, 100.0), None);
        assert_eq!(c.notes()[0].judgement, None);
        // Inside.
        assert_eq!(
            tap_at(&mut c, 1000.0, 120.0, 100.0),
            Some((0, Judgement::Hit300))
        );

        // Keyboard tap at the default cursor (256, 192): out of range.
        let mut k = core("5", "4", "5", "100,100,1000,1,0");
        assert_eq!(k.cursor(), (256.0, 192.0));
        assert_eq!(
            k.tap(Tap {
                at_ms: 1000.0,
                pos: None
            }),
            None
        );
        // Move the cursor onto the note and the same tap hits.
        k.set_cursor(100.0, 100.0);
        assert_eq!(
            k.tap(Tap {
                at_ms: 1000.0,
                pos: None
            }),
            Some((0, Judgement::Hit300))
        );
    }

    /// Δ-R/Δ-T drain pins (rev-C corrections): the ≥ 2000 ms gap rule
    /// makes the circle@1000 → slider@3000 gap a break, so case A's
    /// active drain is (4000, 5000] only — 1000 ms; the multiplier's
    /// drain time is span − breaks = 4000 − 2000 = 2000 ms = 2 s
    /// (the slider still counts — breaks only).
    #[test]
    fn drain_plan_case_a_break_inferred() {
        let plan = DrainPlan::new(
            (1000.0, 5000.0),
            &[(1000.0, 1000.0), (3000.0, 4000.0), (5000.0, 5000.0)],
            &[(3000.0, 4000.0)],
        );
        assert_eq!(plan.breaks, vec![(1000.0, 3000.0)]);
        assert_eq!(plan.active, vec![(4000.0, 5000.0)]);
        assert!(close(plan.active_ms_between(0.0, 6000.0), 1000.0));
        assert!(close(plan.drain_time_s, 2.0));
    }

    /// Case B (rev-C fix 1): gap 1500 < 2000 ms → NO break, so the
    /// slider exclusion is tested alone: active = [1000, 2500) ∪
    /// (3500, 5000] = 3000 ms; multiplier drain time = full 4 s span
    /// (no breaks — contrast with case A's 2 s).
    #[test]
    fn drain_plan_case_b_no_break_slider_excluded() {
        let plan = DrainPlan::new(
            (1000.0, 5000.0),
            &[(1000.0, 1000.0), (2500.0, 3500.0), (5000.0, 5000.0)],
            &[(2500.0, 3500.0)],
        );
        assert!(plan.breaks.is_empty());
        assert_eq!(plan.active, vec![(1000.0, 2500.0), (3500.0, 5000.0)]);
        assert!(close(plan.active_ms_between(0.0, 6000.0), 3000.0));
        assert!(close(plan.drain_time_s, 4.0));
    }

    /// Δ-T boundary: exactly 2000 ms is a break (rev-C ruling); 1999 is
    /// not.
    #[test]
    fn break_gap_boundary() {
        let exact = DrainPlan::new((1000.0, 3000.0), &[(1000.0, 1000.0), (3000.0, 3000.0)], &[]);
        assert_eq!(exact.breaks, vec![(1000.0, 3000.0)]);
        assert!(exact.active.is_empty()); // zero-width span pieces
        assert!(close(exact.active_ms_between(1000.0, 3000.0), 0.0));

        let under = DrainPlan::new((1000.0, 2999.0), &[(1000.0, 1000.0), (2999.0, 2999.0)], &[]);
        assert!(under.breaks.is_empty());
        assert!(close(under.active_ms_between(0.0, 4000.0), 1999.0));
    }

    /// Integration of the corrected drain pins through a real parsed map
    /// (case A): circle@1000, slider 3000..4000 (span = 200/(100×1×1)×500
    /// = 1000 ms), circle@5000. Gap 1000→3000 = 2000 → break [1000,3000];
    /// active = (4000, 5000). OD8 → w50 = 120: both circle misses fire by
    /// 6000. HP 1.0 − 0.20 (two misses) − 0.025 (1000 active ms at rate
    /// 5) = 0.775. (The 0.5-start variants are pinned in score.rs.)
    #[test]
    fn integration_drain_case_a() {
        let mut c = core(
            "5",
            "4",
            "8",
            "100,100,1000,1,0\n100,100,3000,2,0,L|200:100,1,200\n200,200,5000,1,0",
        );
        assert_eq!(c.drain().breaks, vec![(1000.0, 3000.0)]);
        assert_eq!(c.drain().active, vec![(4000.0, 5000.0)]);
        assert_eq!(c.notes().len(), 2); // the slider is not a playable note
        c.advance(6000.0);
        assert_eq!(c.score().counts(), (0, 0, 0, 2));
        assert!(close(c.score().hp(), 0.775));
    }

    /// Case B through a parsed map: gap 1500 → no break; active = 3000 ms
    /// → HP 1.0 − 0.20 − 0.075 = 0.725.
    #[test]
    fn integration_drain_case_b() {
        let mut c = core(
            "5",
            "4",
            "8",
            "100,100,1000,1,0\n100,100,2500,2,0,L|200:100,1,200\n200,200,5000,1,0",
        );
        assert!(c.drain().breaks.is_empty());
        assert_eq!(c.drain().active, vec![(1000.0, 2500.0), (3500.0, 5000.0)]);
        c.advance(6000.0);
        assert!(close(c.score().hp(), 0.725));
    }

    /// The hand-computed full sequence (design-round pin table): OD5
    /// (windows 50/100/150), CS4, HPDrainRate 5, four circles at
    /// 1000/2000/3000/4000 (gaps 1000 → no breaks, no sliders: active =
    /// [1000, 4000] = 3000 ms; DiffMult = round((5+4+5+4/3·8)/38×5) = 3,
    /// unused at these combos — C = 0 throughout).
    ///
    /// Hits: 300@1000, 100@2060 (Δ=60), miss@3000, 50@4120 (Δ=120).
    ///
    /// HP by step (module docs' ordering: each tap advances its own
    /// [last, at] drain slice first; misses fire before the interval
    /// drain inside `advance`): 1.0 → 300: +0.05 clamped 1.0 → advance
    /// 2000: −0.025 → 0.975 → 100@2060: −0.0015 + 0.03 → clamp 1.0 →
    /// miss@3150: −0.10 → 0.90, drain 1940 ms: −0.0485 → 0.8515 →
    /// 50@4120: [4000, 4120] ∩ active = 0 (the active drain ENDS at the
    /// span end 4000) + 0.01 → 0.8615, and nothing drains after.
    #[test]
    fn full_sequence_hand_computed() {
        let mut c = core(
            "5",
            "4",
            "5",
            "100,100,1000,1,0\n120,100,2000,1,0\n140,100,3000,1,0\n160,100,4000,1,0",
        );
        assert_eq!(c.score().difficulty_multiplier(), 3);
        assert_eq!(c.drain().active, vec![(1000.0, 4000.0)]);

        assert_eq!(
            tap_at(&mut c, 1000.0, 100.0, 100.0),
            Some((0, Judgement::Hit300))
        );
        assert!(close(c.score().hp(), 1.0));
        c.advance(2000.0);
        assert!(close(c.score().hp(), 0.975));
        assert_eq!(
            tap_at(&mut c, 2060.0, 120.0, 100.0),
            Some((1, Judgement::Hit100))
        );
        assert!(close(c.score().hp(), 1.0));
        c.advance(4000.0); // fires the miss at deadline 3150
        assert_eq!(c.notes()[2].judgement, Some(Judgement::Miss));
        assert!(close(c.score().hp(), 0.8515));
        assert_eq!(
            tap_at(&mut c, 4120.0, 160.0, 100.0),
            Some((3, Judgement::Hit50))
        );
        assert!(close(c.score().hp(), 0.8615));
        c.advance(100_000.0); // drain [4120, ∞) ∩ active = 0 — it ended at 4000
        assert!(close(c.score().hp(), 0.8615));

        assert!(close(c.score().score(), 450.0));
        assert_eq!(c.score().combo(), 1);
        assert_eq!(c.score().max_combo(), 2);
        assert_eq!(c.score().counts(), (1, 1, 1, 1));
        assert!(close(c.score().accuracy().unwrap(), 0.375));
        assert!(close(c.score().mean_signed_error().unwrap(), 60.0));
        assert!(c.all_judged());
        assert_eq!(c.next_note_time(), None);
    }

    /// The fail latch through the core, on the exact case the ruling
    /// names: DRAIN empties HP inside tap() — the hit that resolves in
    /// the same call must not raise HP or score, and neither may any
    /// later call. HPDrainRate 1400 is a stress value: drain over the
    /// first 150 active ms is 150 × 5e-6 × 1400 = 1.05 → HP 1.0 → 0.0
    /// inside the tap's advance; the tap is at exactly A's late
    /// boundary (1150 = 1000 + w50), so it judges Hit50 (rev-A point 3)
    /// — judged, but scoreless.
    #[test]
    fn fail_latches_when_drain_empties_hp_inside_tap() {
        let mut c = core("1400", "4", "5", "100,100,1000,1,0\n200,100,2000,1,0");
        assert_eq!(c.drain().active, vec![(1000.0, 2000.0)]);
        assert!(!c.failed());

        // The same call: drain inside tap's advance empties the bar.
        assert_eq!(
            tap_at(&mut c, 1150.0, 100.0, 100.0),
            Some((0, Judgement::Hit50))
        );
        assert!(c.failed());
        assert_eq!(c.score().hp(), 0.0); // the +0.01 never landed
        assert_eq!(c.score().score(), 0.0); // nor the 50 points
        assert_eq!(c.score().counts(), (0, 0, 1, 0)); // but it did count

        // Any later call: B is judged Hit100, still no HP or score.
        assert_eq!(
            tap_at(&mut c, 2060.0, 200.0, 100.0),
            Some((1, Judgement::Hit100))
        );
        assert!(c.failed());
        assert_eq!(c.score().hp(), 0.0);
        assert_eq!(c.score().score(), 0.0);
        assert_eq!(c.score().counts(), (0, 1, 1, 0));
        assert!(c.all_judged());
    }
}
