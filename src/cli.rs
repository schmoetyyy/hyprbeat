//! Phase-1 command line (CONTEXT.md D-20).
//!
//! Deliberately a tiny hand-rolled parser: every phase-1 flag is a dev or
//! diagnostic flag, and `clap` is scheduled to arrive in phase 5 together
//! with the config-file overrides (§6). Keeping the dependency tree small
//! is a project value (§4).

use std::path::PathBuf;
use std::time::Duration;

/// Default live-mode safety timeout (D-24): a broken phase-1 build must never
/// strand the user's desktop. `--timeout 0` disables it; phase 7 retires the
/// default once real mode switching exists.
pub const DEFAULT_LIVE_TIMEOUT: Duration = Duration::from_secs(120);
/// Default probe run length.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

pub const HELP: &str = "\
hyprbeat — an open-source rhythm game that lives on your desktop background,
compatible with the osu! beatmap format. Built for Hyprland/Wayland.

Phase 2 preview: ambient gradient wallpaper, frame-callback render loop,
fractional-scale-correct rendering, audio-clock-driven animation (cpal +
symphonia). Maps, judgement and the config file arrive in phases 3–8
(see CONTEXT.md §10).

USAGE:
    hyprbeat [OPTIONS]

OPTIONS (phase-2 dev/diagnostic set; clap + config flags land in phase 5):
        --headless          Render one frame offscreen to a PNG and exit
                            (no compositor required; needs a Vulkan or GLES
                            driver — lavapipe/swrast works)
        --size WxH          Headless logical size [default: 1920x1080]
        --scale SCALE       Headless simulated output scale [default: 1.0]
        --out PATH          Headless PNG output path
                            [default: $TMPDIR/hyprbeat-headless.png]
        --testcard          Draw the scale test pattern (2 px accent border,
                            1 px grid, checker strip, rings, 1 Hz sweep) over
                            the gradient — use it to judge 125% scaling
        --audio PATH        Load an audio file (mp3/ogg/flac/wav) and sync the
                            wallpaper to it: the clock, the animation and
                            the pause machinery all hang off the audio stream
                            (CONTEXT.md §3.3). Decodes land in
                            ~/.cache/hyprbeat/decoded/<sha256>.f32
        --no-audio          Force the degraded wall-clock path (D-16) even if
                            an output device exists — validation instrument
        --volume V          Master volume 0.0–1.0 [default: 0.7]
        --offset-ms N       Universal audio offset (§3.3): shifts game time
                            relative to what you hear; negative = earlier
                            [default: 0]
        --buffer N          Output buffer in frames — the latency floor of the
                            input→sound budget (§3.6). 0 = device default
                            (measured 32 ms on PipeWire — too big); the engine
                            falls back to the device default if a fixed size is
                            refused [default: 256 ≈ 5.3 ms at 48 kHz]
        --latency-test      Input → sound → photon report (§3.6, §12.3):
                            switch to an EMPTY workspace, click the desktop
                            once for keyboard focus, then TAP keys; every tap
                            fires a hitsound + flash and is stamped through
                            the whole pipeline. A summary prints at exit
        --probe             CONTEXT.md §14 Q-1/Q-2 diagnostic: temporarily
                            enable input + keyboard_interactivity=on_demand on
                            the background layer, log what the compositor
                            actually delivers, print a PASS/FAIL report
        --fps FPS           Idle frame-rate cap [default: 30]
        --timeout SECS      Quit after SECS seconds; 0 disables
                            [default: 120 live, 10 probe]
        --version           Print version
    -h, --help              Print help

EXAMPLES:
    hyprbeat --testcard --timeout 10                     # watch the background
    hyprbeat --headless --scale 1.25 --testcard --out /tmp/card.png
    hyprbeat --audio assets/test-tone.wav --timeout 20   # music-synced animation
    kill -USR1 <pid> / kill -USR2 <pid>                  # pause / resume (D-33)
    hyprbeat --latency-test --timeout 30                 # tap-to-sound report
    hyprbeat --probe --timeout 8                         # Q-1/Q-2 verification

Environment: RUST_LOG=hyprbeat=debug for logs, WAYLAND_DEBUG=1 for a protocol
trace. Docs: https://github.com/... (repo URL set at first publish).
";

#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    /// Offscreen single-frame render to PNG (no layer-shell, no compositor).
    pub headless: bool,
    /// Q-1/Q-2 input diagnostics (§14): full input region + on_demand keyboard.
    pub probe: bool,
    /// Tap-to-sound latency report (§3.6, §12.3).
    pub latency_test: bool,
    /// Draw the scale test pattern.
    pub testcard: bool,
    /// Hard exit deadline. `None` = run until killed.
    pub timeout: Option<Duration>,
    /// Idle frame-rate cap while in wallpaper mode (§3.5 `target_fps_idle`).
    pub idle_fps: u32,
    /// Headless logical surface size.
    pub size: (u32, u32),
    /// Headless simulated compositor scale (D-17).
    pub scale: f64,
    /// Headless PNG destination (defaults into the system temp dir, I-07).
    pub out: Option<PathBuf>,
    /// Music file to load and sync to (`--audio`, phase 2).
    pub audio: Option<PathBuf>,
    /// Force the degraded wall clock even with a working device (D-16 test).
    pub no_audio: bool,
    /// Master volume 0..1 (§6 `[general] volume` — CLI stand-in until phase 5).
    pub volume: f32,
    /// Universal audio offset in ms (§3.3).
    pub offset_ms: f64,
    /// Requested output buffer size in frames (D-37). 256 ≈ 5.3 ms at 48 kHz;
    /// 0 = device default (was 32 ms / 1536 frames on the dev machine — the
    /// §3.6 input→sound budget busts with that). 16–8192 clamped; the engine
    /// falls back to the device default if the host refuses a fixed size.
    pub buffer_frames: u32,
    pub help: bool,
    pub version: bool,
}

impl Args {
    /// Smallest gap between two rendered frames in milliseconds.
    pub fn frame_interval_ms(&self) -> f64 {
        1000.0 / self.idle_fps.max(1) as f64
    }
}

/// Parse an argument list (without the program name).
pub fn parse<I, S>(argv: I) -> Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut headless = false;
    let mut probe = false;
    let mut latency_test = false;
    let mut testcard = false;
    let mut idle_fps: u32 = 30;
    let mut size = (1920u32, 1080u32);
    let mut scale = 1.0f64;
    let mut out: Option<PathBuf> = None;
    let mut audio: Option<PathBuf> = None;
    let mut no_audio = false;
    let mut volume = crate::audio::DEFAULT_VOLUME;
    let mut offset_ms = 0.0f64;
    let mut buffer_frames = crate::audio::DEFAULT_BUFFER_FRAMES;
    let mut help = false;
    let mut version = false;
    let mut timeout: Option<Option<Duration>> = None;

    let mut it = argv.into_iter().map(Into::into).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--headless" => headless = true,
            "--probe" => probe = true,
            "--latency-test" => latency_test = true,
            "--testcard" => testcard = true,
            "-h" | "--help" => help = true,
            "--version" => version = true,
            "--audio" => {
                let raw = parse_value(&mut it, "--audio", |s| Ok::<_, String>(s.to_owned()))?;
                audio = Some(PathBuf::from(raw));
            }
            "--no-audio" => no_audio = true,
            "--volume" => {
                volume = parse_value(&mut it, "--volume", parse_volume)?;
            }
            "--offset-ms" => {
                offset_ms = parse_value(&mut it, "--offset-ms", parse_offset)?;
            }
            "--buffer" => {
                buffer_frames = parse_value(&mut it, "--buffer", parse_buffer)?;
            }
            "--fps" => {
                idle_fps = parse_value(&mut it, "--fps", parse_fps)?;
            }
            "--size" => {
                size = parse_value(&mut it, "--size", parse_size)?;
            }
            "--scale" => {
                scale = parse_value(&mut it, "--scale", parse_scale)?;
            }
            "--out" => {
                let raw = parse_value(&mut it, "--out", |s| Ok::<_, String>(s.to_owned()))?;
                out = Some(PathBuf::from(raw));
            }
            "--timeout" => {
                let raw = parse_value(&mut it, "--timeout", parse_timeout)?;
                timeout = Some(raw);
            }
            other => return Err(format!("unknown option `{other}`")),
        }
    }

    if scale == 0.0 {
        return Err("`--scale 0` is not a scale".to_owned());
    }
    if no_audio && audio.is_some() {
        return Err("--no-audio and --audio are mutually exclusive".to_owned());
    }
    if no_audio && latency_test {
        return Err("--latency-test needs an audio stream to fire hitsounds through; it cannot run with --no-audio".to_owned());
    }
    if headless && (audio.is_some() || latency_test || no_audio) {
        return Err("--headless renders a still frame; --audio/--latency-test/--no-audio are live-mode flags".to_owned());
    }

    // --timeout: explicit value wins (0 disables); otherwise a mode-dependent
    // safety default (D-24).
    let timeout = match timeout {
        Some(explicit) => explicit,
        None => {
            if headless {
                None
            } else if probe {
                Some(DEFAULT_PROBE_TIMEOUT)
            } else {
                Some(DEFAULT_LIVE_TIMEOUT)
            }
        }
    };

    // The test card is the whole point of a headless render in phase 1.
    if headless {
        testcard = true;
    }

    Ok(Args {
        headless,
        probe,
        latency_test,
        testcard,
        timeout,
        idle_fps,
        size,
        scale,
        out,
        audio,
        no_audio,
        volume,
        offset_ms,
        buffer_frames,
        help,
        version,
    })
}

fn parse_value<I, T>(
    it: &mut std::iter::Peekable<I>,
    flag: &str,
    parse_fn: impl Fn(&str) -> Result<T, String>,
) -> Result<T, String>
where
    I: Iterator<Item = String>,
{
    let value = it
        .next()
        .ok_or_else(|| format!("`{flag}` requires a value"))?;
    parse_fn(&value).map_err(|e| format!("`{flag} {value}`: {e}"))
}

fn parse_fps(s: &str) -> Result<u32, String> {
    let fps: u32 = s
        .parse()
        .map_err(|_| format!("`{s}` is not a positive integer"))?;
    if !(1..=240).contains(&fps) {
        return Err("fps must be between 1 and 240".to_owned());
    }
    Ok(fps)
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| format!("`{s}` is not WxH (e.g. 1920x1080)"))?;
    let (w, h): (u32, u32) = (
        w.parse()
            .map_err(|_| format!("`{w}` is not a positive integer"))?,
        h.parse()
            .map_err(|_| format!("`{h}` is not a positive integer"))?,
    );
    if w == 0 || h == 0 {
        return Err("both dimensions must be non-zero".to_owned());
    }
    if w > 16384 || h > 16384 {
        return Err("dimensions above 16384 are not supported".to_owned());
    }
    Ok((w, h))
}

fn parse_scale(s: &str) -> Result<f64, String> {
    let scale: f64 = s.parse().map_err(|_| format!("`{s}` is not a number"))?;
    if !scale.is_finite() || !(0.25..=4.0).contains(&scale) {
        return Err("scale must be between 0.25 and 4.0".to_owned());
    }
    Ok(scale)
}

/// `--volume V`: master volume 0..1.
fn parse_volume(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|_| format!("`{s}` is not a number"))?;
    if !(0.0..=1.0).contains(&v) {
        return Err("volume must be between 0.0 and 1.0".to_owned());
    }
    Ok(v)
}

/// `--offset-ms N`: universal audio offset in ms (§3.3); clamped to a sane
/// ±10 s so a typo cannot invert the game's sense of time.
fn parse_offset(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("`{s}` is not a number"))?;
    if !v.is_finite() {
        return Err("offset must be a finite number".to_owned());
    }
    if v.abs() > 10_000.0 {
        return Err("offset is limited to ±10 000 ms".to_owned());
    }
    Ok(v)
}

/// `--buffer N`: output buffer in frames (D-37). 0 = device default;
/// non-zero must be 16..=8192 (below 16 the callback rate itself becomes
/// an RT-scheduling hazard, above 8192 the latency is a fifth of a second).
fn parse_buffer(s: &str) -> Result<u32, String> {
    let n: u32 = s
        .parse()
        .map_err(|_| format!("`{s}` is not a non-negative integer"))?;
    if n != 0 && !(16..=8192).contains(&n) {
        return Err("buffer must be 0 (device default) or between 16 and 8192 frames".to_owned());
    }
    Ok(n)
}

/// `--timeout SECS`: a non-negative duration in (fractional) seconds; `0`
/// means "no timeout".
fn parse_timeout(s: &str) -> Result<Option<Duration>, String> {
    let secs: f64 = s
        .parse()
        .map_err(|_| format!("`{s}` is not a number of seconds"))?;
    if !secs.is_finite() || secs < 0.0 {
        return Err("timeout must be zero or a positive number of seconds".to_owned());
    }
    if secs == 0.0 {
        return Ok(None);
    }
    Ok(Some(Duration::from_secs_f64(secs)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults() {
        let a = args(&[]).unwrap();
        assert!(!a.headless && !a.probe && !a.testcard && !a.latency_test);
        assert_eq!(a.timeout, Some(DEFAULT_LIVE_TIMEOUT));
        assert_eq!(a.idle_fps, 30);
        assert_eq!(a.size, (1920, 1080));
        assert_eq!(a.scale, 1.0);
        assert_eq!(a.frame_interval_ms(), 1000.0 / 30.0);
        assert_eq!(a.audio, None);
        assert!(!a.no_audio);
        assert!((a.volume - 0.7).abs() < 1e-6);
        assert_eq!(a.offset_ms, 0.0);
    }

    #[test]
    fn audio_flags() {
        let a = args(&[
            "--audio",
            "/tmp/song.mp3",
            "--volume",
            "0.25",
            "--offset-ms",
            "-12.5",
        ])
        .unwrap();
        assert_eq!(a.audio, Some(PathBuf::from("/tmp/song.mp3")));
        assert!((a.volume - 0.25).abs() < 1e-6);
        assert_eq!(a.offset_ms, -12.5);
    }

    #[test]
    fn audio_flag_conflicts() {
        assert!(args(&["--no-audio", "--audio", "x.mp3"]).is_err());
        assert!(args(&["--no-audio", "--latency-test"]).is_err());
        assert!(args(&["--headless", "--audio", "x.mp3"]).is_err());
        assert!(args(&["--headless", "--latency-test"]).is_err());
        assert!(args(&["--headless", "--no-audio"]).is_err());
        // but --latency-test WITH an --audio file is legal (taps over music)
        assert!(args(&["--latency-test", "--audio", "x.mp3"]).is_ok());
    }

    #[test]
    fn volume_and_offset_bounds() {
        assert!(args(&["--volume", "1.0"]).is_ok());
        assert!(args(&["--volume", "1.5"]).is_err());
        assert!(args(&["--volume", "-0.1"]).is_err());
        assert!(args(&["--offset-ms", "10000"]).is_ok());
        assert!(args(&["--offset-ms", "10001"]).is_err());
        assert!(args(&["--offset-ms", "inf"]).is_err());
    }

    #[test]
    fn buffer_flag_defaults_and_parses() {
        // default: 256 (D-37)
        let a = args(&["--audio", "x.wav"]).unwrap();
        assert_eq!(a.buffer_frames, 256);
        // explicit values pass through, 0 = device default
        assert_eq!(args(&["--buffer", "128"]).unwrap().buffer_frames, 128);
        assert_eq!(args(&["--buffer", "0"]).unwrap().buffer_frames, 0);
        // out-of-range rejected
        assert!(args(&["--buffer", "8"]).is_err());
        assert!(args(&["--buffer", "99999"]).is_err());
        assert!(args(&["--buffer", "-4"]).is_err());
    }

    #[test]
    fn latency_test_flag_parses() {
        let a = args(&["--latency-test"]).unwrap();
        assert!(a.latency_test);
        assert_eq!(a.timeout, Some(DEFAULT_LIVE_TIMEOUT));
    }

    #[test]
    fn probe_default_timeout_is_short() {
        let a = args(&["--probe"]).unwrap();
        assert_eq!(a.timeout, Some(DEFAULT_PROBE_TIMEOUT));
    }

    #[test]
    fn headless_implies_testcard_and_no_timeout() {
        let a = args(&["--headless"]).unwrap();
        assert!(a.headless && a.testcard);
        assert_eq!(a.timeout, None);
    }

    #[test]
    fn explicit_timeout_wins_and_zero_disables() {
        let a = args(&["--timeout", "3.5"]).unwrap();
        assert_eq!(a.timeout, Some(Duration::from_secs_f64(3.5)));
        let a = args(&["--timeout", "0"]).unwrap();
        assert_eq!(a.timeout, None);
        // explicit 0 also beats the probe default
        let a = args(&["--probe", "--timeout", "0"]).unwrap();
        assert_eq!(a.timeout, None);
    }

    #[test]
    fn headless_options() {
        let a = args(&[
            "--headless",
            "--size",
            "1280x720",
            "--scale",
            "1.25",
            "--out",
            "/tmp/x.png",
        ])
        .unwrap();
        assert_eq!(a.size, (1280, 720));
        assert_eq!(a.scale, 1.25);
        assert_eq!(a.out, Some(PathBuf::from("/tmp/x.png")));
    }

    #[test]
    fn size_parsing() {
        assert!(args(&["--size", "1280x720"]).is_ok());
        assert!(args(&["--size", "720"]).is_err());
        assert!(args(&["--size", "0x720"]).is_err());
        assert!(args(&["--size", "ax720"]).is_err());
        assert!(args(&["--size", "99999x720"]).is_err());
    }

    #[test]
    fn scale_bounds() {
        assert!(args(&["--scale", "1.25"]).is_ok());
        assert!(args(&["--scale", "0.1"]).is_err());
        assert!(args(&["--scale", "5"]).is_err());
        assert!(args(&["--scale", "nan"]).is_err());
    }

    #[test]
    fn fps_bounds() {
        assert!(args(&["--fps", "60"]).is_ok());
        assert!(args(&["--fps", "0"]).is_err());
        assert!(args(&["--fps", "999"]).is_err());
    }

    #[test]
    fn unknown_flag_and_missing_value() {
        assert!(args(&["--frobnicate"]).is_err());
        assert!(args(&["--size"]).is_err());
    }

    #[test]
    fn timeout_must_be_sane() {
        assert!(args(&["--timeout", "-1"]).is_err());
        assert!(args(&["--timeout", "soon"]).is_err());
    }
}
