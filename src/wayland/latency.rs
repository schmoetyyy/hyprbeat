//! `--latency-test` (§3.6 budget "input → sound → photon ≤ 25 ms @ 60 Hz",
//! §12.3 "tap-to-sound offset report").
//!
//! Every key tap fires a hitsound at the audio counter and a visual flash;
//! the pipeline stamps:
//!
//! * `enqueue_ns` — the main thread receives the key event (start of the
//!   measurement);
//! * `mixed_ns`   — the RT callback actually mixed the sound into an output
//!   buffer (the mixer writes this, see `mixer::RtState::taps`);
//! * `frame_ns`   — the first frame carrying the flash was submitted to the
//!   GPU (approximates the photon leg; vsync/display scan-out is beyond what
//!   an app can measure without a photodiode).
//!
//! The audible time is estimated as `mixed_ns + callback period` — the
//! callback fills the buffer the DAC plays *next* (Q-9 records this
//! measurement limit). Everything here is pure math over recorded stamps
//! (I-09) so the report is unit-tested.
//!
//! Usage contract: live mode + an audio stream + an EMPTY workspace (the
//! surface must hold keyboard focus — D-28 lesson), click the desktop once,
//! then TAP keys. The summary prints at exit.

use crate::audio::mixer::Mixer;

/// Duration of the visual flash after a tap (ms).
pub const FLASH_MS: f64 = 150.0;

/// One recorded tap.
#[derive(Debug, Clone, Copy)]
pub struct Tap {
    pub due: u64,
    /// Latency-test slot the event carried (RT writes its stamp there).
    pub tag: u16,
    /// Nanoseconds since the mixer's time base at key-event time.
    pub enqueue_ns: u64,
    /// First flash frame submitted after the tap (set by the renderer).
    pub frame_ns: Option<u64>,
}

impl Tap {
    pub fn new(due: u64, tag: u16, enqueue_ns: u64) -> Self {
        Self {
            due,
            tag,
            enqueue_ns,
            frame_ns: None,
        }
    }
}

/// Percentile over a non-empty slice (nearest-rank).
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Aggregate report over the recorded taps.
#[derive(Debug, Clone, Copy, Default)]
pub struct LatencyStats {
    pub taps: usize,
    /// tap → mixed into an output buffer (ms).
    pub to_mixed_p50: f64,
    pub to_mixed_p95: f64,
    /// tap → estimated audible (mixed + callback period) (ms).
    pub to_audible_p50: f64,
    pub to_audible_p95: f64,
    /// tap → first flash frame submitted (ms).
    pub to_frame_p50: f64,
    pub to_frame_p95: f64,
}

impl LatencyStats {
    /// Aggregate over recorded taps. `callback_period_ms` comes from the
    /// engine (frames-per-callback / rate) — the mixer deliberately does not
    /// store the rate (the clock owns it).
    pub fn summarize_with_period(taps: &[Tap], mixer: &Mixer, callback_period_ms: f64) -> Self {
        let mut to_mixed = Vec::new();
        let mut to_frame = Vec::new();
        for tap in taps {
            let mixed = mixer.tap_mixed_ns(tap.tag);
            if mixed > tap.enqueue_ns {
                to_mixed.push((mixed - tap.enqueue_ns) as f64 / 1e6);
            }
            if let Some(frame) = tap.frame_ns
                && frame > tap.enqueue_ns
            {
                to_frame.push((frame - tap.enqueue_ns) as f64 / 1e6);
            }
        }
        to_mixed.sort_by(|a, b| a.total_cmp(b));
        to_frame.sort_by(|a, b| a.total_cmp(b));
        let mixed_p50 = percentile(&to_mixed, 0.50);
        let mixed_p95 = percentile(&to_mixed, 0.95);
        Self {
            taps: taps.len(),
            to_mixed_p50: mixed_p50,
            to_mixed_p95: mixed_p95,
            to_audible_p50: mixed_p50 + callback_period_ms,
            to_audible_p95: mixed_p95 + callback_period_ms,
            to_frame_p50: percentile(&to_frame, 0.50),
            to_frame_p95: percentile(&to_frame, 0.95),
        }
    }

    /// §3.6 budget check: input → sound (estimated audible) ≤ 25 ms.
    pub fn audible_within_budget(&self) -> bool {
        self.to_audible_p95 <= 25.0
    }

    pub fn print(&self, callback_period_ms: f64) {
        println!("\n=== hyprbeat latency report (CONTEXT.md §3.6 / §12.3) ===");
        println!("taps recorded               : {}", self.taps);
        println!(
            "callback period (buffer)     : {callback_period_ms:.2} ms (observed at runtime; --buffer tunes)"
        );
        println!(
            "tap  → mixed into buffer     : p50 {:.2} ms   p95 {:.2} ms",
            self.to_mixed_p50, self.to_mixed_p95
        );
        println!(
            "tap  → sound ~audible        : p50 {:.2} ms   p95 {:.2} ms   (mixed + period, Q-9 caveat)",
            self.to_audible_p50, self.to_audible_p95
        );
        println!(
            "tap  → flash frame submitted  : p50 {:.2} ms   p95 {:.2} ms   (photon ≈ +vsync beyond this)",
            self.to_frame_p50, self.to_frame_p95
        );
        println!(
            "budget: input→sound ≤ 25 ms  : {}",
            if self.audible_within_budget() {
                "PASS"
            } else {
                "FAIL — record a waiver or tune the buffer"
            }
        );
        println!("note: no microphone — audible time is estimated as mixed+period (§14 Q-9).");
    }
}

/// Next tap slot (cycling; the mixer keeps `mixer::TAP_SLOTS` stamps).
pub fn next_tag(count: usize) -> u16 {
    (count % crate::audio::mixer::TAP_SLOTS) as u16
}

/// Startup instructions (stdout, like `probe::print_instructions`).
pub fn print_instructions(timeout_s: Option<f64>) {
    let seconds = timeout_s.unwrap_or(f64::INFINITY);
    println!(
        "hyprbeat latency test running for {seconds:.0}s — while it runs:\n  \
         1. switch to an EMPTY workspace (D-28: windows capture input),\n  \
         2. click the desktop once (gives the surface keyboard focus),\n  \
         3. TAP keys in a slow rhythm (10–30 taps).\n\
         Every tap fires a hitsound + flash; the tap→sound→photon report prints at exit."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::clock::SharedClock;
    use crate::audio::mixer::SoundEvent;
    use std::sync::Arc;
    use std::time::Duration;

    fn mixer_silent() -> Mixer {
        // hitsound table with one 1-sample sound
        let snd = crate::audio::mixer::HitSound {
            samples: Arc::from(vec![1.0f32]),
        };
        Mixer::new(Arc::new(SharedClock::new()), None, vec![snd], 1)
    }

    #[test]
    fn summarize_matches_mixer_stamps() {
        let mixer = mixer_silent();
        // fire two events with tags 0/1 at "counter 0"; the mixer stamps
        // mixed_ns = origin.elapsed() — emulate by mixing immediately after
        // enqueue, so to_mixed ≈ sub-ms
        let mut taps = Vec::new();
        for tag in [0u16, 1u16] {
            let enqueue = mixer.origin.elapsed().as_nanos() as u64;
            assert!(mixer.fire(SoundEvent {
                due: 0,
                sound: 0,
                gain: 256,
                tag,
            }));
            let mut buf = [0.0f32; 1];
            mixer.mix_into(&mut buf, 0, 1);
            taps.push(Tap::new(0, tag, enqueue));
        }
        let stats = LatencyStats::summarize_with_period(&taps, &mixer, 10.0);
        assert_eq!(stats.taps, 2);
        // both stamps exist and are tiny (same-thread mix)
        assert!(stats.to_mixed_p50 < 5.0, "p50 {}", stats.to_mixed_p50);
        assert!(stats.to_mixed_p95 < 5.0);
        // audible = mixed + period
        assert!((stats.to_audible_p50 - stats.to_mixed_p50 - 10.0).abs() < 1e-9);
        // frame leg absent until the renderer records it
        assert!(stats.to_frame_p50.is_nan());
        // with a 10 ms period the 25 ms budget passes
        assert!(stats.audible_within_budget());
    }

    #[test]
    fn budget_fails_when_period_is_huge() {
        let mixer = mixer_silent();
        let enqueue = mixer.origin.elapsed().as_nanos() as u64;
        assert!(mixer.fire(SoundEvent {
            due: 0,
            sound: 0,
            gain: 256,
            tag: 0,
        }));
        let mut buf = [0.0f32; 1];
        mixer.mix_into(&mut buf, 0, 1);
        let taps = [Tap::new(0, 0, enqueue)];
        let stats = LatencyStats::summarize_with_period(&taps, &mixer, 30.0);
        assert!(!stats.audible_within_budget(), "30 ms period + mix > 25 ms");
    }

    #[test]
    fn frame_ns_leg_is_recorded() {
        let mixer = mixer_silent();
        let enqueue = mixer.origin.elapsed().as_nanos() as u64;
        let mut tap = Tap::new(0, 0, enqueue);
        std::thread::sleep(Duration::from_micros(100));
        tap.frame_ns = Some(mixer.origin.elapsed().as_nanos() as u64);
        let stats = LatencyStats::summarize_with_period(&[tap], &mixer, 5.0);
        // only the existence of the leg matters; a loaded test machine can
        // stretch a 100 µs sleep arbitrarily, so the bound is generous
        assert!(stats.to_frame_p50 > 0.0);
        assert!(stats.to_frame_p50 < 500.0, "got {}", stats.to_frame_p50);
    }

    #[test]
    fn next_tag_cycles_through_slots() {
        assert_eq!(next_tag(0), 0);
        assert_eq!(next_tag(63), 63);
        assert_eq!(next_tag(64), 0);
    }

    #[test]
    fn percentile_basics() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&v, 0.5), 3.0);
        assert_eq!(percentile(&v, 0.95), 5.0);
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert!(percentile(&[], 0.5).is_nan());
    }
}
