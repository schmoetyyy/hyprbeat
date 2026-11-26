//! Judgement math — §7.2 formulas as pure functions (rulings Q-A/Q-B).
//!
//! Every constant here is pinned by a hand-computed unit test
//! (design-round fix 1: AR10 fade-in is 300 ms, not 500 — the §7.2
//! piecewise formula, not a guess). Zero I/O (I-09).

/// Judgement-window HALF-widths in ms (§7.2; Q-A ruling: the wiki's
/// window values are half-widths, boundaries inclusive).
///
/// `80 − 6·OD` / `140 − 8·OD` / `200 − 10·OD`: OD10 → 20/60/100,
/// OD5 → 50/100/150, OD0 → 80/140/200.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Windows {
    pub hit300: f64,
    pub hit100: f64,
    pub hit50: f64,
}

impl Windows {
    pub fn from_od(overall_difficulty: f64) -> Self {
        Self {
            hit300: 80.0 - 6.0 * overall_difficulty,
            hit100: 140.0 - 8.0 * overall_difficulty,
            hit50: 200.0 - 10.0 * overall_difficulty,
        }
    }
}

/// A circle judgement (§7.1: 300/100/50/miss by time delta).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judgement {
    Hit300,
    Hit100,
    Hit50,
    Miss,
}

impl Judgement {
    /// Base score value (§7.2).
    pub fn hit_value(self) -> u32 {
        match self {
            Judgement::Hit300 => 300,
            Judgement::Hit100 => 100,
            Judgement::Hit50 => 50,
            Judgement::Miss => 0,
        }
    }
}

/// Judge a tap by its signed time delta. Inclusive boundaries (Q-A):
/// `|Δ| ≤ w300` → 300, else `|Δ| ≤ w100` → 100, else `|Δ| ≤ w50` → 50,
/// else `None` — the tap judges nothing and is ignored (Δ-P: no ghost
/// miss; misses fire only at deadlines via
/// [`GameplayCore::advance`](crate::game::session::GameplayCore)).
pub fn judge_delta(delta_ms: f64, windows: &Windows) -> Option<Judgement> {
    let d = delta_ms.abs();
    if d <= windows.hit300 {
        Some(Judgement::Hit300)
    } else if d <= windows.hit100 {
        Some(Judgement::Hit100)
    } else if d <= windows.hit50 {
        Some(Judgement::Hit50)
    } else {
        None
    }
}

/// Circle radius in osu!px (§7.2: `54.4 − 4.48·CS`, playfield 512×384).
/// A tap inside the radius (inclusive, squared-distance compare) is in
/// range.
pub fn circle_radius_osupx(circle_size: f64) -> f64 {
    54.4 - 4.48 * circle_size
}

/// Approach preempt in ms (§7.2): how long before its hit time a circle
/// starts being visible.
///
/// `AR < 5: 1200 + 600·(5−AR)/5` else `1200 − 750·(AR−5)/5`
/// (AR10 → 450, AR5 → 1200, AR0 → 1800, AR11 → 300).
pub fn preempt_ms(approach_rate: f64) -> f64 {
    if approach_rate < 5.0 {
        1200.0 + 600.0 * (5.0 - approach_rate) / 5.0
    } else {
        1200.0 - 750.0 * (approach_rate - 5.0) / 5.0
    }
}

/// Circle fade-in duration in ms (§7.2): `AR < 5: 800 + 400·(5−AR)/5`
/// else `800 − 500·(AR−5)/5` — AR10 → **300**, not 500 (design-round
/// fix 1; the render commit uses this for opacity ramping).
pub fn fade_in_ms(approach_rate: f64) -> f64 {
    if approach_rate < 5.0 {
        800.0 + 400.0 * (5.0 - approach_rate) / 5.0
    } else {
        800.0 - 500.0 * (approach_rate - 5.0) / 5.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// §7.2 window pins, hand-computed (OD10/OD5/OD0).
    #[test]
    fn windows_from_od_pins() {
        let od10 = Windows::from_od(10.0);
        assert_eq!((od10.hit300, od10.hit100, od10.hit50), (20.0, 60.0, 100.0));
        let od5 = Windows::from_od(5.0);
        assert_eq!((od5.hit300, od5.hit100, od5.hit50), (50.0, 100.0, 150.0));
        let od0 = Windows::from_od(0.0);
        assert_eq!((od0.hit300, od0.hit100, od0.hit50), (80.0, 140.0, 200.0));
    }

    /// Q-A inclusive boundaries at exact representable edges (OD10:
    /// 20/60/100). Everything past ±w50 judges nothing (Δ-P: ignored
    /// taps, not ghost misses).
    #[test]
    fn boundaries_are_inclusive() {
        let w = Windows::from_od(10.0);
        assert_eq!(judge_delta(0.0, &w), Some(Judgement::Hit300));
        assert_eq!(judge_delta(20.0, &w), Some(Judgement::Hit300));
        assert_eq!(judge_delta(20.0001, &w), Some(Judgement::Hit100));
        assert_eq!(judge_delta(60.0, &w), Some(Judgement::Hit100));
        assert_eq!(judge_delta(60.0001, &w), Some(Judgement::Hit50));
        assert_eq!(judge_delta(100.0, &w), Some(Judgement::Hit50));
        assert_eq!(judge_delta(100.0001, &w), None);
        // Signed: late and early symmetric.
        assert_eq!(judge_delta(-20.0, &w), Some(Judgement::Hit300));
        assert_eq!(judge_delta(-100.0, &w), Some(Judgement::Hit50));
        assert_eq!(judge_delta(-100.0001, &w), None);
    }

    /// §7.2 radius pins: CS0 → 54.4, CS4 → 36.48, CS7 → 23.04.
    #[test]
    fn circle_radius_pins() {
        assert!(close(circle_radius_osupx(0.0), 54.4));
        assert!(close(circle_radius_osupx(4.0), 36.48));
        assert!(close(circle_radius_osupx(7.0), 23.04));
    }

    /// §7.2 approach pins (fix 1: AR10 fade-in = 300 ms). A circle is
    /// fully opaque `fade_in` after it first appears: AR10 → visible at
    /// t−450, opaque at t−150; AR5 → t−1200/t−400; AR0 → t−1800/t−600;
    /// AR11 → t−300/t−100.
    #[test]
    fn approach_rate_pins() {
        assert_eq!(preempt_ms(10.0), 450.0);
        assert_eq!(fade_in_ms(10.0), 300.0);
        assert_eq!(preempt_ms(5.0), 1200.0);
        assert_eq!(fade_in_ms(5.0), 800.0);
        assert_eq!(preempt_ms(0.0), 1800.0);
        assert_eq!(fade_in_ms(0.0), 1200.0);
        assert_eq!(preempt_ms(11.0), 300.0);
        assert_eq!(fade_in_ms(11.0), 200.0);

        // Opaque offset = preempt − fade (the render commit's pin).
        assert_eq!(preempt_ms(10.0) - fade_in_ms(10.0), 150.0);
        assert_eq!(preempt_ms(5.0) - fade_in_ms(5.0), 400.0);
        assert_eq!(preempt_ms(0.0) - fade_in_ms(0.0), 600.0);
        assert_eq!(preempt_ms(11.0) - fade_in_ms(11.0), 100.0);
    }
}
