//! Audio engine (§3.1/§3.2, phase 2): cpal raw output callback = the clock =
//! the latency floor (D-03, D-04).
//!
//! [`AudioEngine::start`] performs, in order:
//! 1. decode the music file (or open a silent stream for the latency test),
//!    through the SHA-256 decode cache (§5.2);
//! 2. pick an output config — **at the file's native rate when the device
//!    accepts it** (PipeWire does), else a device-supported rate with the
//!    decode resampled to match (D-30: the clock then counts exactly the
//!    file's samples);
//! 3. decode + resample the embedded CC0 hitsounds (§5.3, `assets/hitsounds/`);
//! 4. build the mixer + clock, open the f32 stream, and play.
//!
//! Failure is **never** fatal to the wallpaper (D-16, I-08): the caller
//! degrades to the wall clock, shows the degraded state (I-12) and keeps
//! rendering. The stream's error callback only stores a flag (I-02: the RT
//! context must not allocate or log); the main tick notices it and degrades.

pub mod clock;
pub mod decode;
pub mod mixer;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleRate, StreamConfig};

use clock::{Clock, SharedClock};
use decode::Decoded;
use mixer::{HitSound, Mixer, Music};

/// XDG cache root for this project (`~/.cache/hyprbeat`), I-07.
pub fn cache_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "hyprbeat").map(|d| d.cache_dir().to_path_buf())
}

/// Preferred stereo fallback when a device only offers other layouts.
const PREFERRED_CHANNELS: u16 = 2;

/// Why an audio engine could not start (all non-fatal — the caller degrades,
/// D-16).
#[derive(Debug)]
pub enum AudioStartError {
    NoDefaultDevice,
    NoFloatConfig,
    Decode(String),
    StreamBuild(String),
    Play(String),
}

impl std::fmt::Display for AudioStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDefaultDevice => write!(f, "no default audio output device"),
            Self::NoFloatConfig => write!(f, "the output device offers no f32 config"),
            Self::Decode(e) => write!(f, "cannot decode the audio: {e}"),
            Self::StreamBuild(e) => write!(f, "cannot open the output stream: {e}"),
            Self::Play(e) => write!(f, "cannot start playback: {e}"),
        }
    }
}

/// A running audio session. The `Stream` field is load-bearing: dropping a
/// cpal `Stream` stops playback, so the engine must stay alive for as long as
/// the song should be audible.
pub struct AudioEngine {
    pub clock: Clock,
    pub mixer: Arc<Mixer>,
    _stream: cpal::Stream,
    /// Decoded + cache info for the log/summary.
    pub music_frames: Option<u64>,
}

/// Options for [`AudioEngine::start`].
pub struct StartOptions<'a> {
    /// Music file to load (mp3/ogg/flac/wav), or `None` for a silent stream
    /// (the `--latency-test` without-song case).
    pub music: Option<&'a Path>,
    /// Master volume 0..1 (§6 `volume`; CLI `--volume` in phase 2).
    pub volume: f32,
    /// Universal offset in ms (§3.3; CLI `--offset-ms` in phase 2, config
    /// in phase 5).
    pub offset_ms: f64,
    /// Lead-in before song t=0 (ms). Phase 2 uses 0; beatmap `AudioLeadIn`
    /// arrives in phase 3.
    pub lead_in_ms: f64,
    /// Requested output buffer in frames; 0 = device default (D-37). The
    /// device default measured 32 ms/1536 frames on the dev machine — the
    /// input→sound budget (§3.6, ≤ 25 ms) cannot fit inside a 32 ms period,
    /// so the engine requests a small fixed buffer and falls back to the
    /// device default only if the host refuses.
    pub buffer_frames: u32,
}

impl AudioEngine {
    /// Open the output device and start playing. Every failure degrades at
    /// the CALLER — never crashes the wallpaper (D-16).
    pub fn start(opts: StartOptions<'_>) -> Result<Self, AudioStartError> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or(AudioStartError::NoDefaultDevice)?;
        let device_name = device.name().unwrap_or_else(|_| "unknown".to_string());

        // -- decode (through the cache) --------------------------------------
        let decoded_music: Option<Decoded> = match opts.music {
            None => None,
            Some(path) => {
                let cache_root = cache_dir().unwrap_or_else(std::env::temp_dir);
                let (decoded, hit) = decode::decode_file_cached(path, &cache_root)
                    .map_err(AudioStartError::Decode)?;
                tracing::info!(
                    path = %path.display(),
                    // `%` (Display): string fields print bare (`cache=miss`) so
                    // log greps and humans read them without quotes (D-38).
                    cache = %(if hit { "hit" } else { "miss" }),
                    rate = decoded.rate,
                    channels = decoded.channels,
                    duration_ms = decoded.duration_ms() as u64,
                    "music decoded"
                );
                Some(decoded)
            }
        };

        // -- output config (D-30: native rate first; D-37: small fixed buffer) --
        let want_rate = decoded_music.as_ref().map(|d| d.rate).unwrap_or(48_000);
        let (mut config, stream_rate, out_channels) =
            pick_output_config(&device, want_rate, opts.buffer_frames)?;

        // -- music at STREAM rate ---------------------------------------------
        let music = decoded_music.map(|d| {
            let samples = if d.rate == stream_rate {
                d.samples
            } else {
                tracing::info!(
                    from = d.rate,
                    to = stream_rate,
                    "device refused the file's native rate — resampling at load time"
                );
                decode::resample(&d.samples, d.channels, d.rate, stream_rate)
            };
            Music {
                samples: Arc::from(samples),
                channels: d.channels.max(1) as u32,
            }
        });

        // -- hitsounds at STREAM rate (§5.3: embedded CC0 set) ----------------
        let sounds = load_hitsounds(stream_rate)?;

        // -- clock + mixer + stream --------------------------------------------
        let shared = Arc::new(SharedClock::new());
        let clock = Clock::new_audio(shared.clone(), stream_rate, opts.offset_ms);
        if opts.lead_in_ms > 0.0 {
            clock.begin_song_in(opts.lead_in_ms);
        }
        let music_frames = music.as_ref().map(|m| m.frames());
        let mixer = Arc::new(Mixer::new(
            shared.clone(),
            music,
            sounds,
            out_channels as u32,
        ));
        mixer.set_volume(opts.volume);

        // D-37: try the requested config; if a FIXED buffer size is refused,
        // retry once with the device default (never fatal — D-16). The actual
        // callback geometry is observed at runtime and lands in the
        // `--latency-test` report regardless of which branch won.
        let requested_fixed = opts.buffer_frames > 0;
        let mut buffer_desc = if requested_fixed {
            format!("fixed-{}", opts.buffer_frames)
        } else {
            "device default".to_string()
        };
        let stream = match build_stream(&device, &config, &mixer, &shared, out_channels) {
            Ok(stream) => stream,
            Err(err) if requested_fixed => {
                tracing::warn!(
                    requested = opts.buffer_frames,
                    error = %err,
                    "device refused the fixed buffer size — retrying with the device default (D-37)"
                );
                config = pick_output_config(&device, stream_rate, 0)?.0;
                buffer_desc = "device default (fixed refused)".to_string();
                build_stream(&device, &config, &mixer, &shared, out_channels)
                    .map_err(|e| AudioStartError::StreamBuild(e.to_string()))?
            }
            Err(err) => return Err(AudioStartError::StreamBuild(err.to_string())),
        };
        stream
            .play()
            .map_err(|e| AudioStartError::Play(e.to_string()))?;

        let frames = music_frames;
        tracing::info!(
            device = %device_name,
            rate = stream_rate,
            channels = out_channels,
            buffer = %buffer_desc,
            "audio stream started (f32)"
        );
        Ok(Self {
            clock,
            mixer,
            _stream: stream,
            music_frames: frames,
        })
    }

    /// Has the RT error callback flagged the stream as dead? (Checked by the
    /// main tick; degrades the clock when true.)
    pub fn stream_failed(&self) -> bool {
        self.clock.shared().stream_failed.load(Ordering::Relaxed)
    }
}

/// Choose an f32 output config at (in order of preference) `want_rate`, the
/// device's default rate, 48 kHz, or the best the range allows, with the
/// buffer size from `buffer_frames` (0 = device default; D-37).
///
/// Returns the cpal config plus the negotiated rate and channel count — the
/// negotiated rate is what the clock counts and what the music must be
/// resampled to.
fn pick_output_config(
    device: &cpal::Device,
    want_rate: u32,
    buffer_frames: u32,
) -> Result<(StreamConfig, u32, u16), AudioStartError> {
    let ranges: Vec<cpal::SupportedStreamConfigRange> = device
        .supported_output_configs()
        .map_err(|e| AudioStartError::StreamBuild(e.to_string()))?
        .filter(|r| r.sample_format() == cpal::SampleFormat::F32)
        .collect();
    if ranges.is_empty() {
        return Err(AudioStartError::NoFloatConfig);
    }

    // Prefer a stereo layout; fall back to mono; then to whatever exists.
    let pick = ranges
        .iter()
        .find(|r| r.channels() == PREFERRED_CHANNELS)
        .or_else(|| ranges.iter().find(|r| r.channels() == 1))
        .unwrap_or(&ranges[0]);

    let contains =
        |rate: u32| (pick.min_sample_rate().0..=pick.max_sample_rate().0).contains(&rate);
    let default_rate = device
        .default_output_config()
        .map(|c| c.sample_rate().0)
        .ok();
    let rate = if contains(want_rate) {
        want_rate
    } else if default_rate.is_some_and(contains) {
        default_rate.expect("checked")
    } else if contains(48_000) {
        48_000
    } else {
        pick.max_sample_rate().0
    };

    let config = StreamConfig {
        channels: pick.channels(),
        sample_rate: SampleRate(rate),
        buffer_size: if buffer_frames > 0 {
            cpal::BufferSize::Fixed(buffer_frames)
        } else {
            cpal::BufferSize::Default
        },
    };
    Ok((config, rate, pick.channels()))
}

/// Build the output stream with the phase-2 callbacks (I-02: the data
/// callback mixes and advances the counter; the error callback only stores a
/// flag). A separate fn so the D-37 fixed-buffer fallback can build a second
/// stream with fresh closures (a failed build consumes the first pair).
fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    mixer: &Arc<Mixer>,
    shared: &Arc<SharedClock>,
    channels: u16,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mixer = mixer.clone();
    let shared = shared.clone();
    let channels = channels as usize;
    let data_callback = move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
        // I-02: only atomics + buffer writes live here (see mixer docs)
        let frames = (data.len() / channels) as u64;
        let start = shared.samples.load(Ordering::Relaxed);
        mixer.mix_into(data, start, frames);
        shared.samples.fetch_add(frames, Ordering::Relaxed);
    };
    let error_callback = {
        // RT context: store a flag, nothing else — the main tick degrades
        // and logs from a safe context.
        let shared = shared.clone();
        move |_err: cpal::StreamError| {
            shared.stream_failed.store(true, Ordering::Relaxed);
        }
    };
    device.build_output_stream(config, data_callback, error_callback, None)
}

/// Decode + resample the embedded hitsounds to the stream rate.
fn load_hitsounds(rate: u32) -> Result<Vec<HitSound>, AudioStartError> {
    let embedded = decode::embedded_hitsounds().map_err(AudioStartError::Decode)?;
    Ok(embedded
        .into_iter()
        .map(|d| {
            let samples = if d.rate == rate {
                d.samples
            } else {
                decode::resample(&d.samples, d.channels, d.rate, rate)
            };
            HitSound {
                samples: Arc::from(samples),
            }
        })
        .collect())
}

/// Default master volume (§6 `[general] volume`; config lands in phase 5).
pub const DEFAULT_VOLUME: f32 = 0.7;

/// Default output buffer in frames (D-37). Dev-machine measurement
/// 2026-11-13: the device default was 1536 frames @ 48 kHz = 32 ms period —
/// the §3.6 input→sound budget (p95 audible ≤ 25 ms) cannot fit inside a
/// 32 ms period, so we request 256 frames (≈ 5.3 ms @ 48 kHz, ≈ 5.8 ms @
/// 44.1 kHz) and fall back to the device default if the host refuses
/// (`--buffer 0` forces the old behavior; `--buffer N` tunes).
pub const DEFAULT_BUFFER_FRAMES: u32 = 256;

// ---------------------------------------------------------------------------
// tests (config selection math is pure enough to pin without a device)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_volume_matches_spec() {
        assert_eq!(DEFAULT_VOLUME, 0.7, "§6 [general] volume default");
    }

    #[test]
    fn default_buffer_fits_latency_budget() {
        // D-37: 256 frames must stay well under the §3.6 25 ms audible
        // budget at both common rates (two periods must fit with room for
        // the tap-to-mixed leg).
        for rate in [44_100, 48_000] {
            let period_ms = DEFAULT_BUFFER_FRAMES as f64 * 1000.0 / rate as f64;
            assert!(
                period_ms < 7.0,
                "buffer period at {rate} Hz is {period_ms:.2} ms — too large"
            );
        }
    }

    #[test]
    fn cache_dir_is_a_hyprbeat_path() {
        if let Some(dir) = cache_dir() {
            let s = dir.to_string_lossy();
            assert!(s.contains("hyprbeat"), "got {s}");
            assert!(s.contains(".cache"), "XDG cache root, got {s}");
        }
    }

    #[test]
    fn duration_math() {
        // 48 000 stereo frames at 48 kHz = 1 s, no matter the interleave
        let d = Decoded {
            samples: vec![0.0; 48_000 * 2],
            rate: 48_000,
            channels: 2,
        };
        assert_eq!(d.duration_ms(), 1000.0);
    }
}
