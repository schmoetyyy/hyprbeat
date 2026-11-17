//! Visualizer loader (§10 row 3, commit a) — the FIRST live consumer of
//! the D-45 `resolve_set` loader (its §9 row says so verbatim: "the
//! visualizer's loader is its first consumer"). The path is
//! set-key → manifest → hashes → canonical paths → parsed difficulty →
//! [`Timeline`]; NOTHING here re-implements index resolution (D-45
//! landed in commit 6) — this module only selects among what
//! [`LibraryIndex::resolve_set`] returns.
//!
//! Two entry points, one per flag (user ruling 2026-11-17: the strict
//! per-flag split — the previous delivery's shared probing resolver
//! was the reason Δ-H was withdrawn; no path here resolves both
//! spellings):
//! - [`load_set`] — the `--set KEY` form: a library set key (`<set-id>`
//!   or `unsubmitted/<hash8>`, the D-43 routing keys) resolved through
//!   the D-45 manifest ONLY — the key is never probed against the
//!   filesystem, so a file named `KEY` in the working directory cannot
//!   hijack the lookup;
//! - [`load_file`] — the `--map PATH` form (§6 dev testing): ONE direct
//!   `.osu` file that must exist and carry the `.osu` extension — a
//!   set-key spelling is never reinterpreted.
//!
//! Difficulty selection (user ruling 2026-11-17): `--diff NAME` is an
//! EXACT match on the difficulty name; otherwise the FIRST difficulty
//! (manifest order) is used and a warning lists ALL alternatives.
//!
//! Non-standard modes and unparseable diffs are skipped with a warn — a
//! set may mix game modes under one `BeatmapSetID`, and `parse_osu`
//! rejects non-standard modes by design (D-12) — the load fails only
//! when nothing playable remains.

use std::fmt;
use std::path::{Path, PathBuf};

use super::import::normalize_rel;
use super::osz::{LibraryError, LibraryIndex, LibraryPaths, default_paths, paths_from};
use super::timeline::{Timeline, TimelineError};
use super::{Beatmap, parse_osu};

/// How the map was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapSource {
    /// A library set key resolved through the D-45 manifest.
    Set { set_key: String },
    /// A direct `.osu` path (§6 dev testing).
    File { path: PathBuf },
}

/// One loaded difficulty, ready for the renderer (visualizer commit b).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedMap {
    pub source: MapSource,
    /// The chosen difficulty's entry name (set form) or file name.
    pub entry_name: String,
    /// Every PLAYABLE difficulty name in the set, manifest order —
    /// empty for the direct-file form. Skipped entries are not listed:
    /// they are not selectable.
    pub difficulties: Vec<String>,
    pub beatmap: Beatmap,
    pub timeline: Timeline,
    /// Canonical audio path from the set manifest (set form only;
    /// `None` when the map's audio file is not a manifest entry — the
    /// golden corpus's placeholder audio does this — or for the
    /// direct-file form). Commit b's `--audio` wiring consumes it; a
    /// `None` here never fails the load.
    pub audio: Option<PathBuf>,
}

/// Loader errors. Everything except `Library`, `OsuFile` and `Timeline`
/// is decided before any rendering exists to do.
#[derive(Debug)]
pub enum LoadError {
    /// Index/store failure from [`super::osz`] — surfaced untouched.
    Library(LibraryError),
    /// No `--library-root` and the environment resolves no XDG data
    /// dir — mirrors `ImportError::NoDataDir`.
    NoDataDir,
    /// `--set` names no set key in the index. `available` lists every
    /// set key; empty when the index carries no manifests at all (a
    /// v1-era index — one re-import heals, D-45).
    UnknownSet {
        asked: String,
        available: Vec<String>,
    },
    /// Every `.osu` in the set was skipped (parse error or non-standard
    /// mode) — nothing playable. `skipped` carries (entry, reason).
    NoPlayableDifficulty {
        set_key: String,
        skipped: Vec<(String, String)>,
    },
    /// The direct-path form must name an `.osu` file (`.osz` archives
    /// go through `--import` first).
    NotAnOsuFile { path: PathBuf },
    /// Reading or parsing the directly-given `.osu` failed.
    OsuFile { path: PathBuf, reason: String },
    /// `--diff` with the direct-file form: one file is one difficulty,
    /// there is nothing to select.
    DiffOnFileForm { wanted: String, only: String },
    /// The chosen difficulty has no hit objects — nothing to visualize.
    NoHitObjects { version: String },
    /// The timeline could not be built from the chosen difficulty.
    Timeline {
        version: String,
        source: TimelineError,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Library(e) => write!(f, "library: {e}"),
            Self::NoDataDir => write!(
                f,
                "no XDG data dir resolves for the library (HOME/XDG_DATA_HOME unset?) \
                 — pass --library-root DIR"
            ),
            Self::UnknownSet { asked, available } => {
                if available.is_empty() {
                    write!(
                        f,
                        "`--set {asked}` is not a set key in the library index; the \
                         index has no set manifests at all \
                         (v1-era index? re-import your .osz files — D-45)"
                    )
                } else {
                    write!(
                        f,
                        "`--set {asked}` is not a set key in the library index; \
                         available sets: {}",
                        available.join(", ")
                    )
                }
            }
            Self::NoPlayableDifficulty { set_key, skipped } => write!(
                f,
                "set {set_key} has no playable .osu — {} skipped: {}",
                skipped.len(),
                skipped
                    .iter()
                    .map(|(name, reason)| format!("{name}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Self::NotAnOsuFile { path } => write!(
                f,
                "`--map {}` is not an .osu file — import .osz archives with --import first",
                path.display()
            ),
            Self::OsuFile { path, reason } => {
                write!(f, "cannot load .osu at {}: {reason}", path.display())
            }
            Self::DiffOnFileForm { wanted, only } => write!(
                f,
                "`--diff {wanted}` with a single .osu file: it has exactly one \
                 difficulty (`{only}`)"
            ),
            Self::NoHitObjects { version } => write!(
                f,
                "difficulty `{version}` has no hit objects — nothing to visualize"
            ),
            Self::Timeline { version, source } => {
                write!(f, "difficulty `{version}`: {source}")
            }
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Library(e) => Some(e),
            Self::Timeline { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<LibraryError> for LoadError {
    fn from(e: LibraryError) -> Self {
        Self::Library(e)
    }
}

/// What the difficulty selection did — drives the warning line (ruling
/// 2026-11-17) and keeps the choice unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// `--diff` matched exactly.
    Exact,
    /// No `--diff`: the first difficulty (manifest order).
    FirstNoFlag,
    /// `--diff` matched nothing exactly: fell back to the first.
    FirstFlagMiss { wanted: String },
}

/// Ruling 2026-11-17: exact `--diff` match, else the FIRST difficulty
/// (manifest order) with a warning listing ALL alternatives (the caller
/// logs it from the returned [`Selection`]). `diffs` is
/// (difficulty name, entry name) in manifest order; returns the chosen
/// index — always a valid one.
pub fn select_difficulty(diffs: &[(String, String)], want: Option<&str>) -> (usize, Selection) {
    if let Some(wanted) = want {
        if let Some(i) = diffs.iter().position(|(name, _)| name == wanted) {
            return (i, Selection::Exact);
        }
        return (
            0,
            Selection::FirstFlagMiss {
                wanted: wanted.to_owned(),
            },
        );
    }
    (0, Selection::FirstNoFlag)
}

/// Wrap a direct-file read/parse failure (the set form skips instead —
/// module docs).
fn osu_err(path: &Path, e: impl fmt::Display) -> LoadError {
    LoadError::OsuFile {
        path: path.to_path_buf(),
        reason: e.to_string(),
    }
}

/// The `--map PATH` form (§6 dev testing): ONE direct `.osu` file —
/// it must exist and carry the `.osu` extension, and a set-key
/// spelling is never reinterpreted (ruling 2026-11-17: no fallback
/// in either direction). `diff` is refused — one file is one
/// difficulty.
pub fn load_file(path: &Path, diff: Option<&str>) -> Result<LoadedMap, LoadError> {
    if path
        .extension()
        .is_none_or(|e| !e.eq_ignore_ascii_case("osu"))
    {
        return Err(LoadError::NotAnOsuFile {
            path: path.to_path_buf(),
        });
    }
    let text = std::fs::read_to_string(path).map_err(|e| osu_err(path, e))?;
    let map = parse_osu(&text).map_err(|e| osu_err(path, e))?;
    if let Some(wanted) = diff {
        return Err(LoadError::DiffOnFileForm {
            wanted: wanted.to_owned(),
            only: map.metadata.version.clone(),
        });
    }
    if map.hit_objects.is_empty() {
        return Err(LoadError::NoHitObjects {
            version: map.metadata.version.clone(),
        });
    }
    let timeline = Timeline::build(&map).map_err(|source| LoadError::Timeline {
        version: map.metadata.version.clone(),
        source,
    })?;
    let entry_name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    Ok(LoadedMap {
        source: MapSource::File {
            path: path.to_path_buf(),
        },
        entry_name,
        difficulties: vec![map.metadata.version.clone()],
        beatmap: map,
        timeline,
        audio: None,
    })
}

/// The `--set KEY` form — the D-45 consumption path: set key →
/// manifest → hashes → canonical paths, and NOTHING else (ruling
/// 2026-11-17: no filesystem probe of the key — a file named `KEY`
/// in the working directory cannot hijack the lookup; the probing
/// shared resolver was the reason Δ-H was withdrawn).
pub fn load_set(
    set_key: &str,
    diff: Option<&str>,
    library_root: Option<&Path>,
) -> Result<LoadedMap, LoadError> {
    let paths: LibraryPaths = match library_root {
        Some(root) => paths_from(root),
        None => default_paths().ok_or(LoadError::NoDataDir)?,
    };
    let (index, _dangling) = LibraryIndex::load(&paths)?;
    let available: Vec<String> = index.sets().keys().cloned().collect();
    if !index.sets().contains_key(set_key) {
        return Err(LoadError::UnknownSet {
            asked: set_key.to_owned(),
            available,
        });
    }
    // D-45: set → manifest → hashes → canonical paths. Entries whose
    // hash record went stale warn inside resolve_set and are skipped
    // there; a v1-era index never reaches this point (no manifests).
    let entries = index.resolve_set(set_key, &paths);

    let mut diffs: Vec<(String, Beatmap)> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    for (name, path) in &entries {
        // BTreeMap iteration = manifest (entry-name) order — the
        // deterministic "first" of the selection ruling.
        if !name.to_ascii_lowercase().ends_with(".osu") {
            continue;
        }
        let parsed = std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|text| parse_osu(&text).map_err(|e| e.to_string()));
        match parsed {
            Ok(map) => diffs.push((name.clone(), map)),
            Err(reason) => {
                tracing::warn!(
                    set = %set_key,
                    entry = %name,
                    reason = %reason,
                    "timeline load: skipped .osu entry"
                );
                skipped.push((name.clone(), reason));
            }
        }
    }
    if diffs.is_empty() {
        return Err(LoadError::NoPlayableDifficulty {
            set_key: set_key.to_owned(),
            skipped,
        });
    }

    let names: Vec<String> = diffs
        .iter()
        .map(|(_, m)| m.metadata.version.clone())
        .collect();
    let pairs: Vec<(String, String)> = diffs
        .iter()
        .map(|(entry, m)| (m.metadata.version.clone(), entry.clone()))
        .collect();
    let (chosen, selection) = select_difficulty(&pairs, diff);
    match &selection {
        Selection::Exact => {}
        Selection::FirstNoFlag => tracing::warn!(
            "timeline load: no --diff given — using the first difficulty `{}`; \
             available: {}",
            names[chosen],
            names.join(", ")
        ),
        Selection::FirstFlagMiss { wanted } => tracing::warn!(
            "timeline load: `--diff {wanted}` matches no difficulty exactly — using \
             the first `{}`; available: {}",
            names[chosen],
            names.join(", ")
        ),
    }

    // Own the chosen map without re-sorting `diffs`.
    let (entry_name, map) = diffs.swap_remove(chosen);
    if map.hit_objects.is_empty() {
        return Err(LoadError::NoHitObjects {
            version: map.metadata.version.clone(),
        });
    }
    let timeline = Timeline::build(&map).map_err(|source| LoadError::Timeline {
        version: map.metadata.version.clone(),
        source,
    })?;

    // Audio: the chosen map's AudioFilename against the manifest entry
    // names — EXACT, then `normalize_rel`-normalized (Δ-I, ruling
    // 2026-11-17). Never basename-only: two entries sharing a basename
    // in different directories are different files. No match → None +
    // warn, never fatal: placeholder audio (golden corpus) must still
    // visualize.
    let audio_filename = map.general.audio_filename.as_str();
    let normalized_want = normalize_rel(audio_filename);
    let audio = entries
        .iter()
        .find(|(name, _)| name.as_str() == audio_filename || normalize_rel(name) == normalized_want)
        .map(|(_, path)| path.clone());
    if audio.is_none() {
        tracing::warn!(
            audio = %audio_filename,
            "timeline load: audio file is not a manifest entry — no playback path \
             (placeholder fixtures do this); --audio PATH still works in commit b"
        );
    }

    Ok(LoadedMap {
        source: MapSource::Set {
            set_key: set_key.to_owned(),
        },
        entry_name,
        difficulties: names,
        beatmap: map,
        timeline,
        audio,
    })
}

/// CLI wrapper behind `--map PATH` (commit a): load ONE direct `.osu`
/// through [`load_file`] — no `--diff` (one file is one difficulty),
/// no library — then log one grep-able summary line on the D-38
/// contract (string fields via the `%` sigil, numbers bare) and exit.
/// The renderer (commit b) consumes the returned [`LoadedMap`]
/// instead of exiting.
pub fn run_map_cli(map_path: &str) -> Result<LoadedMap, LoadError> {
    let loaded = load_file(Path::new(map_path), None)?;
    log_loaded(&loaded);
    Ok(loaded)
}

/// CLI wrapper behind `--set KEY` (commit a): resolve the key through
/// [`load_set`] — the D-45 manifest ONLY, no filesystem probe (ruling
/// 2026-11-17) — then log the same D-38 summary line and exit. The
/// renderer (commit b) consumes the returned [`LoadedMap`] instead of
/// exiting.
pub fn run_set_cli(
    set_key: &str,
    diff: Option<&str>,
    library_root: Option<&Path>,
) -> Result<LoadedMap, LoadError> {
    let loaded = load_set(set_key, diff, library_root)?;
    log_loaded(&loaded);
    Ok(loaded)
}

/// The one D-38 summary line both wrappers log (`set` carries the set
/// key or the file path — whichever form loaded the map).
fn log_loaded(loaded: &LoadedMap) {
    let (circles, sliders, spinners) = loaded.timeline.counts();
    let set = match &loaded.source {
        MapSource::Set { set_key } => set_key.clone(),
        MapSource::File { path } => path.display().to_string(),
    };
    tracing::info!(
        set = %set,
        diff = %loaded.beatmap.metadata.version,
        entry = %loaded.entry_name,
        objects = loaded.timeline.events.len(),
        circles,
        sliders,
        spinners,
        kiai = loaded.timeline.kiai_ranges.len(),
        span_start_ms = loaded.timeline.span_start_ms,
        span_end_ms = loaded.timeline.span_end_ms,
        audio = %loaded
            .audio
            .as_ref()
            .map_or_else(|| "none".to_owned(), |p| p.display().to_string()),
        "timeline loaded"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::beatmap::import::{ImportLimits, import_osz};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Unique temp root per call (pid + sequence) — parallel tests never
    /// collide, nothing outside the root is touched (I-09; the
    /// import.rs pattern).
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hyprbeat-load-{}-{}-{}",
            tag,
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A full synthetic .osu (I-03: invented metadata). `version` varies
    /// the bytes so two diffs of one set never hash identically.
    fn osu_diff(version: &str, mode: u8, objects: &str) -> Vec<u8> {
        osu_diff_set("31337", version, mode, "audio.mp3", objects)
    }

    /// [`osu_diff`] with an explicit set id (multi-set tests).
    fn osu_diff_set(set_id: &str, version: &str, mode: u8, audio: &str, objects: &str) -> Vec<u8> {
        format!(
            "osu file format v14\n\
             \n\
             [General]\n\
             AudioFilename: {audio}\n\
             Mode: {mode}\n\
             \n\
             [Metadata]\n\
             Version: {version}\n\
             BeatmapSetID: {set_id}\n\
             \n\
             [Difficulty]\n\
             HPDrainRate: 5\n\
             CircleSize: 4\n\
             OverallDifficulty: 8\n\
             SliderMultiplier: 1.9\n\
             SliderTickRate: 1\n\
             \n\
             [TimingPoints]\n\
             0,375,4,2,0,60,1,0\n\
             \n\
             [HitObjects]\n\
             {objects}\n"
        )
        .into_bytes()
    }

    const OBJECTS_ONE_CIRCLE: &str = "256,192,0,1,0";

    /// Import a synthetic set (two standard diffs + audio) and return
    /// its set key. Entry names sort so "Easy" is the manifest-order
    /// first difficulty.
    fn import_set(paths: &LibraryPaths) -> String {
        let entries: Vec<(&str, Vec<u8>)> = vec![
            (
                "synth - set (fixture) [Easy].osu",
                osu_diff("Easy", 0, OBJECTS_ONE_CIRCLE),
            ),
            (
                "synth - set (fixture) [Hard].osu",
                osu_diff("Hard", 0, "256,192,0,1,0\n256,192,500,2,0,L|400:192,1,200"),
            ),
            ("audio.mp3", b"fake-mp3-bytes".to_vec()),
        ];
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            use zip::write::SimpleFileOptions;
            for (name, data) in &entries {
                w.start_file(*name, SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        let report = import_osz(
            std::io::Cursor::new(buf.into_inner()),
            paths,
            &ImportLimits::default(),
        )
        .unwrap();
        report.mode.set_key()
    }

    #[test]
    fn selection_exact_else_first() {
        let diffs = vec![
            ("Easy".to_owned(), "e.osu".to_owned()),
            ("Hard".to_owned(), "h.osu".to_owned()),
        ];
        let (i, s) = select_difficulty(&diffs, Some("Hard"));
        assert_eq!((i, s), (1, Selection::Exact));
        // No exact match (case matters — "exact" means exact):
        let (i, s) = select_difficulty(&diffs, Some("hard"));
        assert_eq!(
            (i, s),
            (
                0,
                Selection::FirstFlagMiss {
                    wanted: "hard".to_owned()
                }
            )
        );
        let (i, s) = select_difficulty(&diffs, None);
        assert_eq!((i, s), (0, Selection::FirstNoFlag));
    }

    #[test]
    fn set_form_loads_first_difficulty_and_resolves_audio() {
        let root = tmpdir("set-first");
        let paths = paths_from(&root);
        let set_key = import_set(&paths);

        let loaded = load_set(&set_key, None, Some(&root)).unwrap();
        assert_eq!(
            loaded.source,
            MapSource::Set {
                set_key: set_key.clone()
            }
        );
        // Manifest (entry-name) order: [Easy].osu < [Hard].osu.
        assert_eq!(loaded.difficulties, vec!["Easy", "Hard"]);
        assert_eq!(loaded.beatmap.metadata.version, "Easy");
        assert_eq!(loaded.entry_name, "synth - set (fixture) [Easy].osu");
        assert_eq!(loaded.timeline.counts(), (1, 0, 0));
        // The manifest resolves the audio entry to its canonical path.
        let audio = loaded.audio.expect("audio entry in the manifest");
        assert!(audio.ends_with("songs/31337/audio.mp3"));
        assert!(audio.is_file());
    }

    #[test]
    fn diff_exact_match_and_miss_fallback() {
        let root = tmpdir("diff");
        let paths = paths_from(&root);
        let set_key = import_set(&paths);

        let loaded = load_set(&set_key, Some("Hard"), Some(&root)).unwrap();
        assert_eq!(loaded.beatmap.metadata.version, "Hard");
        assert_eq!(loaded.timeline.counts(), (1, 1, 0));

        // No exact match → first difficulty (ruling 3), warn logged.
        let loaded = load_set(&set_key, Some("hard"), Some(&root)).unwrap();
        assert_eq!(loaded.beatmap.metadata.version, "Easy");
    }

    #[test]
    fn unknown_set_lists_available_and_v1_hint() {
        let root = tmpdir("unknown");
        let paths = paths_from(&root);
        let set_key = import_set(&paths);

        let err = load_set("999", None, Some(&root)).unwrap_err();
        match &err {
            LoadError::UnknownSet { asked, available } => {
                assert_eq!(asked, "999");
                assert_eq!(available, &vec![set_key.clone()]);
            }
            other => panic!("expected UnknownSet, got {other:?}"),
        }
        assert!(err.to_string().contains("available sets: 31337"));

        // A v1-era index (no manifests at all) gets the re-import hint.
        let index_path = paths.index_file;
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&index_path).unwrap()).unwrap();
        let mut v1 = raw.clone();
        v1["version"] = serde_json::json!(1);
        // Strip manifests: what a pre-D-45 binary wrote.
        v1.as_object_mut().unwrap().remove("sets");
        std::fs::write(&index_path, serde_json::to_string_pretty(&v1).unwrap()).unwrap();

        let err = load_set(&set_key, None, Some(&root)).unwrap_err();
        assert!(matches!(err, LoadError::UnknownSet { ref available, .. } if available.is_empty()));
        assert!(err.to_string().contains("re-import your .osz files"));
    }

    #[test]
    fn nonstandard_and_broken_diffs_are_skipped() {
        let root = tmpdir("skip");
        let paths = paths_from(&root);
        // One playable diff, one mania diff (parse_osu rejects it by
        // design, D-12), one garbage file with an .osu extension.
        let entries: Vec<(&str, Vec<u8>)> = vec![
            ("s (f) [Easy].osu", osu_diff("Easy", 0, OBJECTS_ONE_CIRCLE)),
            (
                "s (f) [Mania].osu",
                osu_diff("Mania Keys", 3, "256,192,0,1,0"),
            ),
            (
                "s (f) [broken].osu",
                b"[Metadata]\nBeatmapSetID: 31337\n".to_vec(),
            ),
            ("audio.mp3", b"x".to_vec()),
        ];
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            use zip::write::SimpleFileOptions;
            for (name, data) in &entries {
                w.start_file(*name, SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        let report = import_osz(
            std::io::Cursor::new(buf.into_inner()),
            &paths,
            &ImportLimits::default(),
        )
        .unwrap();
        let set_key = report.mode.set_key();

        let loaded = load_set(&set_key, None, Some(&root)).unwrap();
        assert_eq!(loaded.difficulties, vec!["Easy"]);
        assert_eq!(loaded.beatmap.metadata.version, "Easy");

        // A set with ONLY skippable diffs fails with the reasons (its
        // own set id — the library keeps the first set intact).
        let entries: Vec<(&str, Vec<u8>)> = vec![(
            "only mania.osu",
            osu_diff_set("31338", "Mania Keys", 3, "audio.mp3", "256,192,0,1,0"),
        )];
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            use zip::write::SimpleFileOptions;
            for (name, data) in &entries {
                w.start_file(*name, SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        let report = import_osz(
            std::io::Cursor::new(buf.into_inner()),
            &paths,
            &ImportLimits::default(),
        )
        .unwrap();
        let err = load_set(&report.mode.set_key(), None, Some(&root)).unwrap_err();
        match &err {
            LoadError::NoPlayableDifficulty { skipped, .. } => {
                assert_eq!(skipped.len(), 1);
                assert!(skipped[0].1.contains("Mode 3"));
            }
            other => panic!("expected NoPlayableDifficulty, got {other:?}"),
        }
    }

    #[test]
    fn file_form_direct_load_and_errors() {
        let root = tmpdir("file-form");
        let osu_path = root.join("direct.osu");
        std::fs::write(
            &osu_path,
            osu_diff("Direct", 0, "256,192,0,1,0\n400,300,3000,12,0,4200"),
        )
        .unwrap();

        let loaded = load_file(&osu_path, None).unwrap();
        assert_eq!(
            loaded.source,
            MapSource::File {
                path: osu_path.clone()
            }
        );
        assert_eq!(loaded.difficulties, vec!["Direct"]);
        assert_eq!(loaded.timeline.counts(), (1, 0, 1));
        assert_eq!(loaded.audio, None);

        // --diff is meaningless for one file — error names its difficulty.
        let err = load_file(&osu_path, Some("Other")).unwrap_err();
        match &err {
            LoadError::DiffOnFileForm { wanted, only } => {
                assert_eq!(wanted, "Other");
                assert_eq!(only, "Direct");
            }
            other => panic!("expected DiffOnFileForm, got {other:?}"),
        }

        // An .osz archive is not the file form's business.
        let osz_path = root.join("bundle.osz");
        std::fs::write(&osz_path, b"pk").unwrap();
        let err = load_file(&osz_path, None).unwrap_err();
        assert!(matches!(err, LoadError::NotAnOsuFile { .. }));

        // Unparseable direct file fails with the parse reason.
        let bad_path = root.join("bad.osu");
        std::fs::write(&bad_path, b"garbage").unwrap();
        let err = load_file(&bad_path, None).unwrap_err();
        assert!(matches!(err, LoadError::OsuFile { .. }));
        assert!(err.to_string().contains("osu file format"));

        // Empty map: nothing to visualize.
        let empty_path = root.join("empty.osu");
        std::fs::write(&empty_path, osu_diff("Empty", 0, "")).unwrap();
        let err = load_file(&empty_path, None).unwrap_err();
        assert!(matches!(err, LoadError::NoHitObjects { .. }));

        // Timeline errors surface with the difficulty name.
        let tl_path = root.join("broken-timing.osu");
        std::fs::write(
            &tl_path,
            "osu file format v14\n\
             \n\
             [General]\n\
             AudioFilename: a.mp3\n\
             \n\
             [Metadata]\n\
             Version: BrokenTiming\n\
             \n\
             [Difficulty]\n\
             HPDrainRate: 5\n\
             CircleSize: 4\n\
             OverallDifficulty: 8\n\
             SliderMultiplier: 1.9\n\
             SliderTickRate: 1\n\
             \n\
             [TimingPoints]\n\
             1000,-50,4,2,0,60,0,0\n\
             \n\
             [HitObjects]\n\
             256,192,2000,2,0,L|400:192,1,100\n",
        )
        .unwrap();
        let err = load_file(&tl_path, None).unwrap_err();
        match &err {
            LoadError::Timeline { version, source } => {
                assert_eq!(version, "BrokenTiming");
                assert_eq!(source, &TimelineError::NoUninheritedTimingPoint);
            }
            other => panic!("expected Timeline, got {other:?}"),
        }
    }

    #[test]
    fn missing_audio_entry_warns_but_loads() {
        // Set WITHOUT an audio entry: the map names audio.mp3 but the
        // manifest lacks it → audio None, load still succeeds.
        let root = tmpdir("no-audio");
        let paths = paths_from(&root);
        let entries: Vec<(&str, Vec<u8>)> =
            vec![("s (f) [Easy].osu", osu_diff("Easy", 0, OBJECTS_ONE_CIRCLE))];
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            use zip::write::SimpleFileOptions;
            for (name, data) in &entries {
                w.start_file(*name, SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        let report = import_osz(
            std::io::Cursor::new(buf.into_inner()),
            &paths,
            &ImportLimits::default(),
        )
        .unwrap();
        let loaded = load_set(&report.mode.set_key(), None, Some(&root)).unwrap();
        assert_eq!(loaded.audio, None);
        assert_eq!(loaded.beatmap.metadata.version, "Easy");
    }

    /// Δ-I (ruling 2026-11-17): exact, then `normalize_rel` — never
    /// basename. `AudioFilename: ./audio.mp3` normalizes onto the flat
    /// manifest entry `audio.mp3`.
    #[test]
    fn audio_match_exact_then_normalized() {
        // The map spells the audio `./audio.mp3`; the manifest stores the
        // flat `audio.mp3`. Exact fails, the normalized compare matches.
        let root = tmpdir("audio-norm");
        let paths = paths_from(&root);
        let entries: Vec<(&str, Vec<u8>)> = vec![
            (
                "synth - set (fixture) [Easy].osu",
                osu_diff_set("31337", "Easy", 0, "./audio.mp3", OBJECTS_ONE_CIRCLE),
            ),
            ("audio.mp3", b"fake-mp3-bytes".to_vec()),
        ];
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            use zip::write::SimpleFileOptions;
            for (name, data) in &entries {
                w.start_file(*name, SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        let report = import_osz(
            std::io::Cursor::new(buf.into_inner()),
            &paths,
            &ImportLimits::default(),
        )
        .unwrap();
        let loaded = load_set(&report.mode.set_key(), None, Some(&root)).unwrap();
        let audio = loaded
            .audio
            .expect("`./audio.mp3` normalizes onto `audio.mp3`");
        assert!(audio.ends_with("songs/31337/audio.mp3"));
    }

    /// Two entries sharing a basename in different directories are
    /// different files: no exact match, no normalized match → None +
    /// warn. The pre-Δ-I basename fallback would have silently
    /// picked `a/audio.mp3` (BTreeMap order) — the wrong file.
    #[test]
    fn audio_same_basename_different_dirs_no_match() {
        // The map names the flat `audio.mp3` (the helper default); the
        // set only carries `a/audio.mp3` and `b/audio.mp3`.
        let root = tmpdir("audio-basename");
        let paths = paths_from(&root);
        let entries: Vec<(&str, Vec<u8>)> = vec![
            (
                "synth - set (fixture) [Easy].osu",
                osu_diff_set("31337", "Easy", 0, "audio.mp3", OBJECTS_ONE_CIRCLE),
            ),
            ("a/audio.mp3", b"fake-mp3-bytes-a".to_vec()),
            ("b/audio.mp3", b"fake-mp3-bytes-b".to_vec()),
        ];
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            use zip::write::SimpleFileOptions;
            for (name, data) in &entries {
                w.start_file(*name, SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        let report = import_osz(
            std::io::Cursor::new(buf.into_inner()),
            &paths,
            &ImportLimits::default(),
        )
        .unwrap();
        let loaded = load_set(&report.mode.set_key(), None, Some(&root)).unwrap();
        assert!(
            loaded.audio.is_none(),
            "same-basename entries in different dirs must not match"
        );
        assert_eq!(loaded.beatmap.metadata.version, "Easy");
    }

    /// Δ-L (ruling 2026-11-17): the strict per-flag split — no
    /// fallback in either direction. `--set KEY` resolves through the
    /// manifest ONLY (the pre-split resolver probed `Path::is_file`
    /// FIRST, so a file named `KEY` in the working directory hijacked
    /// the lookup — the reason Δ-H was withdrawn); `--map PATH` loads
    /// ONE direct `.osu` and never consults the index.
    #[test]
    fn strict_split_no_cross_fallback() {
        let root = tmpdir("split");
        let paths = paths_from(&root);
        let set_key = import_set(&paths);

        // --set KEY: the manifest answer comes back even with a file
        // of that exact name present (the decoy lives in the temp
        // root — tests never touch the working directory, I-09; the
        // probe itself is gone from load_set entirely).
        let decoy = root.join(&set_key);
        std::fs::write(&decoy, b"not a beatmap").unwrap();
        let loaded = load_set(&set_key, None, Some(&root)).unwrap();
        assert_eq!(
            loaded.source,
            MapSource::Set {
                set_key: set_key.clone()
            }
        );
        assert_eq!(loaded.beatmap.metadata.version, "Easy");

        // --map with a set-key spelling: no .osu extension — refused,
        // the key is never reinterpreted through the manifest.
        let err = load_file(&decoy, None).unwrap_err();
        assert!(matches!(err, LoadError::NotAnOsuFile { .. }));

        // --map with a real .osu loads; --set with its path spelling
        // is UnknownSet — the path is never reinterpreted as a file.
        let osu = root.join("direct.osu");
        std::fs::write(&osu, osu_diff("Direct", 0, OBJECTS_ONE_CIRCLE)).unwrap();
        let loaded = load_file(&osu, None).unwrap();
        assert_eq!(loaded.beatmap.metadata.version, "Direct");
        let err = load_set(osu.to_str().unwrap(), None, Some(&root)).unwrap_err();
        assert!(matches!(err, LoadError::UnknownSet { .. }));

        // The wrappers route strictly, one entry point per flag.
        let loaded = run_map_cli(osu.to_str().unwrap()).unwrap();
        assert_eq!(loaded.source, MapSource::File { path: osu.clone() });
        let loaded = run_set_cli(&set_key, None, Some(&root)).unwrap();
        assert_eq!(
            loaded.source,
            MapSource::Set {
                set_key: set_key.clone()
            }
        );
    }
}
