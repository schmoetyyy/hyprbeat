//! Combo, score, accuracy and HP bookkeeping — §7.2 + rulings Q-C/Q-D/Q-E
//! (Δ-Q, Δ-S).
//!
//! One method per judgement ([`ScoreState::apply`]); the caller (the
//! game session) guarantees judgements arrive in tap/advance order.
//! All constants are stand-in or wiki-pinned values, unit-tested; the HP
//! constants are hyprbeat's own (Q-E — not wiki-claimed, revisitable in
//! phase 6/8).
//!
//! Fail latches: the moment HP reaches `0.0` — from a miss or from a
//! drain step, including a drain inside `tap()`'s own advance —
//! [`failed`](ScoreState::failed) engages and no later hit can raise
//! HP or score again (combo/counts keep recording the judgements that
//! still arrive; the play wiring stops feeding taps on fail in commit d
//! — Δ-Y — the latch is the safety net).

use super::judge::Judgement;

/// HP gain for a 300 (Δ-Q, Q-E).
pub const HP_GAIN_300: f64 = 0.05;
/// HP gain for a 100 (Δ-Q, Q-E).
pub const HP_GAIN_100: f64 = 0.03;
/// HP gain for a 50 (Δ-Q, Q-E).
pub const HP_GAIN_50: f64 = 0.01;
/// HP loss for a miss (Δ-Q, Q-E).
pub const HP_MISS: f64 = -0.10;
/// Passive HP drain per ms of ACTIVE drain time per point of HPDrainRate
/// (Δ-Q): `5e-6 × HPDrainRate × ms` — HPDrainRate 5 drains a full bar in
/// 40 000 ms of active time.
pub const HP_DRAIN_PER_MS_PER_RATE: f64 = 5.0e-6;

/// Live combo / score / accuracy / HP / hit-error state.
///
/// HP lives on `0.0..=1.0` (bar fraction), clamped on every change. The
/// fail latch engages the moment HP reaches `0.0`; the play wiring
/// (commit d) reads [`failed`](Self::failed) as FAILED (Δ-W).
#[derive(Debug, Clone)]
pub struct ScoreState {
    difficulty_multiplier: u32,
    combo: u32,
    max_combo: u32,
    score: f64,
    n300: u32,
    n100: u32,
    n50: u32,
    nmiss: u32,
    hp: f64,
    /// Fail latch: HP hit zero at some point (miss or drain). Hits
    /// after the latch never raise HP or score.
    failed: bool,
    /// Signed hit errors of HITS in judgement order (misses excluded —
    /// the osu! hit-error bar counts hits only). The exit summary (fix 6)
    /// reads the mean from this; p50/p95 too.
    hit_errors: Vec<f64>,
}

impl ScoreState {
    /// Fresh state for a map with the given difficulty multiplier.
    pub fn new(difficulty_multiplier: u32) -> Self {
        Self {
            difficulty_multiplier,
            combo: 0,
            max_combo: 0,
            score: 0.0,
            n300: 0,
            n100: 0,
            n50: 0,
            nmiss: 0,
            hp: 1.0,
            failed: false,
            hit_errors: Vec::new(),
        }
    }

    /// Apply one judgement. Δ-S: `Δscore = value × (1 + C·M/25)` with
    /// `C = max(combo_before − 1, 0)` (Q-C) and `M` the map's difficulty
    /// multiplier; a miss adds 0 score and breaks the combo.
    ///
    /// Fail latch: after HP has hit zero, hits still count (the counts
    /// are the factual tally) but raise neither score nor HP.
    pub fn apply(&mut self, judgement: Judgement) {
        match judgement {
            Judgement::Hit300 | Judgement::Hit100 | Judgement::Hit50 => {
                if !self.failed {
                    let c = self.combo.saturating_sub(1) as f64;
                    self.score += f64::from(judgement.hit_value())
                        * (1.0 + c * f64::from(self.difficulty_multiplier) / 25.0);
                    self.combo += 1;
                    self.max_combo = self.max_combo.max(self.combo);
                    self.hp = (self.hp + hp_delta(judgement)).clamp(0.0, 1.0);
                }
            }
            Judgement::Miss => {
                self.combo = 0;
                self.hp = (self.hp + HP_MISS).clamp(0.0, 1.0);
            }
        }
        if self.hp <= 0.0 {
            self.failed = true; // a miss emptied the bar
        }
        match judgement {
            Judgement::Hit300 => self.n300 += 1,
            Judgement::Hit100 => self.n100 += 1,
            Judgement::Hit50 => self.n50 += 1,
            Judgement::Miss => self.nmiss += 1,
        }
    }

    /// Passive drain over `active_ms` of ACTIVE drain time (Δ-R windows:
    /// object span minus inferred breaks minus non-circle intervals).
    /// Emptying the bar here latches the fail.
    pub fn drain(&mut self, active_ms: f64, hp_drain_rate: f64) {
        if active_ms <= 0.0 {
            return;
        }
        self.hp = (self.hp - active_ms * HP_DRAIN_PER_MS_PER_RATE * hp_drain_rate).clamp(0.0, 1.0);
        if self.hp <= 0.0 {
            self.failed = true; // the drain emptied the bar
        }
    }

    /// Record a hit's signed time error in ms (hits only, never misses).
    pub fn record_hit_error(&mut self, delta_ms: f64) {
        self.hit_errors.push(delta_ms);
    }

    /// Mean signed hit error in ms (fix 6: the exit summary's calibration
    /// figure) — `None` before the first hit.
    pub fn mean_signed_error(&self) -> Option<f64> {
        if self.hit_errors.is_empty() {
            return None;
        }
        Some(self.hit_errors.iter().sum::<f64>() / self.hit_errors.len() as f64)
    }

    /// Current combo.
    pub fn combo(&self) -> u32 {
        self.combo
    }

    /// Highest combo reached.
    pub fn max_combo(&self) -> u32 {
        self.max_combo
    }

    /// Raw ScoreV1 accumulator (f64, no floor — Δ-S).
    pub fn score(&self) -> f64 {
        self.score
    }

    /// HP bar fraction, `0.0..=1.0`.
    pub fn hp(&self) -> f64 {
        self.hp
    }

    /// Fail latch: HP has hit zero at some point (miss or drain). Hits
    /// after the latch raise neither HP nor score.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// The map's ScoreV1 difficulty multiplier (Q-D).
    pub fn difficulty_multiplier(&self) -> u32 {
        self.difficulty_multiplier
    }

    /// Signed hit errors of hits, in judgement order.
    pub fn hit_errors(&self) -> &[f64] {
        &self.hit_errors
    }

    /// `(n300, n100, n50, nmiss)`.
    pub fn counts(&self) -> (u32, u32, u32, u32) {
        (self.n300, self.n100, self.n50, self.nmiss)
    }

    /// §7.2 accuracy: `Σ(50·n50 + 100·n100 + 300·n300) / (300·Σn)` —
    /// `None` before the first judgement.
    pub fn accuracy(&self) -> Option<f64> {
        let n = self.n300 + self.n100 + self.n50 + self.nmiss;
        if n == 0 {
            return None;
        }
        let num = 50.0 * f64::from(self.n50)
            + 100.0 * f64::from(self.n100)
            + 300.0 * f64::from(self.n300);
        Some(num / (300.0 * f64::from(n)))
    }
}

/// HP delta of a judgement (Δ-Q constants).
fn hp_delta(j: Judgement) -> f64 {
    match j {
        Judgement::Hit300 => HP_GAIN_300,
        Judgement::Hit100 => HP_GAIN_100,
        Judgement::Hit50 => HP_GAIN_50,
        Judgement::Miss => HP_MISS,
    }
}

/// ScoreV1 difficulty multiplier (Δ-S, Q-D — osu! wiki
/// Gameplay/Score/ScoreV1/osu!, opened 2026-11-26):
///
/// ```text
/// M = round((HPDrain + CircleSize + OD + clamp(n/drain_s × 8, 0, 16)) / 38 × 5)
/// ```
///
/// RAW map values — mods never affect it (and phase 4 is nomod); `n`
/// counts ALL objects (a map property, not a circles-only figure);
/// `drain_s` is the all-objects span minus inferred breaks (Δ-R: kept
/// separate from the HP `active` windows, which also exclude
/// slider/spinner stretches). A degenerate `drain_s ≤ 0` with objects
/// present saturates the clamp (16); with no objects the term is 0.
pub fn difficulty_multiplier(
    hp_drain: f64,
    circle_size: f64,
    overall_difficulty: f64,
    object_count: u32,
    drain_time_s: f64,
) -> u32 {
    let objects_term = if drain_time_s > 0.0 {
        (f64::from(object_count) / drain_time_s * 8.0).clamp(0.0, 16.0)
    } else if object_count > 0 {
        16.0
    } else {
        0.0
    };
    let raw = (hp_drain + circle_size + overall_difficulty + objects_term) / 38.0 * 5.0;
    raw.round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Q-D multiplier pins, hand-computed.
    #[test]
    fn difficulty_multiplier_pins() {
        // {HP5, CS4, OD8, 200 objects, drain 100 s}: 200/100×8 = 16
        // (clamped) → (5+4+8+16)/38×5 = 4.342 → 4.
        assert_eq!(difficulty_multiplier(5.0, 4.0, 8.0, 200, 100.0), 4);
        // {HP2, CS3, OD4, 100 objects, drain 60 s}: 100/60×8 ≈ 13.333 →
        // 22.333/38×5 = 2.939 → 3.
        assert_eq!(difficulty_multiplier(2.0, 3.0, 4.0, 100, 60.0), 3);
        // No objects: term 0 → (5+4+8)/38×5 = 2.237 → 2.
        assert_eq!(difficulty_multiplier(5.0, 4.0, 8.0, 0, 0.0), 2);
        // Degenerate drain (all-break map) with objects: term saturates 16.
        assert_eq!(difficulty_multiplier(5.0, 4.0, 8.0, 50, 0.0), 4);
        // Clamp low: 10 objects over 100 s → 0.8 → (0+0+0.8)/38×5 = 0.105 → 0.
        assert_eq!(difficulty_multiplier(0.0, 0.0, 0.0, 10, 100.0), 0);
    }

    /// Δ-S pins at multiplier 4 (rev-C round: 876, not 864 — the 13-hit
    /// driving also pins the combo counter itself).
    #[test]
    fn score_pins_multiplier_4() {
        let mut s = ScoreState::new(4);
        // Three perfect opening hits: C = 0, 0, 1 → 300 + 300 + 348.
        s.apply(Judgement::Hit300);
        assert!(close(s.score(), 300.0));
        s.apply(Judgement::Hit300);
        assert!(close(s.score(), 600.0));
        s.apply(Judgement::Hit300);
        assert!(close(s.score(), 948.0));
        assert_eq!(s.combo(), 3);
        assert_eq!(s.max_combo(), 3);

        // 13 real hits, then the 14th: combo_before 13 → C = 12 →
        // 300×(1+48/25) = 876.
        let mut t = ScoreState::new(4);
        for _ in 0..13 {
            t.apply(Judgement::Hit300);
        }
        assert_eq!(t.combo(), 13);
        let before = t.score();
        t.apply(Judgement::Hit300);
        assert!(close(t.score() - before, 876.0));

        // 100 and 50 at combo_before 13: 292 and 146 (private-field setup
        // keeps both pins at the SAME combo_before).
        let mut u = ScoreState::new(4);
        u.combo = 13;
        u.apply(Judgement::Hit100);
        assert!(close(u.score(), 292.0));
        let mut v = ScoreState::new(4);
        v.combo = 13;
        v.apply(Judgement::Hit50);
        assert!(close(v.score(), 146.0));
    }

    /// Miss resets the combo; the next hit's C is 0 again. Counts:
    /// 6×300 (5 before + 1 after the miss) + 1 miss → (6, 0, 0, 1).
    #[test]
    fn miss_resets_combo() {
        let mut s = ScoreState::new(4);
        for _ in 0..5 {
            s.apply(Judgement::Hit300);
        }
        assert_eq!(s.combo(), 5);
        s.apply(Judgement::Miss);
        assert_eq!(s.combo(), 0);
        let before = s.score();
        s.apply(Judgement::Hit300);
        assert!(close(s.score() - before, 300.0));
        assert_eq!(s.max_combo(), 5);
        assert_eq!(s.counts(), (6, 0, 0, 1));
    }

    /// Δ-Q HP pins, including the rev-C corrected drain cases (the
    /// user's 0.5 → 0.475 / 0.5 → 0.425 pins live here; the DrainPlan
    /// measures behind them are pinned in session.rs).
    #[test]
    fn hp_pins() {
        let mut s = ScoreState::new(4);
        s.hp = 0.5;
        s.apply(Judgement::Hit300);
        assert!(close(s.hp(), 0.55));
        s.apply(Judgement::Hit100);
        assert!(close(s.hp(), 0.58));
        s.apply(Judgement::Hit50);
        assert!(close(s.hp(), 0.59));
        s.apply(Judgement::Miss);
        assert!(close(s.hp(), 0.49));

        let mut d = ScoreState::new(4);
        d.hp = 0.5;
        d.drain(1000.0, 5.0);
        assert!(close(d.hp(), 0.475));
        d.hp = 0.5;
        d.drain(3000.0, 5.0);
        assert!(close(d.hp(), 0.425));
        // 40 000 ms at rate 5 = a full bar from 1.0 (Δ-Q module doc).
        d.hp = 1.0;
        d.drain(40_000.0, 5.0);
        assert_eq!(d.hp(), 0.0);
        // No-op drain at the floor.
        d.drain(0.0, 5.0);
        assert_eq!(d.hp(), 0.0);
    }

    /// Clamps: gains cap at 1.0; the miss floor is 0.0.
    #[test]
    fn hp_clamps() {
        let mut s = ScoreState::new(4);
        s.hp = 0.97;
        s.apply(Judgement::Hit300);
        assert_eq!(s.hp(), 1.0);
        let mut t = ScoreState::new(4);
        t.hp = 0.05;
        t.apply(Judgement::Miss);
        assert_eq!(t.hp(), 0.0);
    }

    /// Fail latch: a miss or a drain that empties the bar latches; hits
    /// afterwards (same call or any later one) raise neither score nor
    /// HP, while still counting. The miss path:
    #[test]
    fn fail_latches_via_miss_then_freezes() {
        let mut s = ScoreState::new(4);
        s.hp = 0.05;
        s.apply(Judgement::Miss); // 0.05 − 0.10 → clamp 0.0
        assert_eq!(s.hp(), 0.0);
        assert!(s.failed());
        let before = s.score();
        s.apply(Judgement::Hit300);
        assert_eq!(s.hp(), 0.0); // not raised
        assert_eq!(s.score(), before); // not raised
        assert_eq!(s.counts(), (1, 0, 0, 1)); // the judgement still counts
    }

    /// Fail latch, drain path: the drain step empties the bar (the
    /// session-level "drain empties HP inside tap()" case pins the same
    /// latch through GameplayCore).
    #[test]
    fn fail_latches_via_drain_then_freezes() {
        let mut d = ScoreState::new(4);
        d.hp = 0.02;
        d.drain(1000.0, 5.0); // 0.025 → clamp 0.0
        assert_eq!(d.hp(), 0.0);
        assert!(d.failed());
        d.apply(Judgement::Hit300);
        assert_eq!(d.hp(), 0.0);
        assert_eq!(d.score(), 0.0);
        assert_eq!(d.counts(), (1, 0, 0, 0));
    }

    /// §7.2 accuracy pin: 6×300 + 2×100 + 1×50 + 1 miss → 2050/3000.
    #[test]
    fn accuracy_pin() {
        let mut s = ScoreState::new(4);
        for _ in 0..6 {
            s.apply(Judgement::Hit300);
        }
        for _ in 0..2 {
            s.apply(Judgement::Hit100);
        }
        s.apply(Judgement::Hit50);
        s.apply(Judgement::Miss);
        assert!(close(s.accuracy().unwrap(), 2050.0 / 3000.0));
        assert_eq!(ScoreState::new(4).accuracy(), None);
    }

    /// Fix 6: mean signed hit error over hits only.
    #[test]
    fn mean_signed_error_pin() {
        let mut s = ScoreState::new(4);
        assert_eq!(s.mean_signed_error(), None);
        s.record_hit_error(-10.0);
        s.record_hit_error(10.0);
        s.record_hit_error(5.0);
        assert!(close(s.mean_signed_error().unwrap(), 5.0 / 3.0));
        assert_eq!(s.hit_errors(), &[-10.0, 10.0, 5.0]);
    }
}
