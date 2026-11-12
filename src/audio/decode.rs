//! Audio decode + cache + resample (§4, §5.2 `audio/decode.rs`).
//!
//! * Decode any `.osz`-plausible audio (mp3/ogg/flac/wav) to interleaved f32
//!   at the file's **native** rate with symphonia — in the calling thread,
//!   never the RT callback (§4).
//! * Cache the decode under `~/.cache/hyprbeat/decoded/<sha256-of-source>.f32`
//!   (I-07 paths; content-addressed like lazer's hash storage, §5.2). The
//!   cache stores the **native-rate** decode so it stays valid regardless of
//!   which rate the output stream opens at on a future session.
//! * Resample native → stream rate with a table-based windowed-sinc kernel
//!   (only used when the device refuses the file's rate; on PipeWire it
//!   accepts anything, so the clock stays exactly at file rate — D-30).
//!
//! All pure functions over in-memory bytes (I-09): no device I/O anywhere
//! here.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Fully-decoded audio: interleaved f32, native (file) sample rate.
#[derive(Debug, Clone)]
pub struct Decoded {
    pub samples: Vec<f32>,
    pub rate: u32,
    pub channels: u16,
}

impl Decoded {
    pub fn frames(&self) -> u64 {
        self.samples.len() as u64 / self.channels.max(1) as u64
    }
    pub fn duration_ms(&self) -> f64 {
        self.frames() as f64 / self.rate.max(1) as f64 * 1000.0
    }
}

/// Decode in-memory audio bytes (mp3/ogg/flac/wav — anything symphonia's
/// probe recognizes). Deterministic, allocation-light, zero device I/O.
pub fn decode_bytes(bytes: &[u8]) -> Result<Decoded, String> {
    // Owned copy: symphonia's MediaSource wants 'static + Send data.
    let src = MediaSourceStream::new(Box::new(Cursor::new(bytes.to_vec())), Default::default());
    // No probe hint: symphonia sniffs the container from the leading bytes
    // itself, which is deterministic for well-formed files.
    let probed = symphonia::default::get_probe()
        .format(
            &Hint::new(),
            src,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("cannot recognize the audio format: {e}"))?;
    let mut format = probed.format;

    // first track with a real codec
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "no decodable audio track found".to_string())?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("cannot build a decoder for this audio: {e}"))?;

    let mut samples: Vec<f32> = Vec::new();
    let mut spec: Option<(u32, u16)> = None;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphError::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break; // clean end of stream
            }
            Err(SymphError::ResetRequired) => {
                return Err("stream requires a reset mid-file (unsupported)".to_string());
            }
            Err(e) => return Err(format!("error while reading audio packets: {e}")),
        };

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // corrupt packet: skip it (the surrounding packets still decode)
            Err(SymphError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("error while decoding audio: {e}")),
        };
        if decoded.frames() == 0 {
            continue;
        }

        let this_spec = (decoded.spec().rate, decoded.spec().channels.count() as u16);
        match spec {
            None => {
                spec = Some(this_spec);
                sample_buf = Some(SampleBuffer::<f32>::new(
                    decoded.capacity() as u64,
                    *decoded.spec(),
                ));
            }
            Some(s) if s != this_spec => {
                return Err(format!(
                    "audio spec changed mid-file ({:?} → {this_spec:?}); unsupported",
                    s
                ));
            }
            _ => {}
        }

        if let Some(buf) = sample_buf.as_mut() {
            buf.copy_interleaved_ref(decoded);
            samples.extend_from_slice(buf.samples());
        }
    }

    let (rate, channels) = spec.ok_or_else(|| "audio stream carried no frames".to_string())?;
    if channels == 0 || rate == 0 {
        return Err("audio stream has a zero channel count or rate".to_string());
    }
    Ok(Decoded {
        samples,
        rate,
        channels,
    })
}

// ---------------------------------------------------------------------------
// Decode cache (§5.2: ~/.cache/hyprbeat/decoded/<sha256>.f32)
// ---------------------------------------------------------------------------

/// Cache container header, 18 bytes: magic `"HBD1"`, then little-endian
/// `channels: u16` (offset 4), `rate: u32` (offset 6), `frames: u64`
/// (offset 10), followed by `frames × channels` interleaved f32 LE.
/// Version 1. Offsets are pinned by tests.
const CACHE_MAGIC: &[u8; 4] = b"HBD1";
const CACHE_HEADER_LEN: usize = 18;

/// SHA-256 of the SOURCE file, hex-encoded — the cache key (content
/// addressing: same audio → one stored decode, §5.2).
pub fn cache_key(source_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Serialize a decode to the on-disk cache format (pure; IO by the caller of
/// [`write_cache`]).
pub fn encode_cache(decoded: &Decoded) -> Vec<u8> {
    let frames = decoded.frames();
    let mut out = Vec::with_capacity(CACHE_HEADER_LEN + decoded.samples.len() * 4);
    out.extend_from_slice(CACHE_MAGIC);
    out.extend_from_slice(&decoded.channels.to_le_bytes());
    out.extend_from_slice(&decoded.rate.to_le_bytes());
    out.extend_from_slice(&frames.to_le_bytes());
    debug_assert_eq!(out.len(), CACHE_HEADER_LEN);
    for s in &decoded.samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Parse the cache format. Fails loudly on any mismatch — a corrupt cache
/// entry is simply re-decoded (see [`decode_file_cached`]).
pub fn decode_cache(bytes: &[u8]) -> Result<Decoded, String> {
    let bad = |why: &str| format!("decode cache entry invalid: {why}");
    if bytes.len() < CACHE_HEADER_LEN || &bytes[0..4] != CACHE_MAGIC {
        return Err(bad("bad magic"));
    }
    let channels = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
    let rate = u32::from_le_bytes(bytes[6..10].try_into().unwrap());
    let frames = u64::from_le_bytes(bytes[10..18].try_into().unwrap());
    if channels == 0 || rate == 0 {
        return Err(bad("zero channels or rate"));
    }
    let payload = &bytes[CACHE_HEADER_LEN..];
    let total = frames as usize * channels as usize;
    if payload.len() < total * 4 {
        return Err(bad("truncated payload"));
    }
    if payload.len() > total * 4 {
        return Err(bad("trailing garbage"));
    }
    let mut samples = Vec::with_capacity(total);
    for chunk in payload[..total * 4].chunks_exact(4) {
        samples.push(f32::from_le_bytes(chunk.try_into().unwrap()));
    }
    Ok(Decoded {
        samples,
        rate,
        channels,
    })
}

/// Cache path for a source file inside a hyprbeat cache root
/// (`~/.cache/hyprbeat/decoded`).
pub fn cache_path(cache_root: &Path, source_bytes: &[u8]) -> PathBuf {
    cache_root.join(format!("{}.f32", cache_key(source_bytes)))
}

/// Read a cached decode if present and valid.
pub fn read_cache(cache_root: &Path, source_bytes: &[u8]) -> Option<Decoded> {
    let path = cache_path(cache_root, source_bytes);
    let bytes = std::fs::read(&path).ok()?;
    decode_cache(&bytes).ok()
}

/// Write a decode into the cache atomically (tmp + rename; a torn entry can
/// never be observed because [`decode_cache`] would reject it anyway).
pub fn write_cache(
    cache_root: &Path,
    source_bytes: &[u8],
    decoded: &Decoded,
) -> Result<(), String> {
    let path = cache_path(cache_root, source_bytes);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "cannot create the decode cache directory {}: {e}",
                parent.display()
            )
        })?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, encode_cache(decoded))
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("cannot finalize the cache entry {}: {e}", path.display()))?;
    Ok(())
}

/// Decode a file through the cache: hash the source bytes, look the decode up
/// in `cache_root`, and fall back to a fresh decode + cache write.
///
/// Returns `(decoded, cache_hit)`. Cache corruption or IO trouble never
/// fails the load — it just re-decodes (a wallpaper must not die over a
/// cache hiccup, I-08 posture).
pub fn decode_file_cached(path: &Path, cache_root: &Path) -> Result<(Decoded, bool), String> {
    let source = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;

    if let Some(hit) = read_cache(cache_root, &source) {
        tracing::debug!(
            key = %cache_key(&source),
            frames = hit.frames(),
            "decode cache hit"
        );
        return Ok((hit, true));
    }

    let decoded = decode_bytes(&source)?;
    if let Err(err) = write_cache(cache_root, &source, &decoded) {
        // cache failure is non-fatal by design (D-32 posture: re-decode)
        tracing::warn!(%err, "cannot write the decode cache (continuing without)");
    }
    Ok((decoded, false))
}

// ---------------------------------------------------------------------------
// Resampler (native rate → stream rate; used only when the device refuses
// the file's native rate — D-30)
// ---------------------------------------------------------------------------

/// Half-width of the sinc kernel, in zero-crossings per side (quality vs
/// decode-time tradeoff). 24/side measures ≈ 63 dB SNR on the worst-case
/// 44.1→48 kHz hop in this codebase's tests — transparent for a fallback
/// path that PipeWire systems never even take (D-30).
const RESAMPLE_TAPS: usize = 24;
/// Number of fractional phases precomputed in the kernel table.
const RESAMPLE_PHASES: usize = 1024;
/// Anti-alias cutoff as a fraction of the LOWER Nyquist. Deliberately below
/// the textbook 0.95: with integer-spaced taps, a near-Nyquist cutoff makes
/// the truncated kernel's alternating lobes reinforce at Nyquist (measured:
/// flat ≈1.6× gain across the band — 15 dB SNR). 0.85 with 24-zero taps gives
/// a clean 63+ dB passband and a real stopband.
const RESAMPLE_CUTOFF: f64 = 0.85;

/// Table-based windowed-sinc resampler, interleaved.
///
/// Quality-first, speed-second (this runs once per song load, in the decode
/// path — never in RT). Identity copy when the rates already match.
pub fn resample(input: &[f32], channels: u16, from_rate: u32, to_rate: u32) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    let in_frames = input.len() / ch;
    if from_rate == to_rate || in_frames == 0 || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to_rate as f64 / from_rate.max(1) as f64; // out frames per in frame

    // Anti-alias cutoff as a fraction of the SOURCE Nyquist: at the lower of
    // the two rates' Nyquist × 0.95.
    let fc = RESAMPLE_CUTOFF * ratio.min(1.0);
    // zero-crossing spacing in source samples is 1 / (2·fc) → widen the
    // window when heavily downsampling so the tap count covers it
    let zero_spacing = 1.0 / (2.0 * fc.max(1e-6));
    let half_width = (RESAMPLE_TAPS as f64 * zero_spacing).ceil() as i64;
    let taps = (2 * half_width + 1) as usize; // kernel length

    // Kernel table: [phase][tap] of sinc(k − phase_frac)·window, per-phase
    // normalized to unity DC gain.
    let mut table = vec![vec![0.0f32; taps]; RESAMPLE_PHASES];
    for (p, row) in table.iter_mut().enumerate() {
        let frac = p as f64 / RESAMPLE_PHASES as f64;
        let mut sum = 0.0f64;
        for (i, w) in row.iter_mut().enumerate() {
            let k = i as i64 - half_width; // source sample offset
            let x = k as f64 - frac; // distance in source samples
            let s = if x == 0.0 {
                2.0 * fc
            } else {
                (2.0 * fc * x * std::f64::consts::PI).sin() / (std::f64::consts::PI * x)
            };
            // Hann window over the half-width
            let wgt = if x.abs() >= half_width as f64 {
                0.0
            } else {
                0.5 + 0.5 * (std::f64::consts::PI * x / half_width as f64).cos()
            };
            let v = s * wgt;
            *w = v as f32;
            sum += v;
        }
        if sum.abs() > 1e-12 {
            let inv = 1.0 / sum;
            for w in row.iter_mut() {
                *w *= inv as f32;
            }
        }
    }

    let out_frames = ((in_frames as f64) * ratio).round() as usize;
    let mut out = vec![0.0f32; out_frames * ch];
    for o in 0..out_frames {
        // source position of output frame o
        let p = o as f64 / ratio;
        let base = p.floor() as i64;
        let phase = (((p - p.floor()) * RESAMPLE_PHASES as f64) as usize).min(RESAMPLE_PHASES - 1);
        let row = &table[phase];
        for c in 0..ch {
            let mut acc = 0.0f32;
            for (i, w) in row.iter().enumerate() {
                let k = base + i as i64 - half_width;
                if k < 0 || k >= in_frames as i64 {
                    continue; // zero padding at the edges
                }
                acc += input[k as usize * ch + c] * w;
            }
            out[o * ch + c] = acc;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Embedded hitsounds (§5.3: CC0 set ships in the binary)
// ---------------------------------------------------------------------------

/// The embedded hitsound set — see `assets/hitsounds/NOTICE.md` for the
/// provenance audit (original, generated by `dev/gen_assets.py`, CC0).
pub const HIT_SOUND_COUNT: usize = 4;

/// Decode the embedded hitsounds at their native rate (44100 mono).
/// Resampling to the stream rate is the caller's job (once, at startup).
pub fn embedded_hitsounds() -> Result<Vec<Decoded>, String> {
    const FILES: [&[u8]; HIT_SOUND_COUNT] = [
        include_bytes!("../../assets/hitsounds/normal.wav"),
        include_bytes!("../../assets/hitsounds/clap.wav"),
        include_bytes!("../../assets/hitsounds/whistle.wav"),
        include_bytes!("../../assets/hitsounds/finish.wav"),
    ];
    FILES.iter().map(|b| decode_bytes(b)).collect()
}

/// Decode one embedded asset (used by `--latency-test` and unit tests).
pub fn decode_embedded(index: usize) -> Result<Decoded, String> {
    const FILES: [&[u8]; HIT_SOUND_COUNT] = [
        include_bytes!("../../assets/hitsounds/normal.wav"),
        include_bytes!("../../assets/hitsounds/clap.wav"),
        include_bytes!("../../assets/hitsounds/whistle.wav"),
        include_bytes!("../../assets/hitsounds/finish.wav"),
    ];
    let b = FILES.get(index).ok_or("no such hitsound index")?;
    decode_bytes(b)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal in-test WAV writer (RIFF, 16-bit PCM) — lets decode tests run
    /// with zero fixture files.
    fn wav_bytes(samples_i16: &[i16], rate: u32, channels: u16) -> Vec<u8> {
        let data: Vec<u8> = samples_i16.iter().flat_map(|s| s.to_le_bytes()).collect();
        let byte_rate = rate * channels as u32 * 2;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM format
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&byte_rate.to_le_bytes());
        b.extend_from_slice(&(channels * 2).to_le_bytes()); // block align
        b.extend_from_slice(&16u16.to_le_bytes()); // bits
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&data);
        b
    }

    #[test]
    fn decode_wav_stereo_roundtrip() {
        // 3 frames of stereo: L=1000,2000,3000 ; R=-1000,-2000,-3000
        let pcm: Vec<i16> = vec![1000, -1000, 2000, -2000, 3000, -3000];
        let d = decode_bytes(&wav_bytes(&pcm, 44_100, 2)).unwrap();
        assert_eq!((d.rate, d.channels), (44_100, 2));
        assert_eq!(d.frames(), 3);
        let want: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32768.0).collect();
        assert!(
            d.samples
                .iter()
                .zip(&want)
                .all(|(a, b)| (a - b).abs() < 1e-9)
        );
    }

    #[test]
    fn decode_wav_rejects_garbage() {
        assert!(decode_bytes(b"not audio at all").is_err());
        assert!(decode_bytes(&[]).is_err());
    }

    #[test]
    fn embedded_assets_decode() {
        for i in 0..HIT_SOUND_COUNT {
            let d = decode_embedded(i).unwrap();
            assert_eq!((d.rate, d.channels), (44_100, 1), "asset {i}");
            assert!(d.frames() > 0, "asset {i} is not empty");
        }
    }

    #[test]
    fn cache_key_is_content_addressed() {
        let a = b"same";
        let b = b"same";
        let c = b"different";
        assert_eq!(cache_key(a), cache_key(b));
        assert_ne!(cache_key(a), cache_key(c));
        assert_eq!(cache_key(a).len(), 64, "hex sha256");
    }

    #[test]
    fn cache_roundtrip() {
        let d = Decoded {
            samples: vec![0.0, 0.25, -0.5, 0.75],
            rate: 48_000,
            channels: 2,
        };
        let bytes = encode_cache(&d);
        assert_eq!(bytes.len(), CACHE_HEADER_LEN + 4 * 4, "header + payload");
        let back = decode_cache(&bytes).unwrap();
        assert_eq!((back.rate, back.channels), (48_000, 2));
        assert_eq!(back.samples, d.samples);
        assert_eq!(back.frames(), 2);
    }

    #[test]
    fn cache_rejects_corruption() {
        let d = Decoded {
            samples: vec![0.5; 8],
            rate: 44_100,
            channels: 1,
        };
        let mut bytes = encode_cache(&d);
        assert!(decode_cache(&bytes).is_ok());
        bytes[0] = b'X'; // bad magic
        assert!(decode_cache(&bytes).is_err());
        let mut short = bytes.clone();
        short.truncate(15); // torn header
        assert!(decode_cache(&short).is_err());
        let mut long = bytes.clone();
        long.extend_from_slice(&0.0f32.to_le_bytes()); // trailing garbage
        assert!(decode_cache(&long).is_err());
        let torn = &bytes[..bytes.len() - 4]; // truncated payload
        assert!(decode_cache(torn).is_err());
    }

    #[test]
    fn decode_file_cached_hits_second_time() {
        let dir = std::env::temp_dir().join(format!("hyprbeat-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.wav");
        std::fs::write(&src, wav_bytes(&[1200, -400, 900, -300], 22_050, 1)).unwrap();

        let (first, hit) = decode_file_cached(&src, &dir).unwrap();
        assert!(!hit, "first call decodes");
        assert!(
            dir.join(format!("{}.f32", cache_key(&std::fs::read(&src).unwrap())))
                .exists()
        );
        let (second, hit) = decode_file_cached(&src, &dir).unwrap();
        assert!(hit, "second call is served from the cache");
        assert_eq!(first.samples, second.samples);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- resampler ----------------------------------------------------------

    #[test]
    fn resample_identity_when_rates_match() {
        let input = vec![0.1f32, -0.2, 0.3, -0.4];
        assert_eq!(resample(&input, 2, 48_000, 48_000), input);
    }

    #[test]
    fn resample_preserves_dc_level() {
        // constant 0.5 → constant 0.5 (interior samples; edges ring slightly)
        let n = 4000;
        let input = vec![0.5f32; n];
        let out = resample(&input, 1, 44_100, 48_000);
        assert_eq!(out.len(), (n as f64 * 48_000.0 / 44_100.0).round() as usize);
        let interior = &out[100..out.len() - 100];
        assert!(
            interior.iter().all(|&s| (s - 0.5).abs() < 0.005),
            "DC must survive resampling: min={:.4} max={:.4}",
            interior.iter().cloned().fold(2.0, f32::min),
            interior.iter().cloned().fold(-2.0, f32::max),
        );
    }

    #[test]
    fn resample_sine_fidelity() {
        // 1 kHz sine at 44.1k → 48k must match the analytic reference well
        // (band-limited content, so the sinc kernel is in its comfort zone).
        // The measured design floor is ≈ 63 dB; 40 dB leaves margin for
        // table-phase quantization without pretending to lab equipment.
        let from_rate = 44_100u32;
        let to_rate = 48_000u32;
        let f = 1000.0f64;
        let n = 4096;
        let input: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / from_rate as f64).sin() as f32)
            .collect();
        let out = resample(&input, 1, from_rate, to_rate);
        let mut err_sq = 0.0f64;
        let mut ref_sq = 0.0f64;
        let edge = 64; // windowed truncation rings at the very edges
        let window = &out[edge..out.len() - edge];
        for (k, &v) in window.iter().enumerate() {
            let o = k + edge;
            let want = (2.0 * std::f64::consts::PI * f * o as f64 / to_rate as f64).sin();
            err_sq += (v as f64 - want).powi(2);
            ref_sq += want.powi(2);
        }
        let snr_db = 10.0 * (ref_sq / err_sq.max(1e-12)).log10();
        assert!(
            snr_db > 40.0,
            "SNR {snr_db:.1} dB is too low for music-grade resampling"
        );
    }

    #[test]
    fn resample_stereo_keeps_channels_independent() {
        // L = +0.4 constant, R = −0.4 constant
        let n = 3000;
        let input: Vec<f32> = (0..n).flat_map(|_| [0.4, -0.4]).collect();
        let out = resample(&input, 2, 44_100, 48_000);
        let frames = out.len() / 2;
        for o in 100..frames - 100 {
            assert!(
                (out[o * 2] - 0.4).abs() < 0.005,
                "left channel leaked at {o}"
            );
            assert!(
                (out[o * 2 + 1] + 0.4).abs() < 0.005,
                "right channel leaked at {o}"
            );
        }
    }

    #[test]
    fn resample_output_length_is_ratio_times_input() {
        let out = resample(&vec![0.0f32; 44_100], 1, 44_100, 48_000);
        assert_eq!(out.len(), 48_000);
        let out = resample(&vec![0.0f32; 48_000], 1, 48_000, 44_100);
        assert_eq!(out.len(), 44_100);
    }

    #[test]
    fn resample_downsample_cuts_above_target_nyquist() {
        // 20 kHz tone downsampled 48k → 22.05k is above the target Nyquist:
        // it must be strongly attenuated (anti-aliasing works)
        let f = 20_000.0f64;
        let n = 8192;
        let input: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * f * i as f64 / 48_000.0).sin() as f32)
            .collect();
        let out = resample(&input, 1, 48_000, 22_050);
        let power: f64 = out[100..out.len() - 100]
            .iter()
            .map(|&s| (s * s) as f64)
            .sum();
        let in_power: f64 = input[100..n - 100].iter().map(|&s| (s * s) as f64).sum();
        assert!(
            power / in_power < 0.05,
            "aliasing energy ratio {} — cutoff not working",
            power / in_power
        );
    }

    #[test]
    fn resample_empty_input() {
        assert!(resample(&[], 1, 44_100, 48_000).is_empty());
    }

    // keep imports honest
    #[test]
    #[allow(dead_code)]
    fn io_traits_are_used() {
        fn _assert_io<T: std::io::Read + std::io::Seek>() {}
        _assert_io::<Cursor<&[u8]>>();
    }
}
