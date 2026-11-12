//! The audio clock — THE core invariant (§3.3, I-01, D-04, D-16).
//!
//! Game time derives **exclusively** from the audio sample counter, never
//! from `Instant::now()` (I-01). The player judges hits by what they *hear*;
//! deriving time from the audio stream makes desync structurally impossible.
//!
//! Two sources exist:
//!
//! * [`ClockSource::Audio`] — the normal case. A `u64` frame counter is
//!   incremented by the cpal output callback (`AtomicU64::fetch_add`, the
//!   only writer), and song time is
//!   `(counter − song_start) / rate`. Pausing freezes the *mapping*, not the
//!   device: the counter keeps advancing, the mixer outputs silence, and
//!   resume rebases `song_start` so time continues exactly where it stopped
//!   ("audio device time never rewinds — the mapping does", §3.3).
//! * [`ClockSource::Wall`] — the explicit degraded fallback (D-16). Used when
//!   the output stream cannot be opened or is lost at runtime, and
//!   deliberately in `--headless`/CI. `Instant::now()`-based. Gameplay is
//!   possible but scores are unranked and the wallpaper must show the
//!   degraded state (I-12; phase-2 stand-in: shader amber band).
//!
//! Everything here is unit-testable with zero audio I/O (I-09): the atomics
//! are plain `std` sync primitives and the wall-clock math is a pure
//! function over `Instant`s.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Which time base the clock is running on (D-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockSource {
    /// cpal output callback sample counter (the only valid gameplay source).
    Audio,
    /// Degraded `Instant::now()` fallback — HUD-visible, scores unranked.
    Wall,
}

/// The atomics shared between the main thread and the real-time callback.
///
/// Write ownership (I-02 posture — atomics only, no locks, no allocation in
/// the callback):
/// * `samples` — written ONLY by the audio callback (`fetch_add` per
///   callback, once, by the frame count).
/// * `song_start`, `paused` — written only by the main thread, read by the
///   callback (single-word atomic loads/stores, `Relaxed`: no other memory is
///   ordered through them).
/// * `stream_failed` — written by the cpal *error* callback (also RT: it
///   stores a single flag and does nothing else); noticed and acted upon by
///   the main thread's tick.
pub struct SharedClock {
    /// Output frames rendered since the stream started (NOT wall samples of
    /// the song — that difference is `song_start`'s job).
    pub samples: AtomicU64,
    /// Value of `samples` when the song's t = 0 (rebasing point). May be
    /// greater than the current counter during a lead-in.
    pub song_start: AtomicU64,
    /// While true the callback keeps counting but the mixer emits silence
    /// and `time_ms` freezes (T3/T4/T6/T7 machinery, §3.4).
    pub paused: AtomicBool,
    /// Set by the cpal error callback; main thread degrades the clock.
    pub stream_failed: AtomicBool,
}

impl SharedClock {
    pub fn new() -> Self {
        Self {
            samples: AtomicU64::new(0),
            song_start: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            stream_failed: AtomicBool::new(false),
        }
    }

    /// Song-relative frame index of an absolute counter value.
    /// `None` before `song_start` (lead-in) — callers treat it as silence.
    pub fn song_frame(&self, counter: u64) -> Option<u64> {
        counter.checked_sub(self.song_start.load(Ordering::Relaxed))
    }
}

impl Default for SharedClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Main-thread facade: gameplay time in ms (I-01 — the ONLY time source).
///
/// All mutation happens on the main thread; the RT callback only ever touches
/// [`SharedClock`]. `Instant::now()` is read here **only** in the degraded
/// wall mode (or before an audio session exists) — never for timing while an
/// audio clock is healthy.
pub struct Clock {
    shared: Arc<SharedClock>,
    /// Output sample (frame) rate of the open stream.
    rate: u32,
    /// Per-map `AudioLeadIn` + universal offset (§3.3). Universal offset lands
    /// in the config in phase 5; phase 2 exposes it as `--offset-ms`.
    offset_ms: f64,
    source: ClockSource,
    // -- wall bookkeeping (ClockSource::Wall; main thread only) --------------
    wall_origin: Instant,
    wall_paused_at: Option<Instant>,
    wall_paused_total: Duration,
    // -- audio pause bookkeeping (main thread only) ---------------------------
    /// Song-relative frame at which the clock froze (`None` = running).
    /// Signed: negative during a lead-in pause.
    pause_frame: Option<i64>,
}

impl Clock {
    /// A healthy audio-clock at `rate`. `song_start` is set to the counter's
    /// current value (song begins "now"); rebasing happens through
    /// [`Clock::begin_song_in`].
    pub fn new_audio(shared: Arc<SharedClock>, rate: u32, offset_ms: f64) -> Self {
        let start = shared.samples.load(Ordering::Relaxed);
        shared.song_start.store(start, Ordering::Relaxed);
        Self {
            shared,
            rate: rate.max(1),
            offset_ms,
            source: ClockSource::Audio,
            wall_origin: Instant::now(),
            wall_paused_at: None,
            wall_paused_total: Duration::ZERO,
            pause_frame: None,
        }
    }

    /// The degraded / headless wall clock (D-16). Time starts at 0 at
    /// construction.
    pub fn new_wall(offset_ms: f64) -> Self {
        Self {
            shared: Arc::new(SharedClock::new()),
            rate: 1,
            offset_ms,
            source: ClockSource::Wall,
            wall_origin: Instant::now(),
            wall_paused_at: None,
            wall_paused_total: Duration::ZERO,
            pause_frame: None,
        }
    }

    pub fn source(&self) -> ClockSource {
        self.source
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn shared(&self) -> &Arc<SharedClock> {
        &self.shared
    }

    /// Offset song t=0 to a future counter value (lead-in), in milliseconds
    /// from "now".
    pub fn begin_song_in(&self, lead_in_ms: f64) {
        let now = self.shared.samples.load(Ordering::Relaxed);
        let lead = (lead_in_ms.max(0.0) * self.rate as f64 / 1000.0).round() as u64;
        self.shared.song_start.store(now + lead, Ordering::Relaxed);
    }

    /// THE time source for all game logic and rendering (I-01).
    ///
    /// Song-relative milliseconds. **Negative during lead-in** (§3.3: the
    /// mapping may put `song_start` in the future). Frozen while paused.
    /// Monotonic across resume. Continuity across a degrade is preserved by
    /// [`Clock::degrade_to_wall`].
    pub fn time_ms(&self) -> f64 {
        match self.source {
            ClockSource::Audio => {
                // i64 math: song_start can be in the future (lead-in) and the
                // pause point can be negative; f64 keeps 2^53 exact — the
                // counter realistically stays below 2^40.
                let counter = self.shared.samples.load(Ordering::Relaxed) as i64;
                let song_start = self.shared.song_start.load(Ordering::Relaxed) as i64;
                let frame = match self.pause_frame {
                    Some(pf) => pf,
                    None => counter.wrapping_sub(song_start),
                };
                frame as f64 / self.rate as f64 * 1000.0 + self.offset_ms
            }
            ClockSource::Wall => {
                wall_time_ms(
                    self.wall_origin,
                    self.wall_paused_total,
                    self.wall_paused_at,
                ) + self.offset_ms
            }
        }
    }

    /// Absolute stream counter right now (audio mode). Cheap atomic load.
    pub fn counter(&self) -> u64 {
        self.shared.samples.load(Ordering::Relaxed)
    }

    /// Freeze the clock and silence the mixer (§3.4 T3/T4). Idempotent.
    pub fn pause(&mut self) {
        if self.is_paused() {
            return;
        }
        match self.source {
            ClockSource::Audio => {
                let counter = self.shared.samples.load(Ordering::Relaxed) as i64;
                let song_start = self.shared.song_start.load(Ordering::Relaxed) as i64;
                self.pause_frame = Some(counter.wrapping_sub(song_start));
                self.shared.paused.store(true, Ordering::Relaxed);
            }
            ClockSource::Wall => {
                self.wall_paused_at = Some(Instant::now());
            }
        }
    }

    /// Un-freeze. Audio mode rebases `song_start` so song time continues
    /// exactly at the pause point while the device counter (which never
    /// rewound) flows on (§3.3). Idempotent.
    pub fn resume(&mut self) {
        if !self.is_paused() {
            return;
        }
        match self.source {
            ClockSource::Audio => {
                if let Some(pf) = self.pause_frame.take() {
                    let counter = self.shared.samples.load(Ordering::Relaxed) as i64;
                    let new_start = counter.wrapping_sub(pf);
                    self.shared
                        .song_start
                        .store(new_start.max(0) as u64, Ordering::Relaxed);
                }
                self.shared.paused.store(false, Ordering::Relaxed);
            }
            ClockSource::Wall => {
                if let Some(at) = self.wall_paused_at.take() {
                    self.wall_paused_total += at.elapsed();
                }
            }
        }
    }

    pub fn is_paused(&self) -> bool {
        match self.source {
            ClockSource::Audio => self.pause_frame.is_some(),
            ClockSource::Wall => self.wall_paused_at.is_some(),
        }
    }

    /// Switch to the degraded wall clock, preserving time continuity (D-16).
    ///
    /// Called by the main thread when the stream fails at runtime. The wall
    /// origin is back-dated so `time_ms` continues from its current value;
    /// any pause is dropped (the audio is gone — silence either way). The
    /// caller must surface the degraded state to the user (I-12).
    pub fn degrade_to_wall(&mut self) {
        if self.source == ClockSource::Wall {
            return;
        }
        let t = self.time_ms();
        self.source = ClockSource::Wall;
        self.pause_frame = None;
        self.shared.paused.store(false, Ordering::Relaxed);
        // origin = now − t, so wall time starts at exactly the current value
        let back = Duration::from_secs_f64((t / 1000.0).max(0.0));
        self.wall_origin = Instant::now() - back;
        self.wall_paused_at = None;
        self.wall_paused_total = Duration::ZERO;
    }

    pub fn is_degraded(&self) -> bool {
        self.source == ClockSource::Wall
    }

    /// Estimated ms of audio currently queued ahead of the DAC, given the
    /// callback's buffer geometry (frames per callback / rate). Used by the
    /// `--latency-test` report — the callback fills the NEXT period, so
    /// audio mixed "now" becomes audible after roughly this long.
    pub fn buffered_ms(frames_per_callback: u64, rate: u32) -> f64 {
        frames_per_callback as f64 / rate.max(1) as f64 * 1000.0
    }
}

/// Pure wall-clock time math (unit-testable, I-09): ms since `origin`,
/// excluding the accumulated paused durations.
pub fn wall_time_ms(origin: Instant, paused_total: Duration, paused_at: Option<Instant>) -> f64 {
    let now = Instant::now();
    let active = match paused_at {
        Some(at) => now.saturating_duration_since(at),
        None => Duration::ZERO,
    };
    let elapsed = now
        .saturating_duration_since(origin)
        .saturating_sub(paused_total)
        .saturating_sub(active);
    elapsed.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulate the RT callback's only write: counter += frames.
    fn advance(shared: &SharedClock, frames: u64) {
        shared.samples.fetch_add(frames, Ordering::Relaxed);
    }

    #[test]
    fn audio_time_is_samples_over_rate() {
        let shared = Arc::new(SharedClock::new());
        let clock = Clock::new_audio(shared.clone(), 48_000, 0.0);
        assert_eq!(clock.time_ms(), 0.0);
        advance(&shared, 48_000);
        assert_eq!(clock.time_ms(), 1000.0);
        advance(&shared, 240);
        assert!((clock.time_ms() - 1005.0).abs() < 1e-9);
    }

    #[test]
    fn offset_shifts_time_not_silence() {
        let shared = Arc::new(SharedClock::new());
        let clock = Clock::new_audio(shared.clone(), 44_100, -15.0);
        advance(&shared, 44_100);
        assert!((clock.time_ms() - 985.0).abs() < 1e-9);
    }

    #[test]
    fn pause_during_lead_in_keeps_the_remaining_lead() {
        let shared = Arc::new(SharedClock::new());
        let mut clock = Clock::new_audio(shared.clone(), 48_000, 0.0);
        clock.begin_song_in(2000.0);
        advance(&shared, 48_000); // 1 s in, 1 s of lead-in left → t = −1000
        clock.pause();
        advance(&shared, 96_000); // 2 s of frozen silence
        assert!(
            (clock.time_ms() + 1000.0).abs() < 1e-9,
            "frozen mid-lead-in"
        );
        clock.resume();
        // still 1000 ms of lead-in to go
        assert!(
            (clock.time_ms() + 1000.0).abs() < 1e-9,
            "resume preserves the remaining lead-in"
        );
        advance(&shared, 48_000);
        assert!(
            (clock.time_ms() - 0.0).abs() < 1e-6,
            "song reaches t=0 exactly after the remaining lead-in"
        );
    }

    #[test]
    fn lead_in_starts_negative() {
        let shared = Arc::new(SharedClock::new());
        let clock = Clock::new_audio(shared.clone(), 48_000, 0.0);
        clock.begin_song_in(2000.0);
        advance(&shared, 48_000);
        assert!(
            (clock.time_ms() + 1000.0).abs() < 1e-6,
            "one second in, still 1 s of lead-in left"
        );
        advance(&shared, 96_000);
        assert!(
            (clock.time_ms() - 1000.0).abs() < 1e-6,
            "lead-in over, song time is 1 s"
        );
    }

    #[test]
    fn pause_freezes_and_resume_continues_exactly() {
        let shared = Arc::new(SharedClock::new());
        let mut clock = Clock::new_audio(shared.clone(), 48_000, 0.0);
        advance(&shared, 48_000); // t = 1000 ms
        clock.pause();
        assert!((clock.time_ms() - 1000.0).abs() < 1e-9);
        // device time flows on while paused (it never rewinds, §3.3)
        advance(&shared, 240_000); // 5 s of paused silence
        assert!((clock.time_ms() - 1000.0).abs() < 1e-9, "frozen");
        clock.resume();
        assert!(
            (clock.time_ms() - 1000.0).abs() < 1e-9,
            "resume continues at the pause point"
        );
        advance(&shared, 48_000);
        assert!(
            (clock.time_ms() - 2000.0).abs() < 1e-9,
            "and keeps flowing after"
        );
    }

    #[test]
    fn pause_is_idempotent_and_double_resume_is_harmless() {
        let shared = Arc::new(SharedClock::new());
        let mut clock = Clock::new_audio(shared.clone(), 48_000, 0.0);
        advance(&shared, 1_000);
        clock.pause();
        let t = clock.time_ms();
        clock.pause();
        assert!((clock.time_ms() - t).abs() < 1e-9);
        clock.resume();
        clock.resume();
        advance(&shared, 1_000);
        assert!(clock.time_ms() > t, "still ticking after double resume");
    }

    #[test]
    fn paused_flag_matches_clock() {
        let shared = Arc::new(SharedClock::new());
        let mut clock = Clock::new_audio(shared.clone(), 48_000, 0.0);
        assert!(!clock.is_paused());
        assert!(!shared.paused.load(Ordering::Relaxed));
        clock.pause();
        assert!(clock.is_paused());
        assert!(shared.paused.load(Ordering::Relaxed), "mixer silenced");
        clock.resume();
        assert!(!shared.paused.load(Ordering::Relaxed));
    }

    #[test]
    fn degrade_preserves_continuity() {
        let shared = Arc::new(SharedClock::new());
        let mut clock = Clock::new_audio(shared.clone(), 48_000, 0.0);
        advance(&shared, 96_000); // t = 2000 ms
        clock.degrade_to_wall();
        assert_eq!(clock.source(), ClockSource::Wall);
        assert!(clock.is_degraded());
        // continuity: wall time starts at (almost exactly) the same value —
        // allow only the sub-ms cost of the degrade itself
        let t = clock.time_ms();
        assert!(t > 1999.0 && t < 2001.0, "continuous, got {t}");
        std::thread::sleep(Duration::from_millis(30));
        let t2 = clock.time_ms();
        assert!(t2 > t && t2 - t < 60.0, "wall keeps ticking after degrade");
    }

    #[test]
    fn wall_clock_pauses() {
        let mut clock = Clock::new_wall(0.0);
        clock.pause();
        let t = clock.time_ms();
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            (clock.time_ms() - t).abs() < 5.0,
            "wall pause must freeze time (jitter allowed for scheduler)"
        );
        clock.resume();
        std::thread::sleep(Duration::from_millis(20));
        assert!(clock.time_ms() > t, "resumed wall clock ticks");
    }

    #[test]
    fn wall_time_ms_math_is_pure() {
        let origin = Instant::now() - Duration::from_secs(10);
        // not paused
        let t = wall_time_ms(origin, Duration::ZERO, None);
        assert!(t > 9990.0 && t < 10010.0, "≈10 s, got {t}");
        // 3 s accumulated pause in the past, currently running
        let t = wall_time_ms(origin, Duration::from_secs(3), None);
        assert!(t > 6990.0 && t < 7010.0, "≈7 s, got {t}");
        // currently paused since 1 s ago
        let paused_at = Some(Instant::now() - Duration::from_secs(1));
        let t = wall_time_ms(origin, Duration::ZERO, paused_at);
        assert!(t > 8990.0 && t < 9010.0, "frozen at ≈9 s, got {t}");
    }

    #[test]
    fn buffered_ms_is_the_callback_period() {
        assert!((Clock::buffered_ms(512, 48_000) - 10.667).abs() < 0.01);
        assert!((Clock::buffered_ms(256, 44_100) - 5.805).abs() < 0.01);
    }

    #[test]
    fn song_frame_handles_lead_in_and_wrap() {
        let shared = SharedClock::new();
        shared.song_start.store(1000, Ordering::Relaxed);
        assert_eq!(shared.song_frame(999), None);
        assert_eq!(shared.song_frame(1000), Some(0));
        assert_eq!(shared.song_frame(1002), Some(2));
    }
}
