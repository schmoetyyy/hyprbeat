#!/usr/bin/env bash
# hyprbeat phase-3 import validation — run this ON THE DEV MACHINE
# (NixOS + Hyprland), for the commit-5 scope: --import / --library-root CLI
# wiring (§5.2, D-43/D-44). Commit 6 rider (D-45): step 2 also asserts
# the index is schema v2 with the per-set `sets` manifest.
#
#   [0] build inside the flake devshell + offline gates (fmt/clippy/test)
#   [1] committed synthetic fixtures are byte-exact (sha256 -c, D-34 pattern)
#   [2] submitted .osz: exit 0, summary line, on-disk layout, index written
#   [3] re-import: pure D-41 dedup, index.json byte-identical (saved only
#       when mutated)
#   [4] skip reporting: video/.osb/skin/.txt off-allowlist + dir entry
#   [5] unsubmitted .osz: songs/unsubmitted/<hash8>/ routing (D-43)
#   [6] hostile .osz (central dir declares 600 MiB, cap 512 MiB): exits
#       non-zero, writes NOTHING (fail-before-write, D-44)
#   [7] no --library-root: XDG_DATA_HOME override lands in <dir>/hyprbeat/
#       (I-07 default-path machinery)
#   [8] OSZ_SPECIMEN=/path/to/real.osz — REAL user-supplied specimen (kept
#       OUTSIDE the repo, I-03): import + layout + real-bytes dedup, plus
#       the Q-10 spot check (decode the imported audio via --audio: cache
#       miss then hit). Needs the desktop session; skips with a note
#       otherwise.
#
# Hermetic by design: every import lands in a throwaway --library-root /
# redirected XDG dir under $(mktemp -d) — the real
# ~/.local/share/hyprbeat library and ~/.cache are never touched. To import
# into your real library, run the printed command by hand after review.
#
# Env knobs:
#   BIN=/path/to/hyprbeat   use a prebuilt binary, skip build AND the 0b
#                           gates (for CI/sandbox runs where the gates have
#                           already run in the producing environment)
#   OSZ_SPECIMEN=path       enable step 8 (the real .osz)
#   HYPRBEAT_AUDIO_SPECIMEN=0  skip the live --audio decode inside step 8
#
# The dev machine has no python3 (same constraint as the phase-1/2 scripts),
# so the synthetic .osz files are COMMITTED binaries under dev/fixtures/
# (generated once by dev/gen_osz_fixtures.py where python3 exists; sha256
# pinned in step 1). Dependencies here: bash + coreutils + find + grep.
#
# Usage: dev/validate-phase3-import.sh
#        OSZ_SPECIMEN=/path/to/real.osz dev/validate-phase3-import.sh

set -euo pipefail

cd "$(dirname "$0")/.."

say()  { printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
note() { printf '\033[1;33mNOTE: %s\033[0m\n' "$*"; }

PASS=0
FAIL=0
SKIP=0
ok()   { printf '  \033[1;32mPASS\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '  \033[1;31mFAIL\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }
skip() { printf '  \033[1;33mSKIP\033[0m %s\n' "$*"; SKIP=$((SKIP+1)); }

WORK=$(mktemp -d)
LOGDIR="$WORK/logs"
mkdir -p "$LOGDIR"

cleanup() {
  if (( FAIL == 0 )); then
    rm -rf "$WORK"
  else
    printf '\nlogs + scratch roots kept for diagnosis: %s\n' "$WORK" >&2
  fi
}
trap cleanup EXIT

# D-38: strip ANSI escapes (and CR) from a log in place before grepping.
sanitize() {
  sed -i -e $'s/\x1b\\[[0-9;]*[mK]//g' -e 's/\r$//' "$1" 2>/dev/null || true
}

# Print the FULL line when a field grep misses, so failures self-diagnose
# (D-38c).  grep_line <file> <pattern>
grep_line() {
  local line
  line=$(grep "$2" "$1" | head -1 || true)
  if test -n "$line"; then
    printf '%s\n' "$line"
  else
    printf '(no match for %s in %s)\n' "$2" "$1"
  fi
}

# ------------------------------------------------------------------ 0. build
if test -n "${BIN:-}" && test -x "$BIN"; then
  note "BIN set — using prebuilt binary, skipping build + 0b offline gates"
  note "(the producing environment must have run them; paste its output)"
else
  say "0. building inside the flake devshell (nix develop)"
  nix develop -c cargo build --release
  BIN=./target/release/hyprbeat
  test -x "$BIN"

  say "0b. offline gates: fmt --check, clippy -D warnings, cargo test"
  if nix develop -c cargo fmt --check >/dev/null 2>&1; then ok "cargo fmt --check"; else bad "cargo fmt --check"; fi
  if nix develop -c cargo clippy --all-targets -- -D warnings >/dev/null 2>&1; then
    ok "cargo clippy --all-targets -D warnings"
  else
    bad "cargo clippy --all-targets -D warnings"
  fi
  if nix develop -c cargo test --lib >/dev/null 2>&1; then ok "cargo test --lib"; else bad "cargo test --lib"; fi
fi

# --------------------------------------------------- 1. fixture byte-exactness
say "1. committed fixtures are byte-exact (sha256)"
FIXDIR=dev/fixtures
# sha256s from dev/gen_osz_fixtures.py output (deterministic: STORED,
# fixed mtime, fixed order)
printf '%s  %s\n' \
  4e29bbbdf358f9a2cfeab229e15d8cff49208070311d0b31680273e707f57255 "$FIXDIR/submitted.osz" \
  f6f56d3637b5f11d9d0bf248dc1496ab0dc4eab9dfc66dad761cfa70072542df "$FIXDIR/unsubmitted.osz" \
  9fd2ffe637c9b87c1ca3e930a7075c3eec1f2e53c3da52ea11ec780f75be0706 "$FIXDIR/oversize.osz" \
  | sha256sum -c - >/dev/null 2>&1 \
  && ok "submitted/unsubmitted/oversize fixtures match their pinned sha256s" \
  || bad "fixture sha256 mismatch — regenerate with dev/gen_osz_fixtures.py and re-pin"

# ------------------------------------------------- 2. submitted .osz import
say "2. submitted .osz: import into a throwaway library root"
ROOT="$WORK/lib-submitted"
"$BIN" --import "$FIXDIR/submitted.osz" --library-root "$ROOT" >"$LOGDIR/submitted1.log" 2>&1
RC=$?
sanitize "$LOGDIR/submitted1.log"
if test "$RC" -eq 0; then ok "exit 0 (skips are not failures, D-43/D-44)"; else bad "exit $RC — check $LOGDIR/submitted1.log"; fi

SUMMARY=$(grep 'osz import complete' "$LOGDIR/submitted1.log" | head -1 || true)
if printf '%s' "$SUMMARY" | grep -q 'set=20261115'; then
  ok "routed to set 20261115: $(printf '%s' "$SUMMARY" | grep -o 'set=[^ ]*')"
else
  bad "no set=20261115 in summary — $(grep_line "$LOGDIR/submitted1.log" 'osz import complete')"
fi
for field in stored=4 deduped=0 skipped=5 index_saved=true; do
  if printf '%s' "$SUMMARY" | grep -q "$field"; then
    ok "summary field $field"
  else
    bad "summary missing $field — $(grep_line "$LOGDIR/submitted1.log" 'osz import complete')"
  fi
done

SETDIR="$ROOT/songs/20261115"
MISSING=0
for f in \
  "Fixture Artist - Fixture Title (hyprbeat) [Easy].osu" \
  "Fixture Artist - Fixture Title (hyprbeat) [Normal].osu" \
  "audio.wav" "bg.jpg"
do
  test -s "$SETDIR/$f" || { bad "missing on disk: $SETDIR/$f"; MISSING=1; }
done
if test "$MISSING" -eq 0; then ok "songs/20261115/ holds all 4 kept files by exact name"; fi
if test -s "$ROOT/index.json" && grep -q '"version"' "$ROOT/index.json" && grep -q '"sets"' "$ROOT/index.json"; then
  ok "index.json written (schema v2: version + per-set sets manifest, D-13/D-45)"
else
  bad "index.json missing, or lacks the version/sets fields (D-13 v2, D-45)"
fi

# ------------------------------------------- 3. re-import: dedup, index frozen
say "3. re-import the same .osz: pure D-41 dedup, index byte-identical"
cp "$ROOT/index.json" "$LOGDIR/index.before"
"$BIN" --import "$FIXDIR/submitted.osz" --library-root "$ROOT" >"$LOGDIR/submitted2.log" 2>&1
sanitize "$LOGDIR/submitted2.log"
SUMMARY2=$(grep 'osz import complete' "$LOGDIR/submitted2.log" | head -1 || true)
for field in stored=0 deduped=4 index_saved=false; do
  if printf '%s' "$SUMMARY2" | grep -q "$field"; then
    ok "re-import summary field $field"
  else
    bad "re-import missing $field — $(grep_line "$LOGDIR/submitted2.log" 'osz import complete')"
  fi
done
if cmp -s "$LOGDIR/index.before" "$ROOT/index.json"; then
  ok "index.json byte-identical after the dedup re-import (saved only when mutated)"
else
  bad "index.json changed on a pure-dedup re-import"
fi

# ------------------------------------------------------- 4. skip reporting
say "4. skips are reported, never silent (D-43)"
SKIPS=$(grep -c 'osz import: skipped entry' "$LOGDIR/submitted1.log" || true)
if test "$SKIPS" -eq 5; then
  ok "5 skip lines (video/.osb/skin/.txt off-allowlist + 1 directory entry)"
else
  bad "expected 5 skip lines, got ${SKIPS} — check $LOGDIR/submitted1.log"
fi
if grep -q 'name=video.mp4' "$LOGDIR/submitted1.log" && grep -q 'name=subdir/' "$LOGDIR/submitted1.log"; then
  ok "skip lines name the entry (video.mp4, subdir/) and its reason"
else
  bad "skip lines missing entry names — check $LOGDIR/submitted1.log"
fi

# ------------------------------------------------ 5. unsubmitted routing (D-43)
say "5. unsubmitted .osz: songs/unsubmitted/<hash8>/"
"$BIN" --import "$FIXDIR/unsubmitted.osz" --library-root "$ROOT" >"$LOGDIR/unsubmitted.log" 2>&1
sanitize "$LOGDIR/unsubmitted.log"
if grep -qE 'set="?unsubmitted/[0-9a-f]{8}"?' "$LOGDIR/unsubmitted.log"; then
  ok "routed under unsubmitted/<8-hex>: $(grep -o 'set=unsubmitted/[0-9a-f]*' "$LOGDIR/unsubmitted.log" | head -1)"
else
  bad "no unsubmitted/<hash8> routing — $(grep_line "$LOGDIR/unsubmitted.log" 'osz import complete')"
fi
UOSU=$(find "$ROOT/songs/unsubmitted" -name '*.osu' -type f 2>/dev/null | head -1 || true)
UAUDIO=$(find "$ROOT/songs/unsubmitted" -name 'audio.wav' -type f 2>/dev/null | head -1 || true)
if test -n "$UOSU" && test -n "$UAUDIO"; then
  ok "hash8 dir holds the .osu + audio verbatim"
else
  bad "unsubmitted dir incomplete (.osu=${UOSU:-none} audio=${UAUDIO:-none})"
fi
"$BIN" --import "$FIXDIR/unsubmitted.osz" --library-root "$ROOT" >"$LOGDIR/unsubmitted2.log" 2>&1
sanitize "$LOGDIR/unsubmitted2.log"
if grep -q 'deduped=2' "$LOGDIR/unsubmitted2.log"; then
  ok "re-import dedups into the SAME hash8 dir (deterministic bundle hash, D-43)"
else
  bad "unsubmitted re-import did not dedup — $(grep_line "$LOGDIR/unsubmitted2.log" 'osz import complete')"
fi

# --------------------------------------- 6. hostile archive: fail-before-write
say "6. hostile .osz (declared 600 MiB, cap 512 MiB): refuse, write nothing"
HOSTILE="$WORK/lib-hostile"
RC=0
"$BIN" --import "$FIXDIR/oversize.osz" --library-root "$HOSTILE" >"$LOGDIR/oversize.log" 2>&1 || RC=$?
if test "$RC" -ne 0; then ok "exit $RC (refused)"; else bad "oversize import returned 0"; fi
if grep -q 'huge.wav.*carries 629145600 bytes' "$LOGDIR/oversize.log"; then
  ok "error names the entry + declared size (cap 536870912)"
else
  bad "error line not found — $(grep_line "$LOGDIR/oversize.log" 'carries')"
fi
if test ! -e "$HOSTILE"; then
  ok "library root not even created (fail-before-write, D-44)"
else
  bad "hostile import left bytes behind: $(find "$HOSTILE" -type f | head -5)"
fi

# -------------------------------------------- 7. XDG default root (no override)
say "7. no --library-root: XDG_DATA_HOME decides (I-07)"
XDGROOT="$WORK/xdg"
XDG_DATA_HOME="$XDGROOT" "$BIN" --import "$FIXDIR/unsubmitted.osz" >"$LOGDIR/xdg.log" 2>&1
sanitize "$LOGDIR/xdg.log"
if test -s "$XDGROOT/hyprbeat/index.json" && test -d "$XDGROOT/hyprbeat/songs/unsubmitted"; then
  ok "landed in \$XDG_DATA_HOME/hyprbeat/{songs,index.json} (I-07)"
else
  bad "XDG default root wrong — check $LOGDIR/xdg.log and $XDGROOT"
fi

# --------------------------------------------------- 8. real specimen + Q-10
say "8. REAL user-supplied .osz specimen (kept outside the repo, I-03)"
if test -n "${OSZ_SPECIMEN:-}" && test -r "$OSZ_SPECIMEN"; then
  SPECROOT="$WORK/lib-specimen"
  "$BIN" --import "$OSZ_SPECIMEN" --library-root "$SPECROOT" >"$LOGDIR/specimen1.log" 2>&1
  RC=$?
  sanitize "$LOGDIR/specimen1.log"
  if test "$RC" -eq 0; then
    ok "real specimen imported (exit 0)"
  else
    bad "real specimen import failed — $(grep_line "$LOGDIR/specimen1.log" 'error')"
  fi
  echo "    summary (record in §10 row 3 / §14 Q-10):"
  grep 'osz import complete' "$LOGDIR/specimen1.log" | head -1 | sed 's/^/      /'
  SPECSET=$(grep -o 'set=[^ ]*' "$LOGDIR/specimen1.log" | head -1 | cut -d= -f2 || true)
  NOSU=$(find "$SPECROOT/songs" -name '*.osu' -type f 2>/dev/null | wc -l)
  if test "$NOSU" -ge 1; then
    ok "$NOSU .osu files extracted under songs/$SPECSET"
  else
    bad "no .osu extracted (set=${SPECSET:-none})"
  fi
  "$BIN" --import "$OSZ_SPECIMEN" --library-root "$SPECROOT" >"$LOGDIR/specimen2.log" 2>&1
  sanitize "$LOGDIR/specimen2.log"
  if grep -q 'stored=0' "$LOGDIR/specimen2.log"; then
    ok "re-import of the real bytes is pure dedup (D-41 on real data)"
  else
    bad "real re-import stored bytes — $(grep_line "$LOGDIR/specimen2.log" 'osz import complete')"
  fi

  # Q-10 spot check: decode the imported audio through the live engine —
  # needs the desktop session (compositor + audio device); hermetic cache.
  SPECAUDIO=$(find "$SPECROOT/songs" -type f \( -name '*.mp3' -o -name '*.ogg' -o -name '*.flac' -o -name '*.wav' \) 2>/dev/null | head -1 || true)
  if test -n "$SPECAUDIO" && test -n "${WAYLAND_DISPLAY:-}" && test "${HYPRBEAT_AUDIO_SPECIMEN:-1}" = 1; then
    say "8b. Q-10: real-audio decode through --audio (cache miss then hit)"
    export XDG_CACHE_HOME="$WORK/cache"
    rm -rf "$XDG_CACHE_HOME"
    "$BIN" --audio "$SPECAUDIO" --timeout 10 >"$LOGDIR/specaudio1.log" 2>&1 || true
    sanitize "$LOGDIR/specaudio1.log"
    if grep -qE 'cache="?miss"?' "$LOGDIR/specaudio1.log"; then
      ok "first play decodes (cache miss) — $(basename "$SPECAUDIO")"
    else
      bad "expected cache=miss — $(grep_line "$LOGDIR/specaudio1.log" 'cache')"
    fi
    "$BIN" --audio "$SPECAUDIO" --timeout 6 >"$LOGDIR/specaudio2.log" 2>&1 || true
    sanitize "$LOGDIR/specaudio2.log"
    if grep -qE 'cache="?hit"?' "$LOGDIR/specaudio2.log"; then
      ok "second play served from the decode cache (Q-10)"
    else
      bad "expected cache=hit — $(grep_line "$LOGDIR/specaudio2.log" 'cache')"
    fi
    note "listen: did the map's own audio play for ~10 s? (record y/N in §14 Q-10)"
  else
    skip "Q-10 --audio decode (needs WAYLAND_DISPLAY + an audio device; audio=${SPECAUDIO:-none-found})"
  fi
  note "hermetic run — your real library is untouched. To import for real:"
  note "  $BIN --import $OSZ_SPECIMEN"
else
  skip "real-specimen section — set OSZ_SPECIMEN=/path/to/real.osz (kept outside the repo)"
fi

# ------------------------------------------------------------------ summary
say "summary — paste into CONTEXT.md §10 row 3 / §14 (Q-10)"
printf 'synthetic import suite  : see PASS/FAIL above\n'
printf 'real specimen           : %s\n' "${OSZ_SPECIMEN:-<not provided — SKIP>}"
printf 'result: %s passed, %s failed, %s skipped\n' "$PASS" "$FAIL" "$SKIP"
if (( FAIL > 0 )); then exit 1; fi
exit 0
