//! osu!-format beatmap (`.osu`) parser — CONTEXT.md §5.1.
//!
//! Pure and I/O-free (I-09): `parse_osu(&str) -> Result<Beatmap, ParseError>`
//! touches no filesystem, device or clock, so every rule here is
//! unit-testable with zero I/O.
//!
//! Grammar verified against the osu! wiki page ".osu (file format)"
//! (2026-11-13), which is the public spec this clean reimplementation
//! builds on (D-07):
//! - kiai time is `effects` **bit 0** (value 1); bit 3 (value 8) omits the
//!   first bar line — §5.1's earlier "bit 1" wording is corrected;
//! - a slider's first parameter field is `curveType|curvePoints` (pipes
//!   inside the first comma-separated field), followed by
//!   `slides,length,edgeSounds,edgeSets,hitSample` as separate fields;
//! - a perfect-circle (P) slider with more than three points degrades to
//!   bézier (consumed in phase 6, §7.3).
//!
//! Leniency (§5.1 "parse leniently"): UTF-8 BOM, CRLF, `//` comments, blank
//! lines, unknown `[Section]`s, unknown `key: value`s and unknown trailing
//! object fields are tolerated. Legacy fallbacks: missing `ApproachRate`
//! falls back to `OverallDifficulty` (pre-v8 behaviour), missing
//! `StackLeniency` defaults to 0.7, missing `AudioLeadIn`/`Mode` default to
//! 0. Coordinates are NOT range-checked — off-screen anchors occur in real
//! files.
//!
//! Rejected by evidence-first policy until a real specimen arrives (I-03
//! posture): timing-point lines with fewer than 7 comma fields (pre-v5
//! legacy) and osu!mania holds (type bit 7) fail with explicit errors.

use std::fmt;

/// Default `StackLeniency` when the file omits the key (legacy default).
pub const DEFAULT_STACK_LENIENCY: f32 = 0.7;

/// Default trailing hitSample when an object line omits it (wiki).
pub const DEFAULT_HIT_SAMPLE: &str = "0:0:0:0:";

/// A fully parsed `.osu` file.
#[derive(Debug, Clone, PartialEq)]
pub struct Beatmap {
    /// The `NN` in the `osu file format vNN` header (v128 = lazer writes).
    pub format_version: u32,
    pub general: General,
    pub metadata: Metadata,
    pub difficulty: Difficulty,
    /// File order preserved. The wiki promises chronological sorting; the
    /// parser does not sort, so a broken promise surfaces in phase-6
    /// lookups instead of being masked here.
    pub timing_points: Vec<TimingPoint>,
    pub hit_objects: Vec<HitObject>,
    /// Sections present in the file but unused by v0.1 (`Editor`, `Events`,
    /// `Colours`) plus any unknown ones — hardening statistics.
    pub skipped_sections: Vec<String>,
}

/// `[General]` section.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct General {
    /// Required — a map without it can neither play nor be imported.
    pub audio_filename: String,
    /// Milliseconds of silence before the audio starts (default 0).
    pub audio_lead_in_ms: f32,
    /// Defaults to [`DEFAULT_STACK_LENIENCY`] when the file omits the key.
    pub stack_leniency: f32,
    /// v0.1 accepts only 0 = osu!standard (D-12).
    pub mode: u8,
}

/// `[Metadata]` section.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Metadata {
    pub title: String,
    pub title_unicode: String,
    pub artist: String,
    pub artist_unicode: String,
    pub creator: String,
    /// Difficulty name.
    pub version: String,
    /// `-1` is common for unsubmitted maps; the caller decides what that
    /// means (e.g. import under a hash-derived directory).
    pub beatmap_id: i64,
    pub beatmap_set_id: i64,
}

/// `[Difficulty]` section. `ApproachRate` falls back to
/// `overall_difficulty` for pre-v8 files that lack the key.
#[derive(Debug, Clone, PartialEq)]
pub struct Difficulty {
    pub hp_drain_rate: f32,
    pub circle_size: f32,
    pub overall_difficulty: f32,
    pub approach_rate: f32,
    /// Base slider velocity in hundreds of osu! pixels per beat.
    pub slider_multiplier: f32,
    pub slider_tick_rate: f32,
}

/// One `[TimingPoints]` line.
#[derive(Debug, Clone, PartialEq)]
pub struct TimingPoint {
    pub time_ms: f32,
    /// Uninherited: ms per beat. Inherited: negative inverse slider-velocity
    /// multiplier (e.g. -50 → 2× speed, §5.1).
    pub beat_length: f32,
    /// Beats per measure (inherited points ignore it).
    pub meter: u32,
    /// Volume percent 0–100.
    pub volume: f32,
    pub uninherited: bool,
    /// `effects` bit 0 — wiki-verified 2026-11-13.
    pub kiai: bool,
    /// `effects` bit 3.
    pub omit_first_bar_line: bool,
}

impl TimingPoint {
    /// Slider-velocity multiplier carried by this point (§5.1): uninherited
    /// points carry none (1.0); inherited points encode it as
    /// `-100 / beat_length`.
    ///
    /// A malformed inherited point (`beat_length == 0`) yields ±inf —
    /// semantic validation belongs to the caller, not the parser.
    pub fn sv_multiplier(&self) -> f32 {
        if self.uninherited {
            1.0
        } else {
            -100.0 / self.beat_length
        }
    }
}

/// Slider curve type (the letter before the first `|`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurveType {
    /// Bézier; repeated control points mark segment breaks (§7.3).
    Bezier,
    /// Centripetal Catmull-Rom.
    Catmull,
    /// Straight line through all points.
    Linear,
    /// Perfect circle through at most 3 points; more degrade to bézier.
    Perfect,
}

/// Slider body, exactly as parsed (path construction is phase 6, §7.3).
#[derive(Debug, Clone, PartialEq)]
pub struct Slider {
    pub curve_type: CurveType,
    /// Anchor points after the slider head, duplicates preserved — a
    /// repeated point is a bézier segment break, not redundant data.
    pub curve_points: Vec<(f32, f32)>,
    /// Traversals back-and-forth (repeats + 1, wiki).
    pub slides: u32,
    /// Visual length in osu! pixels.
    pub length: f32,
    /// Raw pipe-list edge hitsound bitmasks, e.g. `2|1|2` ("" when absent);
    /// parsed in phase 4 when hitsounds exist.
    pub edge_sounds: String,
    /// Raw pipe-list edge sample sets, e.g. `0:0|0:0|0:2` ("" when absent).
    pub edge_sets: String,
}

/// What the object is.
#[derive(Debug, Clone, PartialEq)]
pub enum HitObjectKind {
    Circle,
    Slider(Slider),
    Spinner { end_time_ms: f32 },
}

/// One `[HitObjects]` line.
#[derive(Debug, Clone, PartialEq)]
pub struct HitObject {
    /// osu!px, NOT range-checked — off-screen anchors occur in real files.
    pub x: f32,
    pub y: f32,
    pub time_ms: f32,
    /// Type bit 2 (value 4).
    pub new_combo: bool,
    /// 3-bit combo-colour skip from type bits 4–6; only meaningful with
    /// `new_combo`.
    pub combo_colour_skip: u8,
    pub kind: HitObjectKind,
    /// Raw hitSound bitmask (bit 0 normal, 1 whistle, 2 finish, 3 clap).
    pub hit_sound: u8,
    /// Raw trailing hitSample field; [`DEFAULT_HIT_SAMPLE`] when the file
    /// omits it.
    pub hit_sample: String,
}

/// Parse error with the offending line for self-diagnosing logs.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    /// 1-based line number; 0 for whole-file errors.
    pub line_no: usize,
    /// Trimmed offending line ("" for whole-file errors).
    pub line: String,
    pub reason: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line.is_empty() {
            write!(f, "beatmap parse error: {}", self.reason)
        } else {
            write!(
                f,
                "beatmap parse error at line {}: {} — `{}`",
                self.line_no, self.reason, self.line
            )
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a complete `.osu` file from text. Zero I/O (I-09).
pub fn parse_osu(text: &str) -> Result<Beatmap, ParseError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let mut format_version: Option<u32> = None;
    let mut section = Section::None;
    let mut general = General {
        stack_leniency: DEFAULT_STACK_LENIENCY,
        ..General::default()
    };
    let mut metadata = Metadata::default();
    let mut timing_points: Vec<TimingPoint> = Vec::new();
    let mut hit_objects: Vec<HitObject> = Vec::new();
    let mut skipped_sections: Vec<String> = Vec::new();
    // hp, cs, od, slider_multiplier, slider_tick_rate — ApproachRate is the
    // optional one (legacy fallback to OverallDifficulty).
    let mut diff: [Option<f32>; 5] = [None; 5];
    let mut approach_rate: Option<f32> = None;
    let mut seen_audio_filename = false;
    let mut seen_difficulty_section = false;

    for (idx, raw) in text.split('\n').enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if format_version.is_none() {
            let Some(rest) = line.strip_prefix("osu file format v") else {
                return Err(err(
                    line_no,
                    line,
                    "expected `osu file format vNN` as the first line",
                ));
            };
            let v = rest.trim().parse::<u32>().map_err(|_| {
                err(
                    line_no,
                    line,
                    "file version must be a number, e.g. `osu file format v14`",
                )
            })?;
            format_version = Some(v);
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = match name.trim() {
                "General" => Section::General,
                "Metadata" => Section::Metadata,
                "Difficulty" => {
                    seen_difficulty_section = true;
                    Section::Difficulty
                }
                "TimingPoints" => Section::TimingPoints,
                "HitObjects" => Section::HitObjects,
                other => {
                    skipped_sections.push(other.to_owned());
                    Section::Skip
                }
            };
            continue;
        }
        match section {
            Section::None => {
                return Err(err(
                    line_no,
                    line,
                    "content before the first `[Section]` header",
                ));
            }
            Section::General => {
                let Some((key, value)) = line.split_once(':') else {
                    return Err(err(line_no, line, "expected `key: value` in [General]"));
                };
                match key.trim() {
                    "AudioFilename" => {
                        general.audio_filename = value.trim().to_owned();
                        seen_audio_filename = true;
                    }
                    "AudioLeadIn" if !value.trim().is_empty() => {
                        general.audio_lead_in_ms =
                            parse_f32(value).map_err(|e| err(line_no, line, e))?;
                    }
                    "StackLeniency" if !value.trim().is_empty() => {
                        general.stack_leniency =
                            parse_f32(value).map_err(|e| err(line_no, line, e))?;
                    }
                    "Mode" if !value.trim().is_empty() => {
                        general.mode = parse_u8(value).map_err(|e| err(line_no, line, e))?;
                    }
                    _ => {}
                }
            }
            Section::Metadata => {
                let Some((key, value)) = line.split_once(':') else {
                    return Err(err(line_no, line, "expected `key: value` in [Metadata]"));
                };
                let value = value.trim();
                match key.trim() {
                    "Title" => metadata.title = value.to_owned(),
                    "TitleUnicode" => metadata.title_unicode = value.to_owned(),
                    "Artist" => metadata.artist = value.to_owned(),
                    "ArtistUnicode" => metadata.artist_unicode = value.to_owned(),
                    "Creator" => metadata.creator = value.to_owned(),
                    "Version" => metadata.version = value.to_owned(),
                    "BeatmapID" if !value.is_empty() => {
                        metadata.beatmap_id =
                            parse_int(value).map_err(|e| err(line_no, line, e))?;
                    }
                    "BeatmapSetID" if !value.is_empty() => {
                        metadata.beatmap_set_id =
                            parse_int(value).map_err(|e| err(line_no, line, e))?;
                    }
                    _ => {}
                }
            }
            Section::Difficulty => {
                let Some((key, value)) = line.split_once(':') else {
                    return Err(err(line_no, line, "expected `key: value` in [Difficulty]"));
                };
                let v = || parse_f32(value).map_err(|e| err(line_no, line, e));
                match key.trim() {
                    "HPDrainRate" => diff[0] = Some(v()?),
                    "CircleSize" => diff[1] = Some(v()?),
                    "OverallDifficulty" => diff[2] = Some(v()?),
                    "ApproachRate" => approach_rate = Some(v()?),
                    "SliderMultiplier" => diff[3] = Some(v()?),
                    "SliderTickRate" => diff[4] = Some(v()?),
                    _ => {}
                }
            }
            Section::TimingPoints => {
                timing_points.push(parse_timing_point(line_no, line)?);
            }
            Section::HitObjects => {
                hit_objects.push(parse_hit_object(line_no, line)?);
            }
            Section::Skip => {}
        }
    }

    let format_version =
        format_version.ok_or_else(|| err(0, "", "file has no `osu file format vNN` header"))?;
    if !seen_audio_filename {
        return Err(err(0, "", "missing required [General] key `AudioFilename`"));
    }
    if general.mode != 0 {
        return Err(err(
            0,
            "",
            format!(
                "Mode {} is not supported — v0.1 accepts only Mode 0 (osu!standard, D-12)",
                general.mode
            ),
        ));
    }
    if !seen_difficulty_section {
        return Err(err(0, "", "file has no [Difficulty] section"));
    }
    let required = [
        ("HPDrainRate", diff[0]),
        ("CircleSize", diff[1]),
        ("OverallDifficulty", diff[2]),
        ("SliderMultiplier", diff[3]),
        ("SliderTickRate", diff[4]),
    ];
    let missing: Vec<&str> = required
        .iter()
        .filter(|(_, o)| o.is_none())
        .map(|(k, _)| *k)
        .collect();
    if !missing.is_empty() {
        return Err(err(
            0,
            "",
            format!("missing required [Difficulty] keys: {}", missing.join(", ")),
        ));
    }
    let [hp, cs, od, sm, st] = diff;
    let difficulty = Difficulty {
        hp_drain_rate: hp.unwrap(),
        circle_size: cs.unwrap(),
        overall_difficulty: od.unwrap(),
        // Legacy fallback: pre-v8 files carry no ApproachRate; osu! then
        // uses OverallDifficulty. Pinned as an assumption until a real
        // old-format file arrives in phase-3 hardening.
        approach_rate: approach_rate.unwrap_or(od.unwrap()),
        slider_multiplier: sm.unwrap(),
        slider_tick_rate: st.unwrap(),
    };

    Ok(Beatmap {
        format_version,
        general,
        metadata,
        difficulty,
        timing_points,
        hit_objects,
        skipped_sections,
    })
}

/// Section state while scanning lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    General,
    Metadata,
    Difficulty,
    TimingPoints,
    HitObjects,
    Skip,
}

fn err(line_no: usize, line: &str, reason: impl Into<String>) -> ParseError {
    ParseError {
        line_no,
        line: line.into(),
        reason: reason.into(),
    }
}

/// Lenient number: accepts int/float text, rejects NaN/inf.
fn parse_f32(s: &str) -> Result<f32, String> {
    let t = s.trim();
    let v: f32 = t.parse().map_err(|_| format!("`{t}` is not a number"))?;
    if !v.is_finite() {
        return Err(format!("`{t}` is not finite"));
    }
    Ok(v)
}

/// Lenient integer: accepts `2` and `2.0`, rejects fractions/NaN/inf.
fn parse_int(s: &str) -> Result<i64, String> {
    let t = s.trim();
    let v: f64 = t.parse().map_err(|_| format!("`{t}` is not a number"))?;
    if !v.is_finite() {
        return Err(format!("`{t}` is not finite"));
    }
    if v.fract() != 0.0 {
        return Err(format!("`{t}` is not an integer"));
    }
    Ok(v as i64)
}

fn parse_u32(s: &str) -> Result<u32, String> {
    let v = parse_int(s)?;
    u32::try_from(v).map_err(|_| format!("`{}` is out of u32 range", s.trim()))
}

fn parse_u8(s: &str) -> Result<u8, String> {
    let v = parse_int(s)?;
    u8::try_from(v).map_err(|_| format!("`{}` is out of u8 range", s.trim()))
}

fn parse_timing_point(line_no: usize, line: &str) -> Result<TimingPoint, ParseError> {
    let e = |reason: String| err(line_no, line, reason);
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 7 || parts.len() > 8 {
        return Err(e(format!(
            "timing point needs 7 (legacy) or 8 comma fields, got {} — fewer-field \
             legacy lines are rejected pending a real specimen",
            parts.len()
        )));
    }
    let time_ms = parse_f32(parts[0]).map_err(e)?;
    let beat_length = parse_f32(parts[1]).map_err(e)?;
    let meter = if parts[2].trim().is_empty() {
        4
    } else {
        parse_u32(parts[2]).map_err(e)?
    };
    // parts[3] (sampleSet) and parts[4] (sampleIndex) are consumed but not
    // stored — v0.1 gameplay reads sample sets from hit objects only.
    let volume = if parts[5].trim().is_empty() {
        100.0
    } else {
        parse_f32(parts[5]).map_err(e)?
    };
    let uninherited = parse_int(parts[6]).map_err(e)? != 0;
    let effects = if parts.len() == 8 {
        parse_int(parts[7]).map_err(e)?
    } else {
        0
    };
    Ok(TimingPoint {
        time_ms,
        beat_length,
        meter,
        volume,
        uninherited,
        // Wiki (verified 2026-11-13): bit 0 = kiai, bit 3 = omit first bar
        // line — §5.1's earlier "bit 1" wording was wrong.
        kiai: (effects & 1) != 0,
        omit_first_bar_line: (effects & 8) != 0,
    })
}

fn parse_hit_object(line_no: usize, line: &str) -> Result<HitObject, ParseError> {
    let e = |reason: String| err(line_no, line, reason);
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 5 {
        return Err(e(format!(
            "hit object needs at least 5 comma fields (x,y,time,type,hitSound), got {}",
            parts.len()
        )));
    }
    let x = parse_f32(parts[0]).map_err(e)?;
    let y = parse_f32(parts[1]).map_err(e)?;
    let time_ms = parse_f32(parts[2]).map_err(e)?;
    let ty = parse_int(parts[3]).map_err(e)?;
    if !(0..=255).contains(&ty) {
        return Err(e(format!("type `{ty}` is out of the 8-bit range")));
    }
    let hit_sound = parse_u8(parts[4]).map_err(e)?;
    if (ty & 128) != 0 {
        return Err(e(
            "osu!mania hold notes (type bit 7) are not valid in a Mode-0 map".to_owned(),
        ));
    }
    let is_circle = (ty & 1) != 0;
    let is_slider = (ty & 2) != 0;
    let is_spinner = (ty & 8) != 0;
    if [is_circle, is_slider, is_spinner]
        .iter()
        .filter(|b| **b)
        .count()
        != 1
    {
        return Err(e(format!(
            "type `{ty}` must set exactly one of the circle (1) / slider (2) / spinner (8) bits"
        )));
    }
    let sample_field = |idx: usize| {
        parts
            .get(idx)
            .filter(|s| !s.trim().is_empty())
            .map(|s| (*s).to_owned())
    };
    let (kind, hit_sample) = if is_circle {
        (HitObjectKind::Circle, sample_field(5))
    } else if is_slider {
        let slider = parse_slider(line_no, line, &parts)?;
        (HitObjectKind::Slider(slider), sample_field(10))
    } else {
        let Some(end) = parts.get(5) else {
            return Err(e("spinner needs an endTime field".to_owned()));
        };
        let end_time_ms = parse_f32(end).map_err(e)?;
        (HitObjectKind::Spinner { end_time_ms }, sample_field(6))
    };
    Ok(HitObject {
        x,
        y,
        time_ms,
        new_combo: (ty & 4) != 0,
        combo_colour_skip: ((ty >> 4) & 7) as u8,
        kind,
        hit_sound,
        hit_sample: hit_sample.unwrap_or_else(|| DEFAULT_HIT_SAMPLE.to_owned()),
    })
}

fn parse_slider(line_no: usize, line: &str, parts: &[&str]) -> Result<Slider, ParseError> {
    let e = |reason: String| err(line_no, line, reason);
    if parts.len() < 8 {
        return Err(e(format!(
            "slider needs at least 8 comma fields (x,y,time,type,hitSound,curve,slides,length), \
             got {}",
            parts.len()
        )));
    }
    let Some((curve_ty, pts)) = parts[5].split_once('|') else {
        return Err(e(
            "slider field 6 must be `curveType|curvePoints`".to_owned()
        ));
    };
    let curve_type = match curve_ty.trim() {
        "B" => CurveType::Bezier,
        "C" => CurveType::Catmull,
        "L" => CurveType::Linear,
        "P" => CurveType::Perfect,
        other => {
            return Err(e(format!(
                "unknown curve type `{other}` (accepted: B, C, L, P)"
            )));
        }
    };
    let mut curve_points = Vec::new();
    for p in pts.split('|') {
        if p.trim().is_empty() {
            continue;
        }
        let Some((px, py)) = p.split_once(':') else {
            return Err(e(format!("curve point `{p}` must be `x:y`")));
        };
        curve_points.push((parse_f32(px).map_err(e)?, parse_f32(py).map_err(e)?));
    }
    if curve_points.is_empty() {
        return Err(e("slider has no curve points".to_owned()));
    }
    let slides = parse_u32(parts[6]).map_err(e)?;
    if slides == 0 {
        return Err(e("slider slides must be >= 1".to_owned()));
    }
    let length = parse_f32(parts[7]).map_err(e)?;
    if length < 0.0 {
        return Err(e("slider length must be >= 0".to_owned()));
    }
    let raw_field = |idx: usize| {
        parts
            .get(idx)
            .filter(|s| !s.trim().is_empty())
            .map(|s| (*s).to_owned())
            .unwrap_or_default()
    };
    Ok(Slider {
        curve_type,
        curve_points,
        slides,
        length,
        edge_sounds: raw_field(8),
        edge_sets: raw_field(9),
    })
}

/// File-level tests over the committed golden corpus (`assets/golden/`);
/// reading files is test-only I/O — the parser stays pure (I-09).
#[cfg(test)]
mod golden;

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal complete `.osu` scaffold. `difficulty`, `timing` and
    /// `objects` replace the whole section bodies so tests can vary them.
    fn file(difficulty: &str, timing: &str, objects: &str) -> String {
        format!(
            "osu file format v14\n\
             \n\
             [General]\n\
             AudioFilename: audio.mp3\n\
             AudioLeadIn: 0\n\
             StackLeniency: 0.7\n\
             Mode: 0\n\
             \n\
             [Metadata]\n\
             Title: Golden Fixture\n\
             TitleUnicode: Golden Fixture\n\
             Artist: hyprbeat\n\
             ArtistUnicode: hyprbeat\n\
             Creator: hyprbeat\n\
             Version: Normal\n\
             BeatmapID: 42\n\
             BeatmapSetID: 7\n\
             \n\
             [Difficulty]\n\
             {difficulty}\n\
             \n\
             [TimingPoints]\n\
             {timing}\n\
             \n\
             [HitObjects]\n\
             {objects}\n"
        )
    }

    fn diff_full() -> &'static str {
        "HPDrainRate: 5\n\
         CircleSize: 4\n\
         OverallDifficulty: 8\n\
         ApproachRate: 9\n\
         SliderMultiplier: 1.8\n\
         SliderTickRate: 1"
    }

    const TIMING_ONE: &str = "11000,333.33,4,0,0,50,1,0";

    #[test]
    fn minimal_file_parses() {
        let map = parse_osu(&file(diff_full(), TIMING_ONE, "256,192,11000,21,2")).unwrap();
        assert_eq!(map.format_version, 14);
        assert_eq!(map.general.audio_filename, "audio.mp3");
        assert_eq!(map.general.audio_lead_in_ms, 0.0);
        assert_eq!(map.general.stack_leniency, 0.7);
        assert_eq!(map.general.mode, 0);
        assert_eq!(map.metadata.title, "Golden Fixture");
        assert_eq!(map.metadata.beatmap_id, 42);
        assert_eq!(map.metadata.beatmap_set_id, 7);
        assert_eq!(map.difficulty.hp_drain_rate, 5.0);
        assert_eq!(map.difficulty.circle_size, 4.0);
        assert_eq!(map.difficulty.overall_difficulty, 8.0);
        assert_eq!(map.difficulty.approach_rate, 9.0);
        assert_eq!(map.difficulty.slider_multiplier, 1.8);
        assert_eq!(map.difficulty.slider_tick_rate, 1.0);
        assert_eq!(map.timing_points.len(), 1);
        assert_eq!(map.hit_objects.len(), 1);
        assert_eq!(map.skipped_sections, Vec::<String>::new());
    }

    #[test]
    fn wiki_example_objects_parse_exactly() {
        // The three hit-object examples from the public osu! wiki page
        // ".osu (file format)".
        let map = parse_osu(&file(
            diff_full(),
            TIMING_ONE,
            "256,192,11000,21,2\n\
             256,192,11200,8,12,12000,3:0:0:80:\n\
             100,100,12600,6,1,B|200:200|250:200|250:200|300:150,2,310.123,2|1|2,0:0|0:0|0:2,\
             0:0:0:0:",
        ))
        .unwrap();
        assert_eq!(map.hit_objects.len(), 3);

        // Circle: type 21 = 1 (circle) + 4 (new combo) + 16 (skip 1 colour).
        let HitObjectKind::Circle = map.hit_objects[0].kind else {
            panic!("expected a circle");
        };
        assert!(map.hit_objects[0].new_combo);
        assert_eq!(map.hit_objects[0].combo_colour_skip, 1);
        assert_eq!(map.hit_objects[0].hit_sound, 2);
        assert_eq!(map.hit_objects[0].hit_sample, DEFAULT_HIT_SAMPLE);

        // Spinner: endTime then optional hitSample.
        let HitObjectKind::Spinner { end_time_ms } = map.hit_objects[1].kind else {
            panic!("expected a spinner");
        };
        assert_eq!(end_time_ms, 12000.0);
        assert_eq!(map.hit_objects[1].hit_sound, 12);
        assert_eq!(map.hit_objects[1].hit_sample, "3:0:0:80:");

        // Slider: type 6 = 2 + 4 (slider + new combo); bézier with a
        // DUPLICATE anchor (segment break, §7.3) that must be preserved.
        let HitObjectKind::Slider(s) = &map.hit_objects[2].kind else {
            panic!("expected a slider");
        };
        assert_eq!(s.curve_type, CurveType::Bezier);
        assert_eq!(
            s.curve_points,
            vec![
                (200.0, 200.0),
                (250.0, 200.0),
                (250.0, 200.0),
                (300.0, 150.0),
            ]
        );
        assert_eq!(s.slides, 2);
        assert_eq!(s.length, 310.123);
        assert_eq!(s.edge_sounds, "2|1|2");
        assert_eq!(s.edge_sets, "0:0|0:0|0:2");
        assert_eq!(map.hit_objects[2].hit_sample, "0:0:0:0:");
        assert!(map.hit_objects[2].new_combo);
    }

    #[test]
    fn all_curve_types_parse() {
        let map = parse_osu(&file(
            diff_full(),
            TIMING_ONE,
            "0,0,0,2,0,L|100:0|200:0,1,100\n\
             0,0,100,2,0,C|100:0|200:0,1,100\n\
             0,0,200,2,0,P|100:0|200:0,1,100",
        ))
        .unwrap();
        let types: Vec<CurveType> = map
            .hit_objects
            .iter()
            .map(|o| match &o.kind {
                HitObjectKind::Slider(s) => s.curve_type,
                _ => unreachable!("only sliders in this fixture"),
            })
            .collect();
        assert_eq!(
            types,
            vec![CurveType::Linear, CurveType::Catmull, CurveType::Perfect]
        );
    }

    #[test]
    fn missing_approach_rate_falls_back_to_overall_difficulty() {
        let diff_no_ar = "HPDrainRate: 5\n\
             CircleSize: 4\n\
             OverallDifficulty: 8\n\
             SliderMultiplier: 1.4\n\
             SliderTickRate: 2";
        let map = parse_osu(&file(diff_no_ar, TIMING_ONE, "")).unwrap();
        assert_eq!(
            map.difficulty.approach_rate,
            map.difficulty.overall_difficulty
        );
    }

    #[test]
    fn bom_crlf_and_comments_are_tolerated() {
        let clean = file(diff_full(), TIMING_ONE, "10,20,11000,1,0");
        let messy = format!(
            "\u{feff}{}",
            clean.replace('\n', "\r\n").trim_end_matches('\r')
        );
        // A `//` comment and blank lines inside the last section.
        let messy = format!("{messy}\r\n// editor junk\r\n\r\n");
        assert_eq!(parse_osu(&clean).unwrap(), parse_osu(&messy).unwrap());
    }

    #[test]
    fn non_standard_mode_is_rejected() {
        let text = file(diff_full(), TIMING_ONE, "").replace("Mode: 0", "Mode: 3");
        let e = parse_osu(&text).unwrap_err();
        assert!(e.reason.contains("Mode 3"), "reason: {}", e.reason);
        assert!(e.reason.contains("D-12"));
    }

    #[test]
    fn mania_hold_is_rejected() {
        // Wiki hold syntax: `x,y,time,type,hitSound,endTime:hitSample`.
        let e = parse_osu(&file(
            diff_full(),
            TIMING_ONE,
            "100,192,11000,132,1,11050:0:0:0:",
        ))
        .unwrap_err();
        assert!(e.reason.contains("mania"), "reason: {}", e.reason);
    }

    #[test]
    fn timing_point_kiai_is_effect_bit_zero() {
        let map = parse_osu(&file(
            diff_full(),
            "0,400,4,0,0,80,1,1\n\
             100,400,4,0,0,80,1,8\n\
             200,-50,4,0,0,80,0,9",
            "",
        ))
        .unwrap();
        assert!(map.timing_points[0].kiai);
        assert!(!map.timing_points[0].omit_first_bar_line);
        assert!(!map.timing_points[1].kiai);
        assert!(map.timing_points[1].omit_first_bar_line);
        assert!(map.timing_points[2].kiai);
        assert!(map.timing_points[2].omit_first_bar_line);
        // §5.1: inherited beatLength -50 → 2× slider velocity.
        assert_eq!(map.timing_points[2].sv_multiplier(), 2.0);
        assert_eq!(map.timing_points[0].sv_multiplier(), 1.0);
    }

    #[test]
    fn seven_field_legacy_timing_point_has_no_effects() {
        let map = parse_osu(&file(diff_full(), "0,400,4,0,0,80,1", "")).unwrap();
        let tp = &map.timing_points[0];
        assert!(!tp.kiai && !tp.omit_first_bar_line);
        assert_eq!(tp.meter, 4);
        assert_eq!(tp.volume, 80.0);
    }

    #[test]
    fn six_field_timing_point_is_rejected_for_now() {
        let e = parse_osu(&file(diff_full(), "0,400,4,0,0,80", "")).unwrap_err();
        assert!(e.reason.contains("7 (legacy) or 8"), "reason: {}", e.reason);
    }

    #[test]
    fn timing_point_file_order_is_preserved() {
        let map = parse_osu(&file(
            diff_full(),
            "500,400,4,0,0,80,1,0\n0,300,4,0,0,80,1,0",
            "",
        ))
        .unwrap();
        assert_eq!(map.timing_points[0].time_ms, 500.0);
        assert_eq!(map.timing_points[1].time_ms, 0.0);
    }

    #[test]
    fn unknown_and_unused_sections_are_recorded_and_skipped() {
        let mut text = file(diff_full(), TIMING_ONE, "");
        text.push_str("\n[Editor]\n// junk\nDistanceSpacing: 1.5\n");
        text.push_str("\n[Events]\n//Background and Video events\n0,0,bg.jpg,0,0\n");
        text.push_str("\n[Colours]\nCombo1: 255,128,64\n");
        text.push_str("\n[NotARealSection]\ngarbage,goes,here\n");
        let map = parse_osu(&text).unwrap();
        assert_eq!(
            map.skipped_sections,
            vec!["Editor", "Events", "Colours", "NotARealSection"]
        );
    }

    #[test]
    fn missing_difficulty_keys_are_named() {
        let e = parse_osu(&file(
            "HPDrainRate: 5\nOverallDifficulty: 8",
            TIMING_ONE,
            "",
        ))
        .unwrap_err();
        assert!(e.reason.contains("CircleSize"), "reason: {}", e.reason);
        assert!(e.reason.contains("SliderMultiplier"));
        assert!(e.reason.contains("SliderTickRate"));
    }

    #[test]
    fn missing_audio_filename_is_an_error() {
        let text = file(diff_full(), TIMING_ONE, "").replace("AudioFilename: audio.mp3\n", "");
        let e = parse_osu(&text).unwrap_err();
        assert!(e.reason.contains("AudioFilename"), "reason: {}", e.reason);
    }

    #[test]
    fn missing_difficulty_section_is_an_error() {
        let text = "osu file format v14\n[General]\nAudioFilename: a.mp3\nMode: 0\n[HitObjects]\n";
        let e = parse_osu(text).unwrap_err();
        assert!(e.reason.contains("[Difficulty]"), "reason: {}", e.reason);
    }

    #[test]
    fn garbage_hit_object_line_reports_the_line() {
        // The object line is line 31 of the scaffold.
        let e = parse_osu(&file(diff_full(), TIMING_ONE, "this is,not an,object")).unwrap_err();
        assert_eq!(e.line_no, 31);
        assert_eq!(e.line, "this is,not an,object");
    }

    #[test]
    fn slider_missing_length_field_is_rejected() {
        let e = parse_osu(&file(diff_full(), TIMING_ONE, "0,0,0,2,0,B|100:0,1")).unwrap_err();
        assert!(
            e.reason.contains("slider needs at least 8"),
            "reason: {}",
            e.reason
        );
    }

    #[test]
    fn slider_without_curve_points_is_rejected() {
        let e = parse_osu(&file(diff_full(), TIMING_ONE, "0,0,0,2,0,B|,1,100")).unwrap_err();
        assert!(e.reason.contains("no curve points"), "reason: {}", e.reason);
    }

    #[test]
    fn type_without_kind_bit_is_rejected() {
        let e = parse_osu(&file(diff_full(), TIMING_ONE, "1,2,10,4,0")).unwrap_err();
        assert!(e.reason.contains("exactly one"), "reason: {}", e.reason);
    }

    #[test]
    fn empty_sections_are_valid() {
        let map = parse_osu(&file(diff_full(), "", "")).unwrap();
        assert!(map.hit_objects.is_empty());
        assert!(map.timing_points.is_empty());
    }

    #[test]
    fn stack_leniency_defaults_when_absent() {
        let text = file(diff_full(), TIMING_ONE, "").replace("StackLeniency: 0.7\n", "");
        let map = parse_osu(&text).unwrap();
        assert_eq!(map.general.stack_leniency, DEFAULT_STACK_LENIENCY);
    }

    #[test]
    fn whitespace_in_key_value_lines_is_tolerated() {
        let text = file(diff_full(), TIMING_ONE, "").replace("CircleSize: 4", "CircleSize:  4 ");
        let map = parse_osu(&text).unwrap();
        assert_eq!(map.difficulty.circle_size, 4.0);
    }

    #[test]
    fn missing_or_wrong_header_is_an_error() {
        assert!(parse_osu("[General]\nAudioFilename: a.mp3\n").is_err());
        let e = parse_osu("osu file format vabc\n").unwrap_err();
        assert!(e.reason.contains("file version"), "reason: {}", e.reason);
    }

    #[test]
    fn lazer_version_number_is_parsed_but_kept() {
        let text = file(diff_full(), TIMING_ONE, "")
            .replace("osu file format v14", "osu file format v128");
        assert_eq!(parse_osu(&text).unwrap().format_version, 128);
    }

    #[test]
    fn content_before_first_section_is_rejected() {
        let e = parse_osu("osu file format v14\nstray line\n[General]\n").unwrap_err();
        assert!(
            e.reason.contains("before the first"),
            "reason: {}",
            e.reason
        );
    }

    #[test]
    fn unsubmitted_map_negative_ids_parse() {
        let text = file(diff_full(), TIMING_ONE, "").replace("BeatmapSetID: 7", "BeatmapSetID: -1");
        assert_eq!(parse_osu(&text).unwrap().metadata.beatmap_set_id, -1);
    }

    #[test]
    fn error_display_includes_line_and_reason() {
        let e = parse_osu(&file(diff_full(), TIMING_ONE, "not-an-object")).unwrap_err();
        let text = e.to_string();
        assert!(text.contains("line 31"), "display: {text}");
        assert!(text.contains("not-an-object"));
    }
}
