//! Real-time mixer (§3.2, §5 `audio/mixer.rs`, invariant I-02 / D-31).
//!
//! Mixes music + hitsound voices into the cpal output buffer, increments the
//! global sample counter, and publishes the audio envelope the renderer
//! pulses to (the visible "wallpaper synced to music" of phase 2's exit
//! criteria).
//!
//! # RT contract (I-02, made concrete in D-31)
//!
//! The cpal output callback may only:
//! * read/`fetch_add` **single-word atomics** (`SharedClock`, volume, …),
//! * write into the caller-provided output buffer,
//! * touch [`RtState`] (voices + tap log) — `UnsafeCell` state owned
//!   exclusively by the one callback thread of the one stream,
//! * pop from the SPSC [`SpscQueue`] (its slot protocol provides the
//!   producer/consumer mutual exclusion; no locks anywhere).
//!
//! It must **never** allocate, lock, log (`tracing` allocates!), panic
//! (all indexing is bounds-checked `get`-style arithmetic, never slicing) or
//! call anything that could. Diagnostics are counted in atomics and read by
//! the main thread's tick.
//!
//! The mixing math itself is factored so it is unit-testable with zero audio
//! I/O (I-09): tests drive [`Mixer::mix_into`] directly with hand-set counter
//! values.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use super::clock::SharedClock;

/// Voices playing simultaneously before events start getting dropped
/// (counted, surfaced in diagnostics). 8 overlapping hitsounds is already
/// far beyond anything a rhythm game needs.
pub const MAX_VOICES: usize = 8;
/// Latency-test tap slots (tag ids cycle through these; see `SoundEvent`).
pub const TAP_SLOTS: usize = 64;

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// Fully-decoded music, interleaved f32, already resampled to the STREAM rate
/// (resampling happens in the decode worker, never here — §4).
pub struct Music {
    pub samples: Arc<[f32]>,
    pub channels: u32,
}

impl Music {
    pub fn frames(&self) -> u64 {
        self.samples.len() as u64 / self.channels.max(1) as u64
    }
}

/// A decoded hitsound (mono, stream rate).
pub struct HitSound {
    pub samples: Arc<[f32]>,
}

impl HitSound {
    pub fn frames(&self) -> u64 {
        self.samples.len() as u64
    }
}

/// A sound trigger, produced on the main thread, consumed by the callback.
///
/// `due` is an absolute counter value (frame index since stream start);
/// `tag` is the latency-test slot to stamp on activation (0xFF = none).
#[derive(Debug, Clone, Copy)]
pub struct SoundEvent {
    pub due: u64,
    /// Index into the mixer's sound table.
    pub sound: u8,
    /// 8.8 fixed-point gain (256 = 1.0).
    pub gain: u16,
    /// Latency-test tap slot, or `TAP_SLOTS`-max sentinel `u16::MAX`.
    pub tag: u16,
}

impl SoundEvent {
    /// A plain "just play it" event.
    pub fn now(due: u64, sound: u8, gain: f32) -> Self {
        Self {
            due,
            sound,
            gain: (gain.clamp(0.0, 4.0) * 256.0).round() as u16,
            tag: u16::MAX,
        }
    }
}

// ---------------------------------------------------------------------------
// SPSC queue (lock-free, allocation-free after construction)
// ---------------------------------------------------------------------------

/// Bounded single-producer single-consumer queue of `Copy` values.
///
/// Slot protocol: indices are monotonically increasing `usize` (slot =
/// `index % N`). Producer exclusively owns `tail` + the half-open slot range
/// `[head, tail)`; consumer exclusively owns `head`. Acquire/Release pairing
/// on the hand-off word publishes the slot contents. The mixer's `&self`
/// sharing across threads is sound *because* of this protocol plus the
/// single-callback-thread ownership of [`RtState`] (see [`Mixer`]).
pub struct SpscQueue<T: Copy, const N: usize> {
    head: AtomicUsize,
    tail: AtomicUsize,
    buf: UnsafeCell<Box<[std::mem::MaybeUninit<T>; N]>>,
}

impl<T: Copy, const N: usize> SpscQueue<T, N> {
    pub fn new() -> Self {
        // MaybeUninit arrays are awkward to construct; zeroed is fine for
        // Copy payloads (they are always written before being read).
        let buf = Box::new(unsafe { std::mem::zeroed() });
        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            buf: UnsafeCell::new(buf),
        }
    }

    /// Producer side. Returns `false` if full (caller counts the drop).
    pub fn push(&self, value: T) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= N {
            return false; // full
        }
        unsafe {
            (*self.buf.get())[tail % N].write(value);
        }
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// Consumer side, non-destructive look at the oldest element.
    pub fn peek(&self) -> Option<T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None; // empty
        }
        unsafe { Some((*self.buf.get())[head % N].assume_init_read()) }
    }

    /// Consumer side; pairs with a preceding `peek`.
    pub fn pop(&self) -> Option<T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let value = unsafe { (*self.buf.get())[head % N].assume_init_read() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(value)
    }

    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Relaxed);
        tail.wrapping_sub(head)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T: Copy, const N: usize> Default for SpscQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: the queue is SPSC by construction; see the protocol comment on the
// type. Soundness relies on exactly one producer (`push`) and one consumer
// (`peek`/`pop`) — both enforced by usage inside this module (main thread
// pushes, the single cpal callback thread peeks/pops).
unsafe impl<T: Copy + Send, const N: usize> Sync for SpscQueue<T, N> {}

// ---------------------------------------------------------------------------
// RT-private state
// ---------------------------------------------------------------------------

/// One playing hitsound voice. `sound == u8::MAX` marks the slot free.
#[derive(Clone, Copy)]
struct Voice {
    sound: u8,
    /// Absolute counter frame at which this voice started.
    start: u64,
    gain: u16,
}

/// State owned exclusively by the audio callback thread (`UnsafeCell` inside
/// [`Mixer`]).
struct RtState {
    voices: [Voice; MAX_VOICES],
    taps: [TapRecord; TAP_SLOTS],
}

/// Latency-test record for one tap slot: the RT side stamps when the sound
/// was actually mixed into an output buffer.
struct TapRecord {
    mixed_ns: AtomicU64,
}

// ---------------------------------------------------------------------------
// Mixer
// ---------------------------------------------------------------------------

pub struct Mixer {
    pub shared: Arc<SharedClock>,
    music: Option<Music>,
    sounds: Vec<HitSound>,
    out_channels: u32,
    /// Fixed-point 16.16 master volume.
    volume: AtomicU32,
    /// Fixed-point 16.16 output envelope (0..=1), refreshed per callback.
    envelope: AtomicU32,
    /// Frames in the last observed callback (buffer geometry for the
    /// `--latency-test` audible-time estimate).
    frames_per_callback: AtomicU32,
    finished: AtomicBool,
    events_dropped: AtomicU32,
    voices_exhausted: AtomicU32,
    clipped: AtomicU32,
    queue: SpscQueue<SoundEvent, 64>,
    rt: UnsafeCell<RtState>,
    /// Time base for latency-test nanosecond stamps (both threads use it).
    pub origin: Instant,
}

// SAFETY (the whole point of I-02/D-31): `Mixer` is shared as `Arc` between
// the main thread and the single cpal callback thread of the single stream.
// * atomics — self-explanatory;
// * `queue` — SPSC protocol (main pushes, callback peeks/pops);
// * `rt` — touched ONLY by the callback thread (mix_into is the sole
//   accessor; unit tests call it single-threaded).
// There is no lock, no allocation and no logging on that path.
unsafe impl Send for Mixer {}
unsafe impl Sync for Mixer {}

impl Mixer {
    /// Build a mixer for a stream with `out_channels` at `rate`. `music` may
    /// be `None` (hitsound-only stream — the `--latency-test` no-song case).
    pub fn new(
        shared: Arc<SharedClock>,
        music: Option<Music>,
        sounds: Vec<HitSound>,
        out_channels: u32,
    ) -> Self {
        // no music ⇒ nothing to finish; with music the latch is set at the
        // song's tail (mix loop step 4)
        let finished = music.is_none();
        Self {
            shared,
            music,
            sounds,
            out_channels: out_channels.clamp(1, 2),
            volume: AtomicU32::new(1 << 16),
            envelope: AtomicU32::new(0),
            frames_per_callback: AtomicU32::new(0),
            finished: AtomicBool::new(finished),
            events_dropped: AtomicU32::new(0),
            voices_exhausted: AtomicU32::new(0),
            clipped: AtomicU32::new(0),
            queue: SpscQueue::new(),
            rt: UnsafeCell::new(RtState {
                voices: [Voice {
                    sound: u8::MAX,
                    start: 0,
                    gain: 0,
                }; MAX_VOICES],
                taps: std::array::from_fn(|_| TapRecord {
                    mixed_ns: AtomicU64::new(0),
                }),
            }),
            origin: Instant::now(),
        }
    }

    // -- main-thread API ------------------------------------------------------

    /// Queue a hitsound at absolute counter `due` (I-02: this is the ONLY
    /// producer call that crosses into the RT side).
    pub fn fire(&self, event: SoundEvent) -> bool {
        if !self.queue.push(event) {
            self.events_dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Master volume (main thread; the callback converts once per callback).
    pub fn set_volume(&self, volume: f32) {
        let v = (volume.clamp(0.0, 1.0) * 65_536.0).round() as u32;
        self.volume.store(v, Ordering::Relaxed);
    }

    /// Current output envelope 0..1 (post-volume RMS of the last callback).
    pub fn envelope(&self) -> f32 {
        (self.envelope.load(Ordering::Relaxed) as f32 / 65_536.0).clamp(0.0, 1.0)
    }

    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    /// Frames per output callback as last observed (0 before the first).
    pub fn frames_per_callback(&self) -> u32 {
        self.frames_per_callback.load(Ordering::Relaxed)
    }

    /// (queue-full drops, voice-exhaustion drops, clipped frame count)
    pub fn diagnostics(&self) -> (u32, u32, u32) {
        (
            self.events_dropped.load(Ordering::Relaxed),
            self.voices_exhausted.load(Ordering::Relaxed),
            self.clipped.load(Ordering::Relaxed),
        )
    }

    /// Read the RT stamp of a latency-test tap (0 = not mixed yet).
    pub fn tap_mixed_ns(&self, tag: u16) -> u64 {
        let rt = unsafe { &*self.rt.get() };
        rt.taps[tag as usize % TAP_SLOTS]
            .mixed_ns
            .load(Ordering::Relaxed)
    }

    // -- RT side (the cpal callback body) -------------------------------------

    /// Mix `frames` interleaved frames starting at absolute counter `start`
    /// into `out`, then return the post-mix RMS envelope (0..1).
    ///
    /// The cpal callback calls this and then `fetch_add`s the counter; tests
    /// call it directly with hand-crafted counters (I-09).
    pub fn mix_into(&self, out: &mut [f32], start: u64, frames: u64) -> f64 {
        let rt = unsafe { &mut *self.rt.get() };
        self.frames_per_callback
            .store(frames as u32, Ordering::Relaxed);

        let och = self.out_channels as usize;
        let paused = self.shared.paused.load(Ordering::Relaxed);
        let song_start = self.shared.song_start.load(Ordering::Relaxed);
        let volume = self.volume.load(Ordering::Relaxed) as f32 / 65_536.0;

        // 1) activate every event due before the end of this callback
        let end = start.wrapping_add(frames);
        while let Some(ev) = self.queue.peek() {
            if ev.due >= end {
                break;
            }
            let _ = self.queue.pop();
            self.activate(rt, ev, start);
        }

        // 2) music geometry (None → silence, finished immediately)
        let music = self.music.as_ref();
        let (music_samples, music_channels) = match music {
            Some(m) => (&m.samples[..], m.channels.max(1) as usize),
            None => (&[][..], 1usize),
        };

        // 3) the mix loop — bounds-checked everywhere, never panicking
        let mut sumsq = 0.0f64;
        let mut clipped = 0u32;
        let mut frame = frame_buf();
        for f in 0..frames {
            let abs = start.wrapping_add(f);
            for v in frame.iter_mut() {
                *v = 0.0;
            }

            if !paused && let Some(song_frame) = abs.checked_sub(song_start) {
                let idx = song_frame as usize * music_channels;
                if idx < music_samples.len() {
                    // channel map: duplicate mono, truncate stereo→mono
                    for (c, slot) in frame.iter_mut().take(och).enumerate() {
                        let src = c.min(music_channels - 1);
                        *slot += music_samples[idx + src];
                    }
                }
            }

            for voice in rt.voices.iter_mut() {
                if voice.sound == u8::MAX || abs < voice.start {
                    continue;
                }
                let snd = match self.sounds.get(voice.sound as usize) {
                    Some(s) => &s.samples[..],
                    None => {
                        voice.sound = u8::MAX;
                        continue;
                    }
                };
                let rel = abs - voice.start;
                let idx = rel as usize;
                if idx >= snd.len() {
                    voice.sound = u8::MAX; // played out
                    continue;
                }
                let gain = voice.gain as f32 / 256.0;
                let sample = snd[idx] * gain;
                for slot in frame.iter_mut().take(och) {
                    *slot += sample; // mono sources on every channel
                }
            }

            let base = (f as usize) * och;
            if base + och > out.len() {
                break; // malformed buffer: keep what fit, never panic
            }
            for (c, slot) in frame.iter().take(och).enumerate() {
                let s = slot * volume;
                if s.abs() > 1.0 {
                    clipped += 1;
                }
                out[base + c] = s;
                sumsq += (s * s) as f64;
            }
        }

        // 4) song-finished latch (idempotent, cheap)
        if let Some(m) = music
            && !paused
            && end >= song_start.wrapping_add(m.frames())
        {
            self.finished.store(true, Ordering::Relaxed);
        }

        self.clipped.fetch_add(clipped, Ordering::Relaxed);

        let rms = if frames > 0 && och > 0 {
            (sumsq / (frames as f64 * och as f64)).sqrt()
        } else {
            0.0
        };
        let env = (rms.clamp(0.0, 1.0) * 65_536.0).round() as u32;
        self.envelope.store(env, Ordering::Relaxed);
        rms
    }

    /// Claim a voice slot for an event; stamp its tap if it has one.
    fn activate(&self, rt: &mut RtState, ev: SoundEvent, callback_start: u64) {
        let Some(slot) = rt.voices.iter_mut().find(|v| v.sound == u8::MAX) else {
            self.voices_exhausted.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // a late event (due before this callback) fires at its start
        slot.sound = ev.sound;
        slot.start = ev.due.max(callback_start);
        slot.gain = ev.gain;
        if ev.tag != u16::MAX {
            let ns = self.origin.elapsed().as_nanos().min(u64::MAX as u128) as u64;
            rt.taps[ev.tag as usize % TAP_SLOTS]
                .mixed_ns
                .store(ns, Ordering::Relaxed);
        }
    }
}

/// Scratch frame accumulator — on the stack, fixed size 2 (the only output
/// geometries we open are mono/stereo; larger channel counts truncate).
fn frame_buf() -> [f32; 2] {
    [0.0f32; 2]
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn music_const(len_frames: u64, value: f32, channels: u32) -> Music {
        Music {
            samples: Arc::from(vec![value; (len_frames * channels as u64) as usize]),
            channels,
        }
    }

    fn sound_ramp(len: usize) -> HitSound {
        HitSound {
            samples: Arc::from((0..len).map(|i| i as f32 / len as f32).collect::<Vec<_>>()),
        }
    }

    fn mixer_with(music: Option<Music>, channels: u32) -> (Arc<SharedClock>, Mixer) {
        let shared = Arc::new(SharedClock::new());
        let m = Mixer::new(shared.clone(), music, vec![sound_ramp(4)], channels);
        (shared, m)
    }

    fn advance(shared: &SharedClock, frames: u64) {
        shared.samples.fetch_add(frames, Ordering::Relaxed);
    }

    #[test]
    fn music_passthrough_stereo_identity() {
        let (shared, m) = mixer_with(Some(music_const(4, 0.5, 2)), 2);
        shared.song_start.store(0, Ordering::Relaxed);
        let mut out = vec![0.0f32; 8]; // 4 frames × 2 ch
        let rms = m.mix_into(&mut out, 0, 4);
        assert!(
            out.iter().all(|&s| s == 0.5),
            "full-scale constant passes through at gain 1"
        );
        assert!((rms - 0.5).abs() < 1e-9);
        advance(&shared, 4);
        assert_eq!(shared.samples.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn volume_scales_output() {
        let (shared, m) = mixer_with(Some(music_const(2, 0.5, 1)), 1);
        shared.song_start.store(0, Ordering::Relaxed);
        m.set_volume(0.25);
        let mut out = vec![0.0f32; 2];
        m.mix_into(&mut out, 0, 2);
        assert!(out.iter().all(|&s| (s - 0.125).abs() < 1e-6));
    }

    #[test]
    fn pause_silences_but_counter_flows() {
        let (shared, m) = mixer_with(Some(music_const(8, 0.5, 1)), 1);
        shared.song_start.store(0, Ordering::Relaxed);
        shared.paused.store(true, Ordering::Relaxed);
        let mut out = vec![0.0f32; 4];
        let rms = m.mix_into(&mut out, 0, 4);
        assert!(out.iter().all(|&s| s == 0.0), "silence while paused");
        assert_eq!(rms, 0.0);
        assert_eq!(m.envelope(), 0.0);
    }

    #[test]
    fn lead_in_then_tail_silence() {
        let (shared, m) = mixer_with(Some(music_const(2, 0.5, 1)), 1);
        shared.song_start.store(2, Ordering::Relaxed); // 2 frames lead-in
        let mut out = vec![0.0f32; 4];
        m.mix_into(&mut out, 0, 4);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
        assert_eq!(out[2], 0.5);
        assert_eq!(out[3], 0.5, "both music frames played");
        // past the end: silence again + finished latch
        let mut out2 = vec![0.0f32; 2];
        m.mix_into(&mut out2, 4, 2);
        assert!(out2.iter().all(|&s| s == 0.0));
        assert!(m.finished(), "finished latched after tail");
    }

    #[test]
    fn mono_music_duplicated_into_stereo() {
        let (shared, m) = mixer_with(Some(music_const(2, 0.25, 1)), 2);
        shared.song_start.store(0, Ordering::Relaxed);
        let mut out = vec![0.0f32; 4];
        m.mix_into(&mut out, 0, 2);
        assert_eq!(out, vec![0.25, 0.25, 0.25, 0.25]);
    }

    #[test]
    fn sound_event_is_sample_accurate() {
        let (_shared, m) = mixer_with(None, 1);
        // ramp of 4 samples: 0.0, 0.25, 0.5, 0.75
        assert!(m.fire(SoundEvent {
            due: 2,
            sound: 0,
            gain: 256, // 1.0
            tag: u16::MAX,
        }));
        let mut out = vec![0.0f32; 4];
        m.mix_into(&mut out, 0, 4);
        // frames 0,1: nothing; frame 2: sound sample 0; frame 3: sample 1
        assert_eq!(out, vec![0.0, 0.0, 0.0, 0.25]);
    }

    #[test]
    fn late_event_fires_immediately_at_callback_start() {
        let (_shared, m) = mixer_with(None, 1);
        assert!(m.fire(SoundEvent {
            due: 1, // missed: callback covers 10..12
            sound: 0,
            gain: 256,
            tag: u16::MAX,
        }));
        let mut out = vec![0.0f32; 2];
        m.mix_into(&mut out, 10, 2);
        assert_eq!(
            out,
            vec![0.0, 0.25],
            "starts at callback start with sound sample 0"
        );
    }

    #[test]
    fn future_event_survives_earlier_callbacks() {
        let (_shared, m) = mixer_with(None, 1);
        assert!(m.fire(SoundEvent::now(12, 0, 1.0)));
        let mut out = vec![0.0f32; 2];
        m.mix_into(&mut out, 0, 2); // covers 0..2 — event not due yet
        assert!(out.iter().all(|&s| s == 0.0));
        assert!(!m.queue.is_empty());
        let mut out = vec![0.0f32; 4];
        m.mix_into(&mut out, 10, 4); // covers 10..14 — fires at 12
        assert_eq!(out[0], 0.0); // frame 10
        assert_eq!(out[1], 0.0); // frame 11
        assert_eq!(out[2], 0.0); // frame 12 → sound sample 0
        assert_eq!(out[3], 0.25); // frame 13 → sound sample 1
    }

    #[test]
    fn voice_exhaustion_is_counted_not_dropped_silently() {
        let (_shared, m) = mixer_with(None, 1);
        for i in 0..(MAX_VOICES + 2) {
            assert!(
                m.fire(SoundEvent::now(0, 0, 1.0)),
                "queue big enough (event {i})"
            );
        }
        let mut out = vec![0.0f32; 1];
        m.mix_into(&mut out, 0, 1);
        let (_, voices_dropped, _) = m.diagnostics();
        assert_eq!(voices_dropped, 2, "8 voices max, 10 events → 2 dropped");
    }

    #[test]
    fn queue_overflow_is_counted() {
        let (_shared, m) = mixer_with(None, 1);
        let mut pushed = 0;
        for i in 0..100 {
            if m.fire(SoundEvent::now(1_000_000 + i as u64, 0, 1.0)) {
                pushed += 1;
            }
        }
        assert_eq!(pushed, 64, "queue capacity");
        assert_eq!(m.diagnostics().0, 36, "overflow drops counted");
    }

    #[test]
    fn envelope_tracks_loudness_and_is_clamped() {
        let (shared, m) = mixer_with(Some(music_const(2, 2.0, 1)), 1); // over-full-scale
        shared.song_start.store(0, Ordering::Relaxed);
        let mut out = vec![0.0f32; 2];
        m.mix_into(&mut out, 0, 2);
        assert_eq!(m.envelope(), 1.0, "clamped to 1");
        assert!(m.diagnostics().2 >= 2, "clipping counted");
    }

    #[test]
    fn taps_are_stamped_on_activation() {
        let (_shared, m) = mixer_with(None, 1);
        assert!(m.fire(SoundEvent {
            due: 1,
            sound: 0,
            gain: 256,
            tag: 3,
        }));
        assert_eq!(m.tap_mixed_ns(3), 0, "pending");
        let mut out = vec![0.0f32; 2];
        m.mix_into(&mut out, 0, 2);
        assert!(m.tap_mixed_ns(3) > 0, "stamped when mixed");
        assert_eq!(m.tap_mixed_ns(9), 0, "other slots untouched");
    }

    #[test]
    fn spsc_fifo_and_bounds() {
        let q: SpscQueue<u32, 4> = SpscQueue::new();
        assert!(q.is_empty());
        assert!(q.peek().is_none());
        assert!(q.pop().is_none());
        assert!(q.push(1));
        assert!(q.push(2));
        assert!(q.push(3));
        assert!(q.push(4));
        assert!(!q.push(5), "full");
        assert_eq!(q.len(), 4);
        assert_eq!(q.peek(), Some(1));
        assert_eq!(q.pop(), Some(1));
        assert!(q.push(5), "space again");
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), Some(3));
        assert_eq!(q.pop(), Some(4));
        assert_eq!(q.pop(), Some(5));
        assert!(q.is_empty());
    }

    #[test]
    fn spsc_wraps_after_capacity() {
        let q: SpscQueue<u8, 2> = SpscQueue::new();
        for round in 0u8..16 {
            assert!(q.push(round));
            assert!(q.push(round));
            assert_eq!(q.pop(), Some(round));
            assert_eq!(q.pop(), Some(round));
        }
        assert!(q.is_empty());
    }

    #[test]
    fn spsc_concurrent_hammer() {
        let q: &'static SpscQueue<u64, 128> = Box::leak(Box::new(SpscQueue::new()));
        let producer = std::thread::spawn(move || {
            for i in 0..50_000u64 {
                while !q.push(i) {
                    std::hint::spin_loop();
                }
            }
        });
        let consumer = std::thread::spawn(move || {
            let mut expect = 0u64;
            let mut got = 0;
            while got < 50_000 {
                if let Some(v) = q.pop() {
                    assert_eq!(v, expect, "ordering must hold for SPSC");
                    expect += 1;
                    got += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        producer.join().unwrap();
        consumer.join().unwrap();
    }

    #[test]
    fn music_without_sound_table_is_silent_for_bad_index() {
        // sounds table with one entry; event pointing at a missing index →
        // voice is freed, no panic
        let (_shared, m) = mixer_with(None, 1);
        assert!(m.fire(SoundEvent::now(0, 9, 1.0)));
        let mut out = vec![0.0f32; 1];
        m.mix_into(&mut out, 0, 1);
        assert_eq!(out[0], 0.0);
    }

    #[test]
    fn variable_callback_sizes_render_bit_identical() {
        // C.3 (2026-11-16): the mixer must render the SAME samples for the
        // SAME event schedule regardless of how the host slices the
        // timeline into callbacks. Positions inside mix_into are absolute
        // (music reads the counter minus song_start; a voice starts at
        // max(due, callback_start)), so chunking must be invisible. This
        // test is the cheap correctness pin, NOT the crackle suspect —
        // the 2026-11 static investigation traced that to the host /
        // capture-tool side (D-37 amendment).
        //
        // Geometry under test: one continuous 1000-frame callback vs the
        // two real-world patterns observed on the dev machine —
        // alternating 117/118 (44.1 kHz mp3, fpc ≈ N/4; 60-frame tail)
        // and alternating 64/128 (quantum 64; 40-frame tail). Event dues
        // sit ON chunk boundaries of both patterns (0/64/117/235/470/940)
        // and mid-chunk (384/700) to pin the activation edge: an event
        // due exactly at a boundary fires in the NEXT chunk at its own
        // due position — never late, never twice. One LONG voice (due
        // 700, 200 samples) spans several chunks in both patterns to
        // pin voice-state persistence across callbacks.
        //
        // The schedule never has two voices live at once — deliberately.
        // Under chunking, a played-out voice frees its slot before later
        // events activate, so later events reuse EARLIER slots; the
        // continuous render assigns every slot up front. With overlapping
        // voices that reorders the f32 accumulation (music + a + b vs
        // music + b + a) — commutative in exact math, NOT bit-commutative
        // in f32 (found while writing this test; ~1 ulp, inaudible, no
        // RT-path change warranted). Single-live-voice frames are
        // order-free, which keeps this test exact instead of
        // tolerance-based.
        //
        // Out of scope by design: per-callback RMS/envelope (defined per
        // callback), frames_per_callback (the last chunk's value), and
        // >8 concurrent voices (MAX_VOICES exhaustion is a separate RT
        // property).
        const FRAMES: u64 = 1000;

        // deterministic stereo music with a clip-hot region (400..600):
        // hot + a full-gain hit exceeds 1.0 (clips), the cold region
        // never does — the clip counter is part of the invariance
        let music_samples: Vec<f32> = (0..FRAMES)
            .flat_map(|f| {
                let base = if (400..600).contains(&f) { 0.9 } else { 0.3 };
                let wiggle = (f % 13) as f32 / 13.0 * 0.2;
                [base - wiggle, base + wiggle * 0.5]
            })
            .collect();
        // sound 0: a short 24-sample hit peaking at 0.35; sound 1: a long
        // 200-sample ramp peaking at 0.35 (the chunk-spanning voice)
        let make_sounds = || {
            vec![
                HitSound {
                    samples: Arc::from(
                        (0..24u32)
                            .map(|i| 0.35 * i as f32 / 24.0)
                            .collect::<Vec<_>>(),
                    ),
                },
                HitSound {
                    samples: Arc::from(
                        (0..200u32)
                            .map(|i| 0.35 * i as f32 / 200.0)
                            .collect::<Vec<_>>(),
                    ),
                },
            ]
        };
        // (due, sound index, 8.8 gain; 256 = 1.0); tag = u16::MAX so no
        // latency-tap stamping interferes
        let events: &[(u64, u8, u16)] = &[
            (0, 0, 256),
            (64, 0, 192),
            (117, 0, 256),
            (235, 0, 256),
            (384, 0, 192),
            (470, 0, 256),
            (700, 1, 256), // the long voice: 700..900, spans chunks
            (940, 0, 256),
        ];

        // Render FRAMES frames through the given chunk sizes: fresh mixer
        // each call (same music bytes, same sounds, same pre-pushed
        // schedule), song_start = 0, default volume 1.0. Returns the full
        // interleaved buffer, the RT diagnostics and the finished latch.
        let render = |chunks: &[u64]| -> (Vec<f32>, (u32, u32, u32), bool) {
            let shared = Arc::new(SharedClock::new());
            shared.song_start.store(0, Ordering::Relaxed);
            let music = Music {
                samples: Arc::from(music_samples.clone()),
                channels: 2,
            };
            let mixer = Mixer::new(shared.clone(), Some(music), make_sounds(), 2);
            for &(due, sound, gain) in events {
                assert!(mixer.fire(SoundEvent {
                    due,
                    sound,
                    gain,
                    tag: u16::MAX,
                }));
            }
            let mut out = vec![0.0f32; FRAMES as usize * 2];
            let mut start = 0u64;
            for &frames in chunks {
                let s = start as usize * 2;
                mixer.mix_into(&mut out[s..s + frames as usize * 2], start, frames);
                shared.samples.fetch_add(frames, Ordering::Relaxed);
                start += frames;
            }
            assert_eq!(start, FRAMES, "chunk sizes must sum to FRAMES");
            (out, mixer.diagnostics(), mixer.finished())
        };

        let continuous = render(&[FRAMES]);
        let alt_117_118 = render(&[117, 118, 117, 118, 117, 118, 117, 118, 60]);
        let alt_64_128 = render(&[64, 128, 64, 128, 64, 128, 64, 128, 64, 128, 40]);

        // bit-identical audio across all three chunkings
        assert_eq!(alt_117_118.0, continuous.0);
        assert_eq!(alt_64_128.0, continuous.0);
        // identical RT diagnostics — and the paths actually exercised
        assert_eq!(alt_117_118.1, continuous.1);
        assert_eq!(alt_64_128.1, continuous.1);
        assert_eq!(continuous.1.0, 0, "no events dropped");
        assert_eq!(continuous.1.1, 0, "no voices exhausted");
        assert!(continuous.1.2 > 0, "the clip-hot region must clip");
        // the finished latch is chunking-independent
        assert!(continuous.2 && alt_117_118.2 && alt_64_128.2);
    }
}
