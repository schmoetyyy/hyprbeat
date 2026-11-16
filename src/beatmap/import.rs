//! `.osz` zip import (§5.2, D-43/D-44) — the library-side driver + the
//! `--import` CLI wrapper.
//!
//! [`super::osz`] owns the index and the content store; this module turns
//! one `.osz` archive into a batch of [`LibraryIndex::store_file`] calls
//! bracketed by one [`LibraryIndex::load`] and one [`LibraryIndex::save`].
//! The core ([`import_osz`]) stays pure library code — no CLI parsing, no
//! XDG access of its own; the caller supplies the [`LibraryPaths`] (tests
//! use temp roots, I-09). [`run_cli`] is the thin `--import` wrapper
//! around it: root resolution (`--library-root` or the XDG default, I-07),
//! file open, and the D-38 log contract.
//!
//! Routing (D-43):
//! - **Filter**: only `*.osu`, audio (`.mp3 .ogg .flac .wav`) and images
//!   (`.jpg .jpeg .png`) are extracted — the §5.2 `songs/<set-id>/` layout
//!   needs nothing else; video, `.osb` storyboards, skins and unknown
//!   extensions are skipped (reported, never fatal).
//! - **Submitted**: the archive's set id comes from the kept `.osu`
//!   entries' `[Metadata] BeatmapSetID` via a lenient metadata-only scan
//!   (a full `parse_osu` would be WRONG here — mania diffs fail it by
//!   D-12 but must still import as bytes). All kept `.osu` files must
//!   agree on one positive id; targets are `songs/<set-id>/<entry name>`.
//! - **Unsubmitted** (every kept `.osu` id absent, 0 or -1): targets are
//!   `songs/unsubmitted/<hash8>/<entry name>` where `hash8` is the first
//!   8 hex chars of the SHA-256 over, for every kept entry sorted by raw
//!   entry name, the line `<raw name>\n<lowercase-hex sha256 of its
//!   bytes>\n`. Deterministic: the same archive always resolves to the
//!   same directory, so re-imports dedup (D-41) into it.
//!
//! Failure posture (D-44): everything that marks a broken or hostile
//! archive — no kept `.osu`, conflicting set ids, two entries resolving to
//! the same target, per-entry/total/entry-count caps — hard-fails BEFORE
//! the first byte is stored, and a failed import never touches
//! `index.json` (fail-before-write, test-pinned). Same-NAME duplicates
//! never reach resolution: zip's reader indexes entries by name and keeps
//! one (last wins, pinned by test). Untrusted entry names are pre-filtered
//! with [`sanitize_rel_path`] and become skips, not errors (the doc there
//! promised exactly this split). Every read is `take()`-guarded so a lying
//! local header cannot exceed the per-entry cap; an actual-oversize abort
//! mid-store leaves already-written bytes unindexed — the index itself is
//! saved only at the very end (the osz.rs ordering guarantee). The index
//! is saved once per import and only when mutated: a pure re-import of
//! known bytes leaves `index.json` untouched.
//!
//! Zero Wayland/audio I/O; tests build their archives in memory (I-09).

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::io::Seek;
use std::path::{Path, PathBuf};

use zip::ZipArchive;

use super::osz::{
    Imported, LibraryError, LibraryIndex, LibraryPaths, default_paths, paths_from,
    sanitize_rel_path, sha256_hex,
};

/// Extraction allowlist (D-43): the §5.2 layout holds `.osu` files, audio
/// and images; nothing else an `.osz` may carry (video, storyboards, skin
/// files) is needed for v0.1 gameplay. Audio extensions mirror the
/// symphonia features (§4).
const KEEP_EXTENSIONS: &[&str] = &[
    ".osu", ".mp3", ".ogg", ".flac", ".wav", ".jpg", ".jpeg", ".png",
];

/// True when `raw_name` carries a keep-listed extension (case-insensitive;
/// the trailing-dot check also keeps a bare `.osu`, leniently).
fn keep_extension(raw_name: &str) -> bool {
    let lower = raw_name.to_ascii_lowercase();
    KEEP_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

/// Collapse `.` components, duplicate separators and trailing separators in
/// an already-sanitized rel path ("x/./a.osu" → "x/a.osu"). Without this,
/// two DIFFERENT raw names could resolve to distinct target strings yet
/// land on the same physical file — past the string-level duplicate check
/// and past [`LibraryIndex`]'s hash-keyed displacement (D-44 hole).
/// Pure string work: `sanitize_rel_path` already rejected `..`, absolute
/// paths, backslashes and NULs, so only normal parts and `.` remain.
fn normalize_rel(rel: &str) -> String {
    rel.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Decompression + fan-out caps for one `.osz` import (D-44). The defaults
/// sit far above anything a real osu! archive needs — a set with its audio
/// rarely exceeds ~100 MB compressed / ~200 MB extracted — so hitting one
/// means the archive is hostile or broken, not merely big.
/// `[REVISITABLE]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportLimits {
    /// Maximum uncompressed size of ONE kept entry.
    pub max_entry_bytes: u64,
    /// Maximum sum of uncompressed sizes of all kept entries.
    pub max_total_bytes: u64,
    /// Maximum number of archive entries (any kind) considered.
    pub max_entries: usize,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_entry_bytes: 512 * 1024 * 1024,
            max_total_bytes: 2 * 1024 * 1024 * 1024,
            max_entries: 10_000,
        }
    }
}

/// Import errors. Everything except [`ImportError::Library`],
/// [`ImportError::Zip`] and [`ImportError::Io`] is decided before the
/// first byte is stored (fail-before-write, D-44).
#[derive(Debug)]
pub enum ImportError {
    /// Index/store failure from [`super::osz`] — a corrupt index fails
    /// safe there and is surfaced untouched.
    Library(LibraryError),
    /// The archive could not be opened, or an entry's metadata is
    /// unreadable.
    Zip(zip::result::ZipError),
    /// Reading a kept entry's bytes failed mid-stream.
    Io(std::io::Error),
    /// No `.osu` file survived the filter — not an importable `.osz`.
    NotAnOsz,
    /// The kept `.osu` files disagree on `[Metadata] BeatmapSetID`:
    /// two different submitted ids, or submitted mixed with unsubmitted.
    /// One `.osz` = one set (D-43); ambiguity is refused, not guessed.
    SetIdConflict { found: Vec<i64> },
    /// Two entries resolve to the same target path (user-ruled hard fail).
    /// Reported with the raw entry names involved.
    DuplicateTarget {
        target: String,
        first: String,
        second: String,
    },
    /// A kept entry declares (or actually carries, via the read guard)
    /// more than [`ImportLimits::max_entry_bytes`] uncompressed bytes.
    OversizeEntry { name: String, size: u64, cap: u64 },
    /// The kept entries sum past [`ImportLimits::max_total_bytes`].
    OversizeTotal { total: u64, cap: u64 },
    /// The archive holds more entries than [`ImportLimits::max_entries`].
    TooManyEntries { count: usize, cap: usize },
    /// The `.osz` file itself could not be opened (missing, unreadable).
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    /// No `--library-root` and the environment resolves no XDG data dir
    /// (no HOME / XDG_DATA_HOME) — nowhere to put the library.
    NoDataDir,
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Library(e) => write!(f, "library: {e}"),
            Self::Zip(e) => write!(f, "bad zip archive: {e}"),
            Self::Io(e) => write!(f, "zip entry read failed: {e}"),
            Self::NotAnOsz => {
                write!(f, "no .osu file in archive — not an importable .osz")
            }
            Self::SetIdConflict { found } => write!(
                f,
                "kept .osu files disagree on BeatmapSetID ({found:?}) — one .osz = one set"
            ),
            Self::DuplicateTarget {
                target,
                first,
                second,
            } => write!(
                f,
                "entries {first:?} and {second:?} both resolve to target {target:?}"
            ),
            Self::OversizeEntry { name, size, cap } => {
                write!(f, "entry {name:?} carries {size} bytes (cap {cap})")
            }
            Self::OversizeTotal { total, cap } => {
                write!(f, "kept entries total {total} bytes (cap {cap})")
            }
            Self::TooManyEntries { count, cap } => {
                write!(f, "archive holds {count} entries (cap {cap})")
            }
            Self::Open { path, source } => {
                write!(f, "cannot open .osz at {}: {source}", path.display())
            }
            Self::NoDataDir => write!(
                f,
                "no XDG data dir resolves for the library (HOME/XDG_DATA_HOME unset?) — pass --library-root DIR"
            ),
        }
    }
}

impl std::error::Error for ImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Library(e) => Some(e),
            Self::Zip(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Open { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<LibraryError> for ImportError {
    fn from(e: LibraryError) -> Self {
        Self::Library(e)
    }
}

/// Which routing the archive took (D-43).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportMode {
    /// All kept `.osu` files agree on this positive set id.
    Submitted { set_id: i64 },
    /// No submitted id anywhere; the archive landed under
    /// `songs/unsubmitted/<hash8>/`.
    Unsubmitted { hash8: String },
}

impl ImportMode {
    /// The library set-key (D-45): `<set-id>` or `unsubmitted/<hash8>` —
    /// the same string that routes the target paths, keys the set
    /// manifest and names the set in the log line. One source of truth.
    pub fn set_key(&self) -> String {
        match self {
            Self::Submitted { set_id } => set_id.to_string(),
            Self::Unsubmitted { hash8 } => format!("unsubmitted/{hash8}"),
        }
    }
}

/// Why an archive entry was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Entry name failed [`sanitize_rel_path`] (traversal, backslash, …).
    UnsafeName,
    /// Extension is not on the D-43 allowlist (video, `.osb`, skins, …).
    OffAllowlist,
    /// A bare directory entry — nothing to extract.
    Directory,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeName => write!(f, "unsafe entry name"),
            Self::OffAllowlist => write!(f, "extension not on the keep list"),
            Self::Directory => write!(f, "directory entry"),
        }
    }
}

/// One skipped archive entry (kept in the report, never silent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Raw entry name as stored in the archive.
    pub name: String,
    pub reason: SkipReason,
}

/// Outcome of one successful [`import_osz`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// Routing decision (set id or unsubmitted bundle hash8).
    pub mode: ImportMode,
    /// Per-kept-entry outcomes, in archive order.
    pub files: Vec<Imported>,
    /// Skipped entries, in archive order.
    pub skipped: Vec<Skipped>,
    /// Fresh stores among `files` (the D-42 replacements are a subset).
    pub stored: usize,
    /// D-41 references among `files`.
    pub deduped: usize,
    /// D-41 repairs among `files`.
    pub repaired: usize,
    /// D-42 displacements among `files` (each displaced one stale entry).
    pub replaced: usize,
    /// Whether the index was mutated and re-saved. A pure re-import of
    /// known bytes dedups everywhere and leaves `index.json` untouched.
    pub index_saved: bool,
}

/// Import one `.osz` archive from `reader` into the library at `paths`
/// (§5.2): load → filter → resolve → store → save. See the module docs
/// for the routing rules and the failure posture.
pub fn import_osz<R: Read + Seek>(
    reader: R,
    paths: &LibraryPaths,
    limits: &ImportLimits,
) -> Result<ImportReport, ImportError> {
    let mut archive = ZipArchive::new(reader).map_err(ImportError::Zip)?;
    if archive.len() > limits.max_entries {
        return Err(ImportError::TooManyEntries {
            count: archive.len(),
            cap: limits.max_entries,
        });
    }

    // ---- pass 1: metadata only — filter, no decompression --------------
    struct Kept {
        index: usize,
        /// Raw entry name (for reports).
        name: String,
        /// Sanitized songs-relative name (validated once, reused).
        rel: String,
        declared: u64,
        is_osu: bool,
    }
    let mut kept: Vec<Kept> = Vec::new();
    let mut skipped: Vec<Skipped> = Vec::new();
    for i in 0..archive.len() {
        let file = archive.by_index(i).map_err(ImportError::Zip)?;
        let name = file.name().map_err(ImportError::Zip)?.into_owned();
        if file.is_dir() {
            skipped.push(Skipped {
                name,
                reason: SkipReason::Directory,
            });
            continue;
        }
        if !keep_extension(&name) {
            skipped.push(Skipped {
                name,
                reason: SkipReason::OffAllowlist,
            });
            continue;
        }
        // Untrusted names are skips, not errors (D-44; sanitize_rel_path
        // doc: "the importer pre-filters skips with it"). The sanitized
        // name is then normalized so distinct raw spellings collapse onto
        // one target spelling before the duplicate check.
        let rel = match sanitize_rel_path(&name) {
            Ok(rel) => normalize_rel(&rel),
            Err(_) => {
                skipped.push(Skipped {
                    name,
                    reason: SkipReason::UnsafeName,
                });
                continue;
            }
        };
        let is_osu = name.to_ascii_lowercase().ends_with(".osu");
        kept.push(Kept {
            index: i,
            name,
            rel,
            declared: file.size(),
            is_osu,
        });
    }

    // ---- declared-size caps: checked before ANY read -------------------
    let mut declared_total = 0u64;
    for k in &kept {
        if k.declared > limits.max_entry_bytes {
            return Err(ImportError::OversizeEntry {
                name: k.name.clone(),
                size: k.declared,
                cap: limits.max_entry_bytes,
            });
        }
        declared_total += k.declared;
    }
    if declared_total > limits.max_total_bytes {
        return Err(ImportError::OversizeTotal {
            total: declared_total,
            cap: limits.max_total_bytes,
        });
    }

    // ---- pass 2: read the (small) .osu entries, scan set ids -----------
    // Retained for the store pass — .osu files are tiny next to audio.
    let mut osu_bytes: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    let mut found_ids: Vec<i64> = Vec::new();
    let mut any_osu = false;
    for k in &kept {
        if !k.is_osu {
            continue;
        }
        any_osu = true;
        let bytes = read_entry(&mut archive, k.index, limits.max_entry_bytes)?;
        // Lenient scan: absent/0/-1/garbage all mean "unsubmitted" (-1 is
        // the osu! convention), so the conflict report can list them.
        found_ids.push(scan_set_id(&bytes).unwrap_or(-1));
        osu_bytes.insert(k.index, bytes);
    }
    if !any_osu {
        return Err(ImportError::NotAnOsz);
    }

    let mut distinct_ids = found_ids;
    distinct_ids.sort_unstable();
    distinct_ids.dedup();
    let mode = match distinct_ids.as_slice() {
        [id] if *id > 0 => ImportMode::Submitted { set_id: *id },
        [-1] => ImportMode::Unsubmitted {
            // Pass 3: hash every kept entry (bounded reads, bytes dropped)
            // and fold the sorted name+hash lines into the bundle hash8.
            hash8: {
                let mut lines: Vec<(String, String)> = Vec::with_capacity(kept.len());
                for k in &kept {
                    let bytes = read_entry(&mut archive, k.index, limits.max_entry_bytes)?;
                    lines.push((k.name.clone(), sha256_hex(&bytes)));
                }
                lines.sort();
                let mut folded = String::new();
                for (name, hash) in &lines {
                    folded.push_str(name);
                    folded.push('\n');
                    folded.push_str(hash);
                    folded.push('\n');
                }
                let digest = sha256_hex(folded.as_bytes());
                digest[..8].to_string()
            },
        },
        _ => {
            return Err(ImportError::SetIdConflict {
                found: distinct_ids,
            });
        }
    };

    // ---- resolve targets; duplicate target = hard fail before any write
    let mut resolved: Vec<(String, String)> = Vec::with_capacity(kept.len());
    for k in &kept {
        let target = match &mode {
            ImportMode::Submitted { set_id } => format!("{set_id}/{}", k.rel),
            ImportMode::Unsubmitted { hash8 } => format!("unsubmitted/{hash8}/{}", k.rel),
        };
        resolved.push((k.name.clone(), target));
    }
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, target) in &resolved {
        if let Some(first) = seen.insert(target.as_str(), name.as_str()) {
            return Err(ImportError::DuplicateTarget {
                target: target.clone(),
                first: first.to_string(),
                second: name.clone(),
            });
        }
    }
    let targets: Vec<String> = resolved.into_iter().map(|(_, target)| target).collect();

    // ---- store pass: one load, N store_file calls ----------------------
    let (mut index, _dangling) = LibraryIndex::load(paths)?;
    let mut files = Vec::with_capacity(kept.len());
    let mut stored = 0usize;
    let mut deduped = 0usize;
    let mut repaired = 0usize;
    let mut replaced = 0usize;
    let mut actual_total = 0u64;
    for (k, target) in kept.iter().zip(targets.iter()) {
        // .osu bytes were already read in pass 2; everything else is
        // decompressed exactly once, here.
        let bytes = match osu_bytes.remove(&k.index) {
            Some(bytes) => bytes,
            None => read_entry(&mut archive, k.index, limits.max_entry_bytes)?,
        };
        actual_total += bytes.len() as u64;
        if actual_total > limits.max_total_bytes {
            // A lying local header got past the declared sum — abort (the
            // bytes written so far stay unindexed; the index is untouched).
            return Err(ImportError::OversizeTotal {
                total: actual_total,
                cap: limits.max_total_bytes,
            });
        }
        let out = index.store_file(&bytes, target, paths)?;
        stored += (!out.deduped && !out.repaired) as usize;
        deduped += out.deduped as usize;
        repaired += out.repaired as usize;
        replaced += out.replaced.is_some() as usize;
        files.push(out);
    }

    // ---- D-45: record the set manifest ----------------------------------
    // EVERY kept entry, deduped or not: after a cross-set dedup this is
    // the only record that set B "owns" a file whose canonical copy
    // lives in set A's directory. Entry names are unique (the
    // duplicate-target check hard-failed otherwise) and the manifest
    // replaces the set's previous one wholesale (the D-42 posture — the
    // .osz is the source of truth for its own set).
    let set_key = mode.set_key();
    let mut manifest_entries: BTreeMap<String, String> = BTreeMap::new();
    for (k, out) in kept.iter().zip(files.iter()) {
        manifest_entries.insert(k.rel.clone(), out.hash.clone());
    }
    let manifest_changed = index.record_set_manifest(&set_key, manifest_entries);

    // ---- save once, only when mutated ----------------------------------
    // A manifest-only change saves too (a v1-era index heals on the
    // first re-import); a pure re-import of known bytes with an
    // unchanged manifest still saves nothing (test-pinned).
    let index_saved = stored > 0 || repaired > 0 || manifest_changed;
    if index_saved {
        index.save(paths)?;
    }

    Ok(ImportReport {
        mode,
        files,
        skipped,
        stored,
        deduped,
        repaired,
        replaced,
        index_saved,
    })
}

/// CLI wrapper behind `--import PATH` (commit 5, §5.2): resolve the
/// library root (`--library-root DIR` overrides the XDG default, I-07),
/// open the `.osz` file, drive [`import_osz`] with the default
/// [`ImportLimits`] (the D-44 caps), and log the report on the D-38 field
/// contract — string fields via the `%` Display sigil so they print bare,
/// numbers bare by default — so `dev/validate-phase3-import.sh` can grep
/// one summary line:
/// `osz import complete set=… stored=… deduped=… repaired=… replaced=…
/// skipped=… index_saved=…`.
///
/// Skips are logged and reported, never fatal (D-43/D-44); the exit code
/// is the caller's: `Ok` (even with skips) vs [`ImportError`].
pub fn run_cli(osz_path: &Path, library_root: Option<&Path>) -> Result<ImportReport, ImportError> {
    let paths = match library_root {
        Some(root) => paths_from(root),
        None => default_paths().ok_or(ImportError::NoDataDir)?,
    };
    let file = std::fs::File::open(osz_path).map_err(|source| ImportError::Open {
        path: osz_path.to_path_buf(),
        source,
    })?;
    let report = import_osz(file, &paths, &ImportLimits::default())?;
    log_report(&report);
    Ok(report)
}

/// Emit the import report on the D-38 log contract: one info line per
/// skipped entry (the D-43/D-44 "skips are reported, never silent"), one
/// warn line per repair or D-42 displacement (state the library healed),
/// and one grep-able summary line for the validation script.
fn log_report(report: &ImportReport) {
    let set = report.mode.set_key();
    for s in &report.skipped {
        tracing::info!(name = %s.name, reason = %s.reason, "osz import: skipped entry");
    }
    for f in &report.files {
        if f.repaired {
            tracing::warn!(path = %f.path, "osz import: dangling entry repaired (D-41)");
        }
        if let Some(displaced) = &f.replaced {
            tracing::warn!(
                path = %f.path,
                displaced = %displaced,
                "osz import: stale entry replaced (D-42)"
            );
        }
    }
    tracing::info!(
        set = %set,
        stored = report.stored,
        deduped = report.deduped,
        repaired = report.repaired,
        replaced = report.replaced,
        skipped = report.skipped.len(),
        index_saved = report.index_saved,
        "osz import complete"
    );
}

/// Read one entry's bytes with the per-entry cap enforced on BOTH the
/// declared size and the actual read (`take()` guard — a local header may
/// lie about its size, the decompressed stream cannot, D-44).
fn read_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    index: usize,
    cap: u64,
) -> Result<Vec<u8>, ImportError> {
    let file = archive.by_index(index).map_err(ImportError::Zip)?;
    let name = file.name().map_err(ImportError::Zip)?.into_owned();
    let declared = file.size();
    if declared > cap {
        return Err(ImportError::OversizeEntry {
            name,
            size: declared,
            cap,
        });
    }
    let mut guarded = file.take(cap.saturating_add(1));
    let mut buf = Vec::new();
    guarded.read_to_end(&mut buf).map_err(ImportError::Io)?;
    if buf.len() as u64 > cap {
        return Err(ImportError::OversizeEntry {
            name,
            size: buf.len() as u64,
            cap,
        });
    }
    Ok(buf)
}

/// Lenient `[Metadata] BeatmapSetID` scan (D-43). A full `parse_osu` would
/// be wrong here: the importer must store files verbatim regardless of
/// game mode, and mania diffs fail it by design (D-12). Walks sections
/// lossily (BOM/CRLF tolerated), returns the id only when the value
/// parses to a positive integer.
fn scan_set_id(bytes: &[u8]) -> Option<i64> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut in_metadata = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_metadata = line.eq_ignore_ascii_case("[metadata]");
            continue;
        }
        if !in_metadata {
            continue;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.trim().eq_ignore_ascii_case("beatmapsetid")
        {
            return value.trim().parse::<i64>().ok().filter(|id| *id > 0);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Unique temp root per call (pid + sequence) — parallel tests never
    /// collide, nothing outside the root is touched (I-09).
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hyprbeat-import-{}-{}-{}",
            tag,
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// In-memory zip fixture: entries stored in the given order.
    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = ZipWriter::new(&mut buf);
            for (name, data) in entries {
                w.start_file(*name, SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    /// Minimal synthetic .osu body — invented metadata (I-03), only the
    /// set-id line matters to the importer. `title` varies the bytes so two
    /// diffs of one set never hash identically (identical bytes dedup by
    /// D-41 — the fixture must not collide accidentally).
    fn osu(set_id: &str, title: &str) -> Vec<u8> {
        format!("[Metadata]\nBeatmapSetID: {set_id}\nTitle: {title}\n").into_bytes()
    }

    fn import(bytes: Vec<u8>, paths: &LibraryPaths) -> Result<ImportReport, ImportError> {
        import_osz(std::io::Cursor::new(bytes), paths, &ImportLimits::default())
    }

    /// CRC-32 (IEEE 802.3) — needed only by the hand-rolled fixture below.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    /// Hand-rolled minimal zip (STORED entries) carrying the SAME entry
    /// name twice. [`ZipWriter`] refuses duplicate names, but real-world
    /// archivers do not — and a duplicate name is exactly how two entries
    /// resolve to one target (P2). No compression, zero extra fields.
    fn zip_bytes_duplicate_name(name: &str, data1: &[u8], data2: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let name = name.as_bytes();
        let mut offsets = Vec::new();
        for data in [data1, data2] {
            offsets.push(out.len() as u32);
            let crc = crc32(data);
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local sig
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method: STORED
            out.extend_from_slice(&0u16.to_le_bytes()); // mod time
            out.extend_from_slice(&0x0021u16.to_le_bytes()); // 1980-01-01
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra len
            out.extend_from_slice(name);
            out.extend_from_slice(data);
        }
        let cd_start = out.len() as u32;
        for (data, offset) in [data1, data2].iter().zip(offsets.iter()) {
            let crc = crc32(data);
            out.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central sig
            out.extend_from_slice(&20u16.to_le_bytes()); // version made by
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0x0021u16.to_le_bytes()); // date
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra len
            out.extend_from_slice(&0u16.to_le_bytes()); // comment len
            out.extend_from_slice(&0u16.to_le_bytes()); // disk number
            out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(name);
        }
        let cd_size = out.len() as u32 - cd_start;
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // EOCD sig
        out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        out.extend_from_slice(&0u16.to_le_bytes()); // cd disk
        out.extend_from_slice(&2u16.to_le_bytes()); // entries this disk
        out.extend_from_slice(&2u16.to_le_bytes()); // entries total
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_start.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out
    }

    #[test]
    fn scan_set_id_variants() {
        let id = |body: &str| scan_set_id(body.as_bytes());
        assert_eq!(id("[Metadata]\nBeatmapSetID: 12789\n"), Some(12789));
        // Absent / -1 / 0 / garbage all mean unsubmitted (None).
        assert_eq!(id("[Metadata]\nTitle: x\n"), None);
        assert_eq!(id("[Metadata]\nBeatmapSetID: -1\n"), None);
        assert_eq!(id("[Metadata]\nBeatmapSetID: 0\n"), None);
        assert_eq!(id("[Metadata]\nBeatmapSetID: abc\n"), None);
        // Section-aware: the key only counts inside [Metadata].
        assert_eq!(id("[Events]\nBeatmapSetID: 5\n[Metadata]\n"), None);
        // Leniency: CRLF, BOM, case-insensitive key, surrounding blanks.
        assert_eq!(
            id("\u{feff}[Events]\r\n[Metadata]\r\n  beatmapsetid :  42  \r\n"),
            Some(42)
        );
    }

    #[test]
    fn submitted_osz_routes_into_set_dir_and_dedups() {
        let base = tmpdir("submitted");
        let paths = paths_from(&base);
        let audio: &[u8] = b"fake-audio-bytes";
        let bg: &[u8] = b"fake-bg-bytes";
        let normal = osu("100", "Normal");
        let extra = osu("100", "Extra");
        let bytes = zip_bytes(&[
            ("audio.mp3", audio),
            ("video.mp4", b"skipped"),
            ("100 - Normal.osu", &normal),
            ("extra/notes.osu", &extra), // nested names land verbatim
            ("bg.jpg", bg),
        ]);
        let report = import(bytes.clone(), &paths).unwrap();
        assert_eq!(report.mode, ImportMode::Submitted { set_id: 100 });
        assert_eq!(report.files.len(), 4);
        assert_eq!(report.stored, 4);
        assert_eq!(report.deduped, 0);
        assert!(report.repaired == 0 && report.replaced == 0);
        assert!(report.index_saved);
        // The video is the only skip.
        assert_eq!(
            report.skipped,
            vec![Skipped {
                name: "video.mp4".to_string(),
                reason: SkipReason::OffAllowlist,
            }]
        );
        // Targets: songs/<set-id>/<entry name>, nested dir preserved.
        for file in &report.files {
            let expected = match file.hash.as_str() {
                h if h == sha256_hex(audio) => "100/audio.mp3",
                h if h == sha256_hex(bg) => "100/bg.jpg",
                h if h == sha256_hex(&normal) => "100/100 - Normal.osu",
                _ => "100/extra/notes.osu",
            };
            assert_eq!(file.path, expected);
            assert_eq!(
                fs::read(paths.songs_dir.join(expected)).unwrap(),
                match file.path.as_str() {
                    "100/audio.mp3" => audio,
                    "100/bg.jpg" => bg,
                    "100/100 - Normal.osu" => &normal[..],
                    _ => &extra[..],
                }
            );
        }
        // Re-import of the same archive: every file dedups (D-41), the
        // index is NOT re-saved, the routing is identical.
        let again = import(bytes, &paths).unwrap();
        assert_eq!(again.mode, report.mode);
        assert_eq!(again.stored, 0);
        assert_eq!(again.deduped, 4);
        assert!(!again.index_saved);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn unsubmitted_osz_lands_under_hash8_dir() {
        let base = tmpdir("unsubmitted");
        let paths = paths_from(&base);
        let audio: &[u8] = b"unshared-audio";
        let d1 = osu("-1", "Diff1");
        let d2 = b"[Metadata]\nTitle: no id key\n".to_vec(); // absent id
        let bytes = zip_bytes(&[("song.mp3", audio), ("diff1.osu", &d1), ("diff2.osu", &d2)]);
        let report = import(bytes.clone(), &paths).unwrap();
        let hash8 = match &report.mode {
            ImportMode::Unsubmitted { hash8 } => hash8.clone(),
            other => panic!("expected unsubmitted, got {other:?}"),
        };
        assert_eq!(hash8.len(), 8);
        // All three files land in the SAME hash8 directory.
        assert_eq!(report.files.len(), 3);
        for file in &report.files {
            let prefix = format!("unsubmitted/{hash8}/");
            assert!(file.path.starts_with(&prefix), "{}", file.path);
        }
        assert!(report.index_saved);
        // Deterministic across imports: same archive → same hash8, and the
        // re-import dedups everywhere without touching the index.
        let again = import(bytes, &paths).unwrap();
        assert_eq!(again.mode, report.mode);
        assert_eq!(again.stored, 0);
        assert_eq!(again.deduped, 3);
        assert!(!again.index_saved);
        fs::remove_dir_all(&base).unwrap();
        // The formula is pinned: sorted <name>\n<sha256>\n lines hashed,
        // first 8 hex chars — a hardcoded vector guards future refactors.
        let mut folded = String::new();
        for (name, data) in [
            ("diff1.osu", &d1[..]),
            ("diff2.osu", &d2[..]),
            ("song.mp3", audio),
        ] {
            folded.push_str(name);
            folded.push('\n');
            folded.push_str(&sha256_hex(data));
            folded.push('\n');
        }
        assert_eq!(hash8, &sha256_hex(folded.as_bytes())[..8]);
    }

    #[test]
    fn hash8_is_independent_of_entry_order() {
        // D-43 determinism: the same kept set packed in a different order
        // must resolve to the same unsubmitted directory.
        let d1 = osu("-1", "Order");
        let audio: &[u8] = b"order-proof-audio";
        let one = zip_bytes(&[("a/diff.osu", &d1), ("b/song.mp3", audio)]);
        let two = zip_bytes(&[("b/song.mp3", audio), ("a/diff.osu", &d1)]);
        let mode_of = |bytes: Vec<u8>| {
            let base = tmpdir("order");
            let paths = paths_from(&base);
            let report = import(bytes, &paths).unwrap();
            fs::remove_dir_all(&base).unwrap();
            report.mode
        };
        assert_eq!(mode_of(one), mode_of(two));
    }

    #[test]
    fn same_name_duplicates_collapse_in_the_reader() {
        // Same RAW name twice: zip 9.6's reader indexes entries by name, so
        // the duplicate never reaches resolution — one addressable entry
        // survives, and the LAST one wins (observed behavior, pinned here
        // so a dependency bump that flips it forces a doc re-check).
        // Different-name collisions still hard-fail: see
        // dot_component_names_collapse_to_one_target.
        let base = tmpdir("dup-collapse");
        let paths = paths_from(&base);
        let first = osu("100", "First");
        let second = osu("100", "Second");
        let bytes = zip_bytes_duplicate_name("map.osu", &first, &second);
        let report = import(bytes, &paths).unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].path, "100/map.osu");
        assert_eq!(report.files[0].hash, sha256_hex(&second));
        assert_eq!(report.stored, 1);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn dot_component_names_collapse_to_one_target() {
        // The normalization hole: two DIFFERENT raw names ("set/a.osu" vs
        // "set/./a.osu") pointing at the same physical file must collapse
        // onto one target string and trip the same hard fail.
        let base = tmpdir("dup-dot");
        let paths = paths_from(&base);
        let a = osu("100", "A");
        let b = osu("100", "B");
        let bytes = zip_bytes(&[("set/map.osu", &a), ("set/./map.osu", &b)]);
        let err = import(bytes, &paths).unwrap_err();
        match &err {
            ImportError::DuplicateTarget {
                target,
                first,
                second,
            } => {
                assert_eq!(target, "100/set/map.osu");
                assert_eq!(
                    (first.as_str(), second.as_str()),
                    ("set/map.osu", "set/./map.osu")
                );
            }
            other => panic!("expected DuplicateTarget, got {other:?}"),
        }
        assert!(!paths.index_file.exists());
        assert!(!paths.songs_dir.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn unsafe_names_and_filter_misses_are_skipped() {
        let base = tmpdir("skips");
        let paths = paths_from(&base);
        let good = osu("100", "Good");
        let bytes = zip_bytes(&[
            ("../evil.osu", &good), // traversal name → UnsafeName skip
            ("notes.osu", &good),   // survives
            ("video.avi", b"v"),    // off-allowlist
            ("story.osb", b"s"),    // off-allowlist
            ("skin.ini", b"k"),     // off-allowlist
        ]);
        let report = import(bytes, &paths).unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].path, "100/notes.osu");
        assert_eq!(report.skipped.len(), 4);
        let reasons: Vec<(&str, &SkipReason)> = report
            .skipped
            .iter()
            .map(|s| (s.name.as_str(), &s.reason))
            .collect();
        assert!(reasons.contains(&("../evil.osu", &SkipReason::UnsafeName)));
        assert!(reasons.contains(&("video.avi", &SkipReason::OffAllowlist)));
        assert!(reasons.contains(&("story.osb", &SkipReason::OffAllowlist)));
        assert!(reasons.contains(&("skin.ini", &SkipReason::OffAllowlist)));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn directory_entries_are_skipped_silently_but_reported() {
        let base = tmpdir("dirs");
        let paths = paths_from(&base);
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = ZipWriter::new(&mut buf);
            w.add_directory("folder", SimpleFileOptions::default())
                .unwrap();
            let data = osu("100", "Nested");
            w.start_file("folder/map.osu", SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut w, &data).unwrap();
            w.finish().unwrap();
        }
        let report = import(buf.into_inner(), &paths).unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].path, "100/folder/map.osu");
        assert_eq!(
            report.skipped,
            vec![Skipped {
                name: "folder/".to_string(),
                reason: SkipReason::Directory,
            }]
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn no_osu_entry_is_not_an_osz() {
        let base = tmpdir("no-osu");
        let paths = paths_from(&base);
        let bytes = zip_bytes(&[("audio.mp3", b"a"), ("bg.jpg", b"b")]);
        let err = import(bytes, &paths).unwrap_err();
        assert!(matches!(err, ImportError::NotAnOsz), "got {err:?}");
        assert!(!paths.index_file.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn conflicting_and_mixed_set_ids_hard_fail() {
        let base = tmpdir("id-conflict");
        let paths = paths_from(&base);
        // Two different submitted ids.
        let a = osu("100", "A");
        let b = osu("200", "B");
        let err = import(zip_bytes(&[("a.osu", &a), ("b.osu", &b)]), &paths).unwrap_err();
        assert!(
            matches!(&err, ImportError::SetIdConflict { found } if found == &[100, 200]),
            "got {err:?}"
        );
        // Submitted mixed with unsubmitted is the same refusal.
        let un = osu("-1", "Un");
        let err = import(zip_bytes(&[("a.osu", &a), ("u.osu", &un)]), &paths).unwrap_err();
        assert!(
            matches!(&err, ImportError::SetIdConflict { found } if found == &[-1, 100]),
            "got {err:?}"
        );
        assert!(!paths.index_file.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn limits_are_enforced_before_any_write() {
        // Per-entry cap (declared), total cap (declared sum), entry-count
        // cap — all fail-before-write (D-44).
        let limits = ImportLimits {
            max_entry_bytes: 8,
            max_total_bytes: 2 * 1024 * 1024 * 1024,
            max_entries: 10_000,
        };
        let base = tmpdir("cap-entry");
        let paths = paths_from(&base);
        let err = import_osz(
            std::io::Cursor::new(zip_bytes(&[("audio.mp3", b"0123456789ABCDEF")])),
            &paths,
            &limits,
        )
        .unwrap_err();
        assert!(
            matches!(
                &err,
                ImportError::OversizeEntry {
                    size: 16,
                    cap: 8,
                    ..
                }
            ),
            "got {err:?}"
        );
        assert!(!paths.index_file.exists());
        fs::remove_dir_all(&base).unwrap();

        let limits = ImportLimits {
            max_entry_bytes: 8,
            max_total_bytes: 10,
            max_entries: 10_000,
        };
        let base = tmpdir("cap-total");
        let paths = paths_from(&base);
        let err = import_osz(
            std::io::Cursor::new(zip_bytes(&[("a.mp3", b"01234567"), ("b.mp3", b"89ABCDEF")])),
            &paths,
            &limits,
        )
        .unwrap_err();
        assert!(
            matches!(&err, ImportError::OversizeTotal { total: 16, cap: 10 }),
            "got {err:?}"
        );
        assert!(!paths.index_file.exists());
        fs::remove_dir_all(&base).unwrap();

        let limits = ImportLimits {
            max_entry_bytes: 1024,
            max_total_bytes: 1024,
            max_entries: 1,
        };
        let base = tmpdir("cap-count");
        let paths = paths_from(&base);
        let err = import_osz(
            std::io::Cursor::new(zip_bytes(&[("a.mp3", b"x"), ("b.mp3", b"y")])),
            &paths,
            &limits,
        )
        .unwrap_err();
        assert!(
            matches!(&err, ImportError::TooManyEntries { count: 2, cap: 1 }),
            "got {err:?}"
        );
        assert!(!paths.index_file.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn update_reimport_replaces_end_to_end() {
        // The osz.rs D-42 unit test, driven through the whole importer: a
        // map update (same set-id + entry name, different bytes) must land
        // as a replace — the stale entry gone, the index claiming only the
        // new hash at the path, unchanged files deduping around it.
        let base = tmpdir("update-e2e");
        let paths = paths_from(&base);
        let audio: &[u8] = b"same-audio";
        let v1 = osu("100", "v1");
        let v2 = osu("100", "v2");
        let one = zip_bytes(&[("map.osu", &v1), ("audio.mp3", audio)]);
        let report1 = import(one, &paths).unwrap();
        assert_eq!(report1.stored, 2);
        assert_eq!(report1.replaced, 0);

        let two = zip_bytes(&[("map.osu", &v2), ("audio.mp3", audio)]);
        let report2 = import(two, &paths).unwrap();
        // The updated map is a fresh store that displaced the old entry;
        // the untouched audio dedups (D-41 reference).
        assert_eq!(report2.stored, 1);
        assert_eq!(report2.replaced, 1);
        assert_eq!(report2.deduped, 1);
        assert!(report2.index_saved);

        let (index, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(dangling.is_empty());
        assert_eq!(index.files().len(), 2); // audio + updated map only
        let old_hash = sha256_hex(&v1);
        let new_hash = sha256_hex(&v2);
        assert!(index.lookup(&old_hash).is_none(), "stale entry survived");
        assert_eq!(index.lookup(&new_hash).unwrap().path, "100/map.osu");
        assert_eq!(fs::read(paths.songs_dir.join("100/map.osu")).unwrap(), v2);
        fs::remove_dir_all(&base).unwrap();
    }

    // ---- run_cli (commit 5): file + library-root glue around import_osz --

    #[test]
    fn run_cli_imports_file_into_explicit_root() {
        let base = tmpdir("run-cli-happy");
        let root = base.join("lib");
        let osz_path = base.join("set.osz");
        let bytes = zip_bytes(&[
            ("audio.mp3", b"run-cli-audio"),
            ("A - B (c) [Easy].osu", &osu("20261115", "Easy")),
        ]);
        fs::write(&osz_path, bytes).unwrap();

        let report = run_cli(&osz_path, Some(&root)).unwrap();
        assert_eq!(report.stored, 2);
        assert!(report.index_saved);
        assert!(report.skipped.is_empty());
        assert_eq!(report.mode, ImportMode::Submitted { set_id: 20261115 });
        assert!(root.join("songs/20261115/audio.mp3").is_file());
        assert!(root.join("songs/20261115/A - B (c) [Easy].osu").is_file());
        assert!(root.join("index.json").is_file());

        // Re-import through the same wrapper: pure D-41 dedup, index
        // byte-identical (saved only when mutated).
        let before = fs::read_to_string(root.join("index.json")).unwrap();
        let again = run_cli(&osz_path, Some(&root)).unwrap();
        assert_eq!(again.stored, 0);
        assert_eq!(again.deduped, 2);
        assert!(!again.index_saved);
        let after = fs::read_to_string(root.join("index.json")).unwrap();
        assert_eq!(before, after);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn run_cli_missing_file_is_a_typed_error() {
        let base = tmpdir("run-cli-open");
        let err = run_cli(&base.join("nope.osz"), Some(&base)).unwrap_err();
        assert!(matches!(err, ImportError::Open { .. }), "got {err:?}");
        assert!(err.to_string().contains("cannot open .osz"));
        // nothing was created — fail-before-write posture extends to the
        // wrapper: the root does not even gain songs/ or index.json
        assert!(!base.join("index.json").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    // ---- D-45: per-set manifests -----------------------------------------

    #[test]
    fn manifest_records_every_kept_entry() {
        let base = tmpdir("manifest-all-entries");
        let paths = paths_from(&base);
        let audio: &[u8] = b"manifest-fixture-audio";
        let map = osu("777", "Manifest");
        let report = import(
            zip_bytes(&[
                ("map.osu", &map),
                ("audio.wav", audio),
                ("video.mp4", b"skipped"),
            ]),
            &paths,
        )
        .unwrap();
        assert_eq!(report.stored, 2);
        let (index, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(dangling.is_empty());
        let manifest = index.sets().get("777").expect("manifest for set 777");
        // kept entries only — the skipped video never enters the manifest
        assert_eq!(manifest.entries.len(), 2);
        assert_eq!(manifest.entries.get("map.osu"), Some(&report.files[0].hash));
        assert_eq!(
            manifest.entries.get("audio.wav"),
            Some(&report.files[1].hash)
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn cross_set_dedup_keeps_set_b_resolvable() {
        // The D-45 headline: set 222's audio dedups into set 111's copy
        // (D-41) — no songs/222/audio.wav on disk — yet 222's manifest
        // maps the entry to the shared hash and resolve_set lands on
        // 111's canonical path. A v1 index could not answer this.
        let base = tmpdir("cross-set-dedup");
        let paths = paths_from(&base);
        let audio: &[u8] = b"shared-audio-bytes";
        let first = import(
            zip_bytes(&[("a.osu", &osu("111", "A")), ("audio.wav", audio)]),
            &paths,
        )
        .unwrap();
        assert_eq!(first.stored, 2);
        let second = import(
            zip_bytes(&[("b.osu", &osu("222", "B")), ("audio.wav", audio)]),
            &paths,
        )
        .unwrap();
        assert_eq!(second.stored, 1); // b.osu only — the audio deduped
        assert_eq!(second.deduped, 1);
        // dedup never writes: 222's dir holds the .osu but not the audio
        assert!(paths.songs_dir.join("222/b.osu").exists());
        assert!(!paths.songs_dir.join("222/audio.wav").exists());
        // the manifest keeps 222 fully resolvable, across the dedup
        let (index, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(dangling.is_empty());
        let resolved = index.resolve_set("222", &paths);
        assert_eq!(
            resolved.get("audio.wav").unwrap(),
            &paths.songs_dir.join("111/audio.wav")
        );
        assert_eq!(
            resolved.get("b.osu").unwrap(),
            &paths.songs_dir.join("222/b.osu")
        );
        // and the resolved audio carries the shared bytes
        assert_eq!(
            fs::read(paths.songs_dir.join("111/audio.wav")).unwrap(),
            audio
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn manifest_change_alone_saves_the_index() {
        // The D-45 heal: a v1-era index (no manifests) + an all-dedup
        // re-import → stored=0, repaired=0, but the manifest changed →
        // index_saved=true and the on-disk file becomes v2. The next
        // re-import changes nothing → no save, byte-identical.
        let base = tmpdir("manifest-heal");
        let paths = paths_from(&base);
        let audio: &[u8] = b"heal-audio-bytes";
        let map = osu("555", "Heal");
        let archive = zip_bytes(&[("map.osu", &map), ("audio.wav", audio)]);
        let first = import(archive.clone(), &paths).unwrap();
        assert!(first.index_saved);

        // Regress the index to the v1 shape an old binary would leave
        // (same files map, no sets, version 1).
        let mut files = serde_json::Map::new();
        for (out, size) in first.files.iter().zip([map.len(), audio.len()]) {
            files.insert(
                out.hash.clone(),
                serde_json::json!({"path": out.path, "size": size}),
            );
        }
        let v1 = serde_json::json!({"version": 1, "files": files}).to_string();
        fs::write(&paths.index_file, v1).unwrap();

        // all-dedup re-import over the v1 index: manifest-only change
        let second = import(archive.clone(), &paths).unwrap();
        assert_eq!(second.stored, 0);
        assert_eq!(second.deduped, 2);
        assert_eq!(second.repaired, 0);
        assert!(second.index_saved, "a manifest-only change must save");
        let raw = fs::read_to_string(&paths.index_file).unwrap();
        assert!(raw.contains("\"version\": 2"));
        assert!(raw.contains("\"sets\""));

        // third import: nothing changes → no save, byte-identical
        let before = fs::read(&paths.index_file).unwrap();
        let third = import(archive, &paths).unwrap();
        assert!(!third.index_saved);
        assert_eq!(fs::read(&paths.index_file).unwrap(), before);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn update_reimport_rewrites_the_manifest() {
        // D-42 + D-45: an update re-import (same set, NEW audio bytes)
        // displaces the old hash; the wholesale-replace manifest must
        // follow the new bytes — the entry never points at the displaced
        // hash.
        let base = tmpdir("manifest-rewrite");
        let paths = paths_from(&base);
        let map = osu("321", "Update");
        let old_audio: &[u8] = b"audio-version-one";
        let new_audio: &[u8] = b"audio-version-two!";
        import(
            zip_bytes(&[("map.osu", &map), ("audio.wav", old_audio)]),
            &paths,
        )
        .unwrap();
        let second = import(
            zip_bytes(&[("map.osu", &map), ("audio.wav", new_audio)]),
            &paths,
        )
        .unwrap();
        assert_eq!(second.stored, 1);
        assert_eq!(second.replaced, 1); // D-42 displacement
        assert!(second.index_saved);
        let (index, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(dangling.is_empty());
        let manifest = index.sets().get("321").expect("manifest for 321");
        assert_eq!(manifest.entries.len(), 2);
        assert_eq!(
            manifest.entries.get("audio.wav"),
            Some(&sha256_hex(new_audio))
        );
        // resolve lands on the NEW bytes
        let resolved = index.resolve_set("321", &paths);
        assert_eq!(fs::read(resolved["audio.wav"].clone()).unwrap(), new_audio);
        fs::remove_dir_all(&base).unwrap();
    }
}
