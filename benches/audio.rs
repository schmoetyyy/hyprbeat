//! Criterion benches for the phase-2 audio math (§12.3: "criterion for clock
//! math + slider LUT" — the slider LUT bench arrives with phase 6).
//!
//! Run with `cargo bench` inside the devshell. These pin the hot paths the
//! RT callback and song-load depend on.

use std::sync::Arc;

use hyprbeat::audio::clock::{Clock, SharedClock};
use hyprbeat::audio::decode;
use hyprbeat::audio::mixer::{HitSound, Mixer, Music};

use criterion::{Criterion, criterion_group, criterion_main};

fn bench_clock(c: &mut Criterion) {
    let shared = Arc::new(SharedClock::new());
    let mut clock = Clock::new_audio(shared.clone(), 48_000, 0.0);
    // 10 minutes of song already played
    shared
        .samples
        .store(48_000 * 600, std::sync::atomic::Ordering::Relaxed);

    c.bench_function("clock/time_ms (audio)", |b| {
        b.iter(|| {
            let t = clock.time_ms();
            std::hint::black_box(t);
        })
    });

    c.bench_function("clock/pause+resume cycle", |b| {
        b.iter(|| {
            clock.pause();
            let t = clock.time_ms();
            clock.resume();
            std::hint::black_box(t);
        })
    });
}

fn bench_mixer(c: &mut Criterion) {
    // 60 s of loud-ish stereo music at 48 kHz
    let n = 48_000 * 60;
    let samples: Vec<f32> = (0..n * 2)
        .map(|i| ((i as f32 * 0.03).sin() * 0.4).clamp(-1.0, 1.0))
        .collect();
    let music = Music {
        samples: Arc::from(samples),
        channels: 2,
    };
    let snd = HitSound {
        samples: Arc::from(vec![0.5f32; 4_410 * 2]),
    };
    let shared = Arc::new(SharedClock::new());
    let mixer = Mixer::new(shared, Some(music), vec![snd], 2);

    // 512 frames × 2ch — the typical PipeWire/ALSA callback geometry
    let mut out = vec![0.0f32; 512 * 2];
    c.bench_function("mixer/mix 512-frame stereo callback", |b| {
        b.iter(|| {
            // advance the counter like the real callback does
            let rms = mixer.mix_into(&mut out, 48_000 * 600, 512);
            std::hint::black_box(rms);
        })
    });
}

fn bench_resample(c: &mut Criterion) {
    // 10 s stereo 44.1 → 48 kHz (the rare-but-possible device mismatch)
    let n = 44_100 * 10;
    let input: Vec<f32> = (0..n * 2)
        .map(|i| ((i as f32 * 0.02).sin() * 0.3).clamp(-1.0, 1.0))
        .collect();
    c.bench_function("decode/resample 10s stereo 44.1k→48k", |b| {
        b.iter(|| {
            let out = decode::resample(&input, 2, 44_100, 48_000);
            std::hint::black_box(&out);
        })
    });
}

criterion_group!(benches, bench_clock, bench_mixer, bench_resample);
criterion_main!(benches);
