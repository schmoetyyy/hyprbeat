//! Library index + content-addressed file store (§5.2, D-13, D-41).
//!
//! The library lives under the XDG data dir (I-07, `directories` crate):
//!
//! ```text
//! ~/.local/share/hyprbeat/
//!     songs/<set-id>/…     # extracted files; canonical copies
//!     index.json           # v2 (D-13/D-45): sha256 → {path, size} +
//!                           # sets: set-key → entry-name → sha256
//! ```
//!
//! Import flow (§5.2): hash the incoming bytes; if the hash is already
//! indexed AND its canonical file still exists, record a REFERENCE (D-41)
//! and write nothing; if the entry exists but its canonical file is gone
//! (dangling), re-extract the incoming bytes at the new target and repoint
//! the entry (repair); otherwise store the bytes and add the entry.
//! Identical bytes are therefore stored exactly once — the library-side
//! mirror of the D-32 hash-keyed decode cache.
//!
//! Per-set manifests (D-45): every import also records the set's
//! entry-name → sha256 mapping for EVERY kept entry (deduped or not) —
//! after a cross-set dedup this manifest is the only record that set B
//! "owns" a file whose canonical copy lives in set A's directory. A v1
//! index (pre-manifest) reads as v2-with-empty-`sets`
//! ([`LibraryIndex::load`]); loaders go through
//! [`LibraryIndex::resolve_set`].
//!
//! Update re-imports (D-42): hash-UNKNOWN bytes aimed at a path another
//! entry still claims (same set-id + entry name, different bytes) displace
//! that entry before the write — a stale claim (old hash → new bytes) must
//! never survive the call. Known hashes never displace anything; see
//! [`LibraryIndex::store_file`].
//!
//! Failure posture (I-06/I-08 style): a corrupt `index.json` fails safe —
//! typed error, file left byte-identical on disk, no silent wipe, no temp
//! residue. Index saves are atomic (temp file + fsync + rename, the D-32
//! pattern). The `.osz` importer ([`super::import`]) drives this module in
//! batches: one load, many [`LibraryIndex::store_file`], one save.
//!
//! Zero Wayland/audio I/O; tests supply their own temp roots (I-09).

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Resolved library locations (I-07). Both live in the same data dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryPaths {
    /// Extracted files: `<data dir>/songs` (§5.2).
    pub songs_dir: PathBuf,
    /// The library index: `<data dir>/index.json` (D-13).
    pub index_file: PathBuf,
}

/// Library paths inside `data_dir` (pure; the §5.2 layout is pinned by
/// tests). Mirrors `audio::decode::cache_path`'s caller-supplied-root
/// pattern so tests never touch the real XDG dirs.
pub fn paths_from(data_dir: &Path) -> LibraryPaths {
    LibraryPaths {
        songs_dir: data_dir.join("songs"),
        index_file: data_dir.join("index.json"),
    }
}

/// XDG-resolved default library paths (`<data>/hyprbeat/{songs,index.json}`).
/// `None` only where the environment has no usable home/data dir; callers
/// degrade gracefully (I-08 posture) instead of panicking.
pub fn default_paths() -> Option<LibraryPaths> {
    ProjectDirs::from("", "", "hyprbeat").map(|dirs| paths_from(dirs.data_dir()))
}

/// Library errors. [`LibraryError::Corrupt`] never touches the file on disk
/// (fail-safe, no silent wipe).
#[derive(Debug)]
pub enum LibraryError {
    /// `index.json` exists but cannot be trusted: unparsable JSON, missing
    /// fields, or an unsupported schema version. The file is left
    /// byte-identical; recovery is a human decision, never an auto-wipe.
    Corrupt { path: PathBuf, reason: String },
    /// Filesystem I/O failure at `path`.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A songs-relative target path tried to escape the songs tree.
    BadPath(String),
}

impl fmt::Display for LibraryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt { path, reason } => write!(
                f,
                "library index at {} is corrupt, refusing to touch it: {reason}",
                path.display()
            ),
            Self::Io { path, source } => {
                write!(f, "library I/O failed at {}: {source}", path.display())
            }
            Self::BadPath(raw) => write!(f, "rejected unsafe library path: {raw:?}"),
        }
    }
}

impl std::error::Error for LibraryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// `index.json` schema version (D-45: v2 adds per-set manifests). Bumped
/// only with a migration story; an index written by a newer schema is
/// refused (fail-safe), not guessed at. A v1 index is READ as
/// v2-with-empty-`sets` ([`LibraryIndex::load`]); the reverse direction
/// is deliberately unsupported — an old binary cannot read a v2 index
/// (its version check refuses), so the binary upgrades first, the index
/// follows on its next save.
pub const INDEX_VERSION: u32 = 2;

/// The schema version read leniently (D-45 migration): a v1 index parses
/// as v2-with-empty-`sets` and upgrades in memory; see [`LibraryIndex::load`].
pub const INDEX_VERSION_V1: u32 = 1;

/// One stored file: the canonical copy under `songs/` (D-41: identical
/// bytes live once; other imports of the same hash reference this entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Canonical location relative to [`LibraryPaths::songs_dir`],
    /// `/`-separated, validated by [`sanitize_rel_path`] at import time.
    pub path: String,
    /// Byte size of the stored file.
    pub size: u64,
}

/// One set's manifest (D-45): entry name → content hash for every file
/// the importer kept (deduped or not). This is the piece v1 could not
/// represent — after a cross-set dedup the `files` map alone cannot answer
/// "which files belong to this set" (the requesting set's name→hash
/// mapping was lost at dedup time), so loaders resolve
/// set → manifest → hashes → canonical paths
/// ([`LibraryIndex::resolve_set`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetManifest {
    /// Entry name (the importer's normalized songs-relative name inside
    /// the set directory) → lowercase-hex SHA-256 of the stored bytes.
    pub entries: BTreeMap<String, String>,
}

/// The library index (`index.json`, D-13 schema v2). `files` keys are
/// lowercase-hex SHA-256 digests; `sets` keys are set-keys (`<set-id>` or
/// `unsubmitted/<hash8>`, the D-43 routing keys).
/// [`std::collections::BTreeMap`] keeps the serialized file deterministic
/// (stable diffs, stable tests).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryIndex {
    version: u32,
    files: BTreeMap<String, IndexEntry>,
    /// Per-set manifests (D-45). `#[serde(default)]` so a v1 index —
    /// written before manifests existed — parses with an empty map.
    #[serde(default)]
    sets: BTreeMap<String, SetManifest>,
}

impl Default for LibraryIndex {
    /// A fresh, empty index — already stamped with [`INDEX_VERSION`], so a
    /// first-run save/load cycle round-trips.
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            files: BTreeMap::new(),
            sets: BTreeMap::new(),
        }
    }
}

/// Outcome of storing one file (the D-41 outcomes + the D-42 displacement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    /// Lowercase-hex SHA-256 of the incoming bytes.
    pub hash: String,
    /// Canonical `songs/`-relative path now recorded in the index.
    pub path: String,
    /// Known hash + intact canonical file: nothing was written, the
    /// existing copy is referenced (D-41 reference).
    pub deduped: bool,
    /// Known hash + MISSING canonical file: the incoming bytes were
    /// re-extracted at the new target and the entry repointed (repair).
    pub repaired: bool,
    /// Update re-import (D-42): the hash of the entry that claimed the
    /// target path and was displaced (entry removed, its bytes overwritten
    /// by the incoming ones). `None` when nothing was displaced.
    pub replaced: Option<String>,
}

impl LibraryIndex {
    /// Load `index.json`, verifying that every entry's canonical file
    /// exists (D-41 ruling, "on load").
    ///
    /// Returns the index plus the hashes of DANGLING entries (canonical
    /// file missing). Dangling entries are KEPT, never dropped silently:
    /// the next import of the same hash repairs them (D-41). A missing
    /// index file is a normal first run → empty index. A corrupt or
    /// wrong-version index is a hard [`LibraryError::Corrupt`] — the file
    /// is never rewritten or deleted here.
    pub fn load(paths: &LibraryPaths) -> Result<(Self, Vec<String>), LibraryError> {
        let file = &paths.index_file;
        if !file.exists() {
            return Ok((Self::default(), Vec::new()));
        }
        let bytes = fs::read(file).map_err(|e| LibraryError::Io {
            path: file.clone(),
            source: e,
        })?;
        let mut parsed: Self =
            serde_json::from_slice(&bytes).map_err(|e| LibraryError::Corrupt {
                path: file.clone(),
                reason: format!("JSON: {e}"),
            })?;
        if parsed.version == INDEX_VERSION_V1 {
            // D-45: a v1 index reads as v2-with-empty-`sets`. The upgrade
            // happens IN MEMORY only (this load never writes); the next
            // save — which only happens once the index mutates — stamps
            // v2. Re-importing the .osz files rebuilds the manifests for
            // free (D-41 dedup stores nothing). An old binary reading a
            // v2 index still fails its own version check — the
            // compatibility is one-directional on purpose (D-13
            // fail-safe).
            tracing::warn!(
                "library index is schema v1 (pre-D-45): no per-set manifests — re-import your .osz files to rebuild them (dedup makes that free); the index becomes v2 on its next save"
            );
            parsed.version = INDEX_VERSION;
        } else if parsed.version != INDEX_VERSION {
            return Err(LibraryError::Corrupt {
                path: file.clone(),
                reason: format!(
                    "unsupported schema version {} (expected {INDEX_VERSION})",
                    parsed.version
                ),
            });
        }
        let mut dangling = Vec::new();
        for (hash, entry) in &parsed.files {
            if !paths.songs_dir.join(&entry.path).exists() {
                dangling.push(hash.clone());
            }
        }
        if !dangling.is_empty() {
            tracing::warn!(
                count = dangling.len(),
                "library index: dangling entries (canonical file missing); kept, repaired on re-import (D-41)"
            );
        }
        Ok((parsed, dangling))
    }

    /// Atomically replace `index.json`: serialize → write temp file →
    /// fsync → rename over the target (the D-32 pattern). On any failure
    /// the previous index file is left untouched.
    pub fn save(&self, paths: &LibraryPaths) -> Result<(), LibraryError> {
        let file = &paths.index_file;
        let json = serde_json::to_string_pretty(self).map_err(|e| LibraryError::Corrupt {
            path: file.clone(),
            reason: format!("serialize: {e}"),
        })?;
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|e| LibraryError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }
        // "index.json" → "index.json.tmp": a stray temp is obvious to a
        // human inspecting the data dir.
        let tmp = file.with_extension("json.tmp");
        {
            let mut f = fs::File::create(&tmp).map_err(|e| LibraryError::Io {
                path: tmp.clone(),
                source: e,
            })?;
            f.write_all(json.as_bytes()).map_err(|e| LibraryError::Io {
                path: tmp.clone(),
                source: e,
            })?;
            f.sync_all().map_err(|e| LibraryError::Io {
                path: tmp.clone(),
                source: e,
            })?;
        }
        fs::rename(&tmp, file).map_err(|e| LibraryError::Io {
            path: tmp,
            source: e,
        })?;
        Ok(())
    }

    /// Store one incoming file: write the bytes (unless deduped), update
    /// the in-memory index, and report the outcome. Does NOT save the
    /// index — the `.osz` importer will batch many [`Self::store_file`]
    /// calls between one [`Self::load`] and one [`Self::save`].
    ///
    /// Ordering guarantee: bytes hit disk BEFORE the index can reference
    /// them, so a crash never leaves the index claiming a missing file —
    /// worst case is an unindexed extra file that the next import rewrites.
    ///
    /// D-42 (update re-import): when the incoming hash is NEW and another
    /// entry still claims the target path, that entry is displaced first
    /// (reported as [`Imported::replaced`]) — its bytes are about to be
    /// overwritten, and a surviving claim would make the index lie. A KNOWN
    /// hash never displaces anything (D-41 reference wins, nothing written),
    /// even when the requested target is claimed by another entry. The
    /// dangling-repair branch carries the same guard.
    pub fn store_file(
        &mut self,
        bytes: &[u8],
        raw_target: &str,
        paths: &LibraryPaths,
    ) -> Result<Imported, LibraryError> {
        let target = sanitize_rel_path(raw_target)?;
        let hash = sha256_hex(bytes);
        if let Some(entry) = self.files.get(&hash) {
            if paths.songs_dir.join(&entry.path).exists() {
                // D-41 reference: identical bytes already stored — keep.
                // Known hashes never displace the target's claimant (D-42).
                return Ok(Imported {
                    hash,
                    path: entry.path.clone(),
                    deduped: true,
                    repaired: false,
                    replaced: None,
                });
            }
            // Dangling: canonical copy is gone — re-extract the incoming
            // bytes at the NEW target and repoint the entry (D-41 repair).
            // D-42 guard: the new target may be claimed by another entry
            // whose bytes the write below would clobber.
            let displaced = self.displace_claimant(&target, Some(&hash));
            write_store_file(&paths.songs_dir, &target, bytes)?;
            self.files.insert(
                hash.clone(),
                IndexEntry {
                    path: target.clone(),
                    size: bytes.len() as u64,
                },
            );
            return Ok(Imported {
                hash,
                path: target,
                deduped: false,
                repaired: true,
                replaced: displaced,
            });
        }
        // D-42: hash-unknown bytes at a claimed path = map update. The
        // write below overwrites the claimant's canonical copy, so its
        // entry must go first — the index never claims a path whose bytes
        // belong to another hash.
        let displaced = self.displace_claimant(&target, None);
        write_store_file(&paths.songs_dir, &target, bytes)?;
        self.files.insert(
            hash.clone(),
            IndexEntry {
                path: target.clone(),
                size: bytes.len() as u64,
            },
        );
        Ok(Imported {
            hash,
            path: target,
            deduped: false,
            repaired: false,
            replaced: displaced,
        })
    }

    /// D-42 helper: remove the entry whose canonical path is `target` (if
    /// any, apart from `except`) and return its hash. Called immediately
    /// before bytes are written over that path.
    fn displace_claimant(&mut self, target: &str, except: Option<&str>) -> Option<String> {
        let found = self
            .files
            .iter()
            .find(|(hash, entry)| Some(hash.as_str()) != except && entry.path == target)
            .map(|(hash, _)| hash.clone());
        if let Some(hash) = found {
            self.files.remove(&hash);
            Some(hash)
        } else {
            None
        }
    }

    /// Self-contained import: load → store → save. Convenience for
    /// single-file callers; the `.osz` importer batches instead.
    pub fn import_file(
        bytes: &[u8],
        raw_target: &str,
        paths: &LibraryPaths,
    ) -> Result<Imported, LibraryError> {
        let (mut index, _dangling) = Self::load(paths)?;
        let out = index.store_file(bytes, raw_target, paths)?;
        index.save(paths)?;
        Ok(out)
    }

    /// Index entry for a content hash, if known.
    pub fn lookup(&self, hash: &str) -> Option<&IndexEntry> {
        self.files.get(hash)
    }

    /// All entries, keyed by lowercase-hex SHA-256.
    pub fn files(&self) -> &BTreeMap<String, IndexEntry> {
        &self.files
    }

    /// The schema version this index was written with. A v1 file reports
    /// [`INDEX_VERSION`] here already — the in-memory upgrade happened at
    /// load; the DISK file keeps its old bytes until the next save.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// All set manifests, keyed by set-key (D-45). A v1-loaded index has
    /// an empty map until its sets are re-imported.
    pub fn sets(&self) -> &BTreeMap<String, SetManifest> {
        &self.sets
    }

    /// Record one set's manifest (D-45), WHOLESALE: the `.osz` is the
    /// source of truth for its own set, so a re-import replaces the
    /// manifest outright (the D-42 replace posture applied to manifests —
    /// a merge could keep entries the new archive no longer ships).
    /// Entry names are unique per import (the duplicate-target check
    /// refuses collisions before the store pass). Returns whether the
    /// index changed — the importer folds this into its save decision.
    pub fn record_set_manifest(
        &mut self,
        set_key: &str,
        entries: BTreeMap<String, String>,
    ) -> bool {
        let manifest = SetManifest { entries };
        if self.sets.get(set_key) == Some(&manifest) {
            return false;
        }
        self.sets.insert(set_key.to_string(), manifest);
        true
    }

    /// Resolve a set's entries to canonical paths (D-45):
    /// set → manifest → hashes → `files` entries → `songs_dir`-joined
    /// paths. Works across dedups: an entry whose canonical bytes live in
    /// ANOTHER set's directory resolves to that copy. Entries whose hash
    /// is missing from `files` (a stale manifest — e.g. a D-42
    /// displacement elsewhere removed the record) are skipped with a
    /// warn, never fatal; re-importing the set heals the manifest. An
    /// unknown set-key yields an empty map.
    pub fn resolve_set(&self, set_key: &str, paths: &LibraryPaths) -> BTreeMap<String, PathBuf> {
        let mut out = BTreeMap::new();
        let Some(manifest) = self.sets.get(set_key) else {
            return out;
        };
        for (name, hash) in &manifest.entries {
            match self.files.get(hash) {
                Some(entry) => {
                    out.insert(name.clone(), paths.songs_dir.join(&entry.path));
                }
                None => {
                    tracing::warn!(
                        set = %set_key,
                        entry = %name,
                        "library index: manifest entry has no file record (stale manifest?) — skipped; re-import the set to heal (D-45)"
                    );
                }
            }
        }
        out
    }
}

/// Lowercase-hex SHA-256 of `bytes` (content addressing, §5.2/D-32).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(2 * digest.len());
    for b in digest {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Validate a songs-relative target path: relative, `/`-separated, no `..`
/// components, no backslashes or NUL, at least one non-empty component.
/// `.osz` zip entries (next commit) are untrusted names — this is the
/// traversal gate; the importer pre-filters skips with it, hard failures
/// surface as [`LibraryError::BadPath`].
pub fn sanitize_rel_path(raw: &str) -> Result<String, LibraryError> {
    let bad = || LibraryError::BadPath(raw.to_string());
    if raw.is_empty() || raw.contains('\\') || raw.contains('\0') {
        return Err(bad());
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(bad());
    }
    let mut any_normal = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => any_normal = true,
            Component::CurDir => {} // "a/./b" is harmless; keep it simple
            _ => return Err(bad()), // ParentDir / RootDir / Prefix
        }
    }
    if !any_normal {
        return Err(bad());
    }
    Ok(raw.to_string())
}

/// Write bytes to `songs_dir/rel`, creating parent directories. A plain
/// write is enough here: until the index references the file it is garbage
/// that the next import rewrites (see [`LibraryIndex::store_file`]).
fn write_store_file(songs_dir: &Path, rel: &str, bytes: &[u8]) -> Result<(), LibraryError> {
    let full = songs_dir.join(rel);
    let parent = full
        .parent()
        .ok_or_else(|| LibraryError::BadPath(rel.to_string()))?;
    fs::create_dir_all(parent).map_err(|e| LibraryError::Io {
        path: parent.to_path_buf(),
        source: e,
    })?;
    fs::write(&full, bytes).map_err(|e| LibraryError::Io {
        path: full,
        source: e,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Unique temp root per call (pid + sequence): parallel tests never
    /// collide, and no `$HOME`/env mutation is needed (I-09: test-only I/O
    /// under an explicitly supplied root).
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hyprbeat-osz-{}-{}-{}",
            tag,
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const BYTES_A: &[u8] = b"hyprbeat fixture alpha";
    const BYTES_B: &[u8] = b"hyprbeat fixture beta";

    #[test]
    fn sha256_known_vectors() {
        // Standard NIST vectors (SHA-256 of "abc" and of the empty string).
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn paths_follow_52_layout() {
        let paths = paths_from(Path::new("/data/root"));
        assert_eq!(paths.songs_dir, PathBuf::from("/data/root/songs"));
        assert_eq!(paths.index_file, PathBuf::from("/data/root/index.json"));
        // The XDG default always lands inside a `hyprbeat` data dir (I-07),
        // whatever $HOME / XDG_DATA_HOME say — suffix assertions only, so
        // the test is env-independent and race-free.
        let default = default_paths().expect("XDG data dir resolves in test envs");
        assert!(default.songs_dir.ends_with("hyprbeat/songs"));
        assert!(default.index_file.ends_with("hyprbeat/index.json"));
        assert_eq!(default.songs_dir.parent(), default.index_file.parent());
    }

    #[test]
    fn sanitize_rejects_traversal_and_absolute() {
        for bad in [
            "../evil.bin",
            "a/../../evil.bin",
            "/abs/path.bin",
            "",
            "..",
            ".",
            "a\\b.bin",
            "a\0b",
        ] {
            assert!(sanitize_rel_path(bad).is_err(), "should reject {bad:?}");
        }
        assert_eq!(sanitize_rel_path("set/audio.mp3").unwrap(), "set/audio.mp3");
        assert_eq!(sanitize_rel_path("a/./b.bin").unwrap(), "a/./b.bin");
    }

    #[test]
    fn store_new_file_writes_and_indexes() {
        let base = tmpdir("store-new");
        let paths = paths_from(&base);
        let mut index = LibraryIndex::default();
        let out = index
            .store_file(BYTES_A, "12345/audio.mp3", &paths)
            .unwrap();
        assert!(!out.deduped && !out.repaired);
        assert_eq!(out.hash, sha256_hex(BYTES_A));
        assert_eq!(out.path, "12345/audio.mp3");
        assert_eq!(
            fs::read(paths.songs_dir.join("12345/audio.mp3")).unwrap(),
            BYTES_A
        );
        index.save(&paths).unwrap();
        let (reloaded, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(dangling.is_empty());
        assert_eq!(reloaded, index);
        assert_eq!(reloaded.version(), INDEX_VERSION);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn dedup_hit_references_existing_copy() {
        let base = tmpdir("dedup");
        let paths = paths_from(&base);
        let mut index = LibraryIndex::default();
        index
            .store_file(BYTES_A, "set-one/audio.mp3", &paths)
            .unwrap();
        let again = index
            .store_file(BYTES_A, "set-two/audio.mp3", &paths)
            .unwrap();
        assert!(again.deduped && !again.repaired);
        // References the canonical copy; the new target was never written.
        assert_eq!(again.path, "set-one/audio.mp3");
        assert!(!paths.songs_dir.join("set-two").exists());
        assert_eq!(index.files().len(), 1);
        index.save(&paths).unwrap();
        let (reloaded, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(dangling.is_empty());
        assert_eq!(
            reloaded.lookup(&again.hash).unwrap().path,
            "set-one/audio.mp3"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn missing_index_is_first_run_and_import_file_round_trips() {
        let base = tmpdir("first-run");
        let paths = paths_from(&base);
        let (index, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(index.files().is_empty() && dangling.is_empty());
        let out = LibraryIndex::import_file(BYTES_A, "9/audio.ogg", &paths).unwrap();
        assert!(!out.deduped && !out.repaired);
        let (index, dangling) = LibraryIndex::load(&paths).unwrap();
        assert_eq!(index.lookup(&out.hash).unwrap().path, "9/audio.ogg");
        assert!(dangling.is_empty());
        // Second identical import through the same wrapper dedups (D-41).
        let out2 = LibraryIndex::import_file(BYTES_A, "9/other.ogg", &paths).unwrap();
        assert!(out2.deduped);
        assert_eq!(out2.path, "9/audio.ogg");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn dangling_entry_detected_on_load_and_repaired_on_reimport() {
        let base = tmpdir("dangling");
        let paths = paths_from(&base);
        let mut index = LibraryIndex::default();
        let first = index
            .store_file(BYTES_A, "gone-set/audio.mp3", &paths)
            .unwrap();
        index.save(&paths).unwrap();
        // Simulate the canonical copy vanishing (a deleted set directory).
        fs::remove_dir_all(paths.songs_dir.join("gone-set")).unwrap();

        // Load detects the dangling entry and KEEPS it (no silent drop).
        let (loaded, dangling) = LibraryIndex::load(&paths).unwrap();
        assert_eq!(dangling, vec![first.hash.clone()]);
        assert!(loaded.lookup(&first.hash).is_some());

        // Re-import of the same bytes at a new target repairs it (D-41).
        let mut index = loaded;
        let out = index
            .store_file(BYTES_A, "fresh-set/audio.mp3", &paths)
            .unwrap();
        assert!(!out.deduped && out.repaired);
        assert_eq!(out.path, "fresh-set/audio.mp3");
        assert_eq!(
            fs::read(paths.songs_dir.join("fresh-set/audio.mp3")).unwrap(),
            BYTES_A
        );
        assert_eq!(
            index.lookup(&first.hash).unwrap().path,
            "fresh-set/audio.mp3"
        );
        index.save(&paths).unwrap();
        let (final_index, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(dangling.is_empty());
        assert_eq!(
            final_index.lookup(&first.hash).unwrap().path,
            "fresh-set/audio.mp3"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn corrupt_index_fails_safe_no_wipe() {
        let base = tmpdir("corrupt");
        let paths = paths_from(&base);
        fs::create_dir_all(&paths.songs_dir).unwrap();
        let garbage: &[u8] = b"{ this is not json ...";
        fs::write(&paths.index_file, garbage).unwrap();

        let err = LibraryIndex::load(&paths).unwrap_err();
        assert!(matches!(&err, LibraryError::Corrupt { .. }), "got {err:?}");
        // The file is byte-identical afterwards (no wipe, no rewrite).
        assert_eq!(fs::read(&paths.index_file).unwrap(), garbage);
        // No temp file residue.
        assert!(!paths.index_file.with_extension("json.tmp").exists());

        // Parsed-but-unknown schema versions are equally refused.
        const WRONG_VERSION: &[u8] = br#"{"version":99,"files":{}}"#;
        fs::write(&paths.index_file, WRONG_VERSION).unwrap();
        let err = LibraryIndex::load(&paths).unwrap_err();
        assert!(matches!(&err, LibraryError::Corrupt { .. }), "got {err:?}");
        assert_eq!(fs::read(&paths.index_file).unwrap(), WRONG_VERSION);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn save_is_atomic_no_tmp_residue() {
        let base = tmpdir("atomic");
        let paths = paths_from(&base);
        let mut index = LibraryIndex::default();
        index.store_file(BYTES_A, "a.bin", &paths).unwrap();
        index.save(&paths).unwrap();
        index.store_file(BYTES_B, "b.bin", &paths).unwrap();
        index.save(&paths).unwrap(); // second save over the first, cleanly
        assert!(!paths.index_file.with_extension("json.tmp").exists());
        let (reloaded, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(dangling.is_empty());
        assert_eq!(reloaded, index);
        assert_eq!(reloaded.files().len(), 2);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn update_reimport_replaces_stale_entry() {
        // D-42 (mandated test): same logical path, different bytes. The new
        // store must displace the old entry instead of leaving the index
        // claiming the OLD hash at a path that now holds the NEW bytes.
        let base = tmpdir("update-replace");
        let paths = paths_from(&base);
        let mut index = LibraryIndex::default();
        let old = index.store_file(BYTES_A, "100/audio.mp3", &paths).unwrap();
        assert!(old.replaced.is_none());
        index.save(&paths).unwrap();

        let new = index.store_file(BYTES_B, "100/audio.mp3", &paths).unwrap();
        assert!(!new.deduped && !new.repaired);
        assert_eq!(new.replaced.as_deref(), Some(old.hash.as_str()));
        // Exactly one entry survives: the new hash → the path.
        assert_eq!(index.files().len(), 1);
        assert!(index.lookup(&old.hash).is_none());
        assert_eq!(index.lookup(&new.hash).unwrap().path, "100/audio.mp3");
        // The bytes on disk are the new ones, and the reloaded index agrees
        // (no dangling, no stale claim).
        assert_eq!(
            fs::read(paths.songs_dir.join("100/audio.mp3")).unwrap(),
            BYTES_B
        );
        index.save(&paths).unwrap();
        let (reloaded, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(dangling.is_empty());
        assert_eq!(reloaded, index);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn known_hash_dedup_never_displaces_target() {
        // D-41 precedence over D-42: a known hash is a reference and never
        // writes — even when the requested target is claimed by another
        // entry. That entry's canonical copy stays intact.
        let base = tmpdir("dedup-no-displace");
        let paths = paths_from(&base);
        let mut index = LibraryIndex::default();
        let a = index.store_file(BYTES_A, "100/audio.mp3", &paths).unwrap();
        let _b = index.store_file(BYTES_B, "200/audio.mp3", &paths).unwrap();
        let again = index.store_file(BYTES_B, "100/audio.mp3", &paths).unwrap();
        assert!(again.deduped && !again.repaired);
        assert!(again.replaced.is_none());
        assert_eq!(again.path, "200/audio.mp3");
        assert_eq!(index.files().len(), 2);
        assert_eq!(index.lookup(&a.hash).unwrap().path, "100/audio.mp3");
        // The bytes at the disputed target are still the original ones.
        assert_eq!(
            fs::read(paths.songs_dir.join("100/audio.mp3")).unwrap(),
            BYTES_A
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn dangling_repair_displaces_claimant_of_new_target() {
        // D-41 repair + D-42 guard combined: re-extracting a dangling hash
        // at a target another entry claims must displace that entry, or the
        // write would silently clobber its canonical bytes.
        let base = tmpdir("repair-displace");
        let paths = paths_from(&base);
        let mut index = LibraryIndex::default();
        let a = index.store_file(BYTES_A, "100/audio.mp3", &paths).unwrap();
        let b = index.store_file(BYTES_B, "200/audio.mp3", &paths).unwrap();
        index.save(&paths).unwrap();
        // B's canonical copy vanishes; re-import B targeting 100's path.
        fs::remove_dir_all(paths.songs_dir.join("200")).unwrap();
        let (loaded, dangling) = LibraryIndex::load(&paths).unwrap();
        assert_eq!(dangling, vec![b.hash.clone()]);
        let mut index = loaded;
        let out = index.store_file(BYTES_B, "100/audio.mp3", &paths).unwrap();
        assert!(out.repaired);
        assert_eq!(out.replaced.as_deref(), Some(a.hash.as_str()));
        assert_eq!(index.files().len(), 1);
        assert_eq!(index.lookup(&b.hash).unwrap().path, "100/audio.mp3");
        assert_eq!(
            fs::read(paths.songs_dir.join("100/audio.mp3")).unwrap(),
            BYTES_B
        );
        fs::remove_dir_all(&base).unwrap();
    }

    // ---- D-45: schema v2, per-set manifests, v1-reads-as-v2 -------------

    #[test]
    fn v1_index_reads_as_v2_with_empty_sets() {
        // D-45 migration, load side: a v1 index (pre-manifest) parses as
        // v2-with-empty-`sets`, upgrades to v2 IN MEMORY (the next save
        // writes v2), and the load itself leaves the file byte-identical.
        // The reverse is false on purpose: an OLD binary cannot read a v2
        // index (its version check refuses) — upgrade the binary first.
        let base = tmpdir("v1-reads-as-v2");
        let paths = paths_from(&base);
        let hash = sha256_hex(BYTES_A);
        let mut files = serde_json::Map::new();
        files.insert(
            hash.clone(),
            serde_json::json!({"path": "123/audio.mp3", "size": BYTES_A.len()}),
        );
        let v1 = serde_json::json!({"version": 1, "files": files}).to_string();
        fs::create_dir_all(paths.songs_dir.join("123")).unwrap();
        fs::write(paths.songs_dir.join("123/audio.mp3"), BYTES_A).unwrap();
        fs::write(&paths.index_file, &v1).unwrap();

        let (index, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(dangling.is_empty());
        assert_eq!(index.version(), INDEX_VERSION, "upgraded in memory");
        assert!(index.sets().is_empty());
        assert_eq!(index.lookup(&hash).unwrap().path, "123/audio.mp3");
        // the load itself never rewrote the v1 file
        assert_eq!(fs::read_to_string(&paths.index_file).unwrap(), v1);

        // a save (which only happens on mutation) now writes v2 + sets
        index.save(&paths).unwrap();
        let raw = fs::read_to_string(&paths.index_file).unwrap();
        assert!(raw.contains("\"version\": 2"));
        assert!(raw.contains("\"sets\": {}"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn v2_round_trip_with_manifests() {
        let base = tmpdir("v2-roundtrip");
        let paths = paths_from(&base);
        let mut index = LibraryIndex::default();
        let a = index.store_file(BYTES_A, "100/a.mp3", &paths).unwrap();
        let b = index.store_file(BYTES_B, "100/b.osu", &paths).unwrap();
        let mut entries = BTreeMap::new();
        entries.insert("a.mp3".to_string(), a.hash.clone());
        entries.insert("b.osu".to_string(), b.hash.clone());
        assert!(index.record_set_manifest("100", entries.clone()));
        // re-recording the identical manifest is a no-op — this feeds the
        // "save only when mutated" decision
        assert!(!index.record_set_manifest("100", entries));
        index.save(&paths).unwrap();
        let (reloaded, dangling) = LibraryIndex::load(&paths).unwrap();
        assert!(dangling.is_empty());
        assert_eq!(reloaded, index);
        assert_eq!(reloaded.version(), INDEX_VERSION);
        assert_eq!(reloaded.sets()["100"].entries.len(), 2);
        assert_eq!(reloaded.sets()["100"].entries["a.mp3"], a.hash);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn schema_versions_other_than_1_and_2_are_refused() {
        // D-13/D-45: v2 is the only writer, v1 is read-as-v2; everything
        // else (0, 3, 99, …) is a hard Corrupt and the file stays
        // byte-identical (no wipe, no rewrite).
        let base = tmpdir("version-refusal");
        let paths = paths_from(&base);
        fs::create_dir_all(&paths.songs_dir).unwrap();
        let cases: [&[u8]; 3] = [
            br#"{"version":0,"files":{}}"#,
            br#"{"version":3,"files":{}}"#,
            br#"{"version":99,"files":{}}"#,
        ];
        for raw in cases {
            fs::write(&paths.index_file, raw).unwrap();
            let err = LibraryIndex::load(&paths).unwrap_err();
            assert!(matches!(&err, LibraryError::Corrupt { .. }), "got {err:?}");
            assert_eq!(fs::read(&paths.index_file).unwrap(), raw);
        }
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn resolve_set_maps_entries_to_canonical_paths() {
        // Cross-set dedup geometry at the index level (the D-45 headline):
        // set 300's manifest points at bytes whose canonical copy lives in
        // set 100 — resolve lands on 100's path, not 300's.
        let base = tmpdir("resolve-set");
        let paths = paths_from(&base);
        let mut index = LibraryIndex::default();
        let a = index.store_file(BYTES_A, "100/audio.mp3", &paths).unwrap();
        let b = index.store_file(BYTES_B, "200/map.osu", &paths).unwrap();
        let mut entries = BTreeMap::new();
        entries.insert("audio.mp3".to_string(), a.hash.clone());
        entries.insert("map.osu".to_string(), b.hash.clone());
        index.record_set_manifest("300", entries);
        let resolved = index.resolve_set("300", &paths);
        assert_eq!(resolved.len(), 2);
        assert_eq!(
            resolved.get("audio.mp3").unwrap(),
            &paths.songs_dir.join("100/audio.mp3")
        );
        assert_eq!(
            resolved.get("map.osu").unwrap(),
            &paths.songs_dir.join("200/map.osu")
        );
        // unknown set-key: empty, not an error (a v1-loaded index has no
        // manifests at all)
        assert!(index.resolve_set("999", &paths).is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn resolve_set_skips_entries_missing_from_files() {
        // A stale manifest entry (hash absent from `files` — e.g. a D-42
        // displacement elsewhere removed the record) is skipped with a
        // warn, never fatal; its resolvable sibling still resolves.
        // Re-importing the set heals the manifest.
        let base = tmpdir("resolve-skip");
        let paths = paths_from(&base);
        let mut index = LibraryIndex::default();
        let a = index.store_file(BYTES_A, "100/a.mp3", &paths).unwrap();
        let ghost = sha256_hex(BYTES_B); // hash of bytes never stored
        let mut entries = BTreeMap::new();
        entries.insert("a.mp3".to_string(), a.hash.clone());
        entries.insert("ghost.mp3".to_string(), ghost);
        index.record_set_manifest("100", entries);
        let resolved = index.resolve_set("100", &paths);
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved.get("a.mp3").unwrap(),
            &paths.songs_dir.join("100/a.mp3")
        );
        fs::remove_dir_all(&base).unwrap();
    }
}
