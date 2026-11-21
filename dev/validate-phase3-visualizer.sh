#!/usr/bin/env bash
# hyprbeat phase-3 visualizer validation — run this ON THE DEV MACHINE
# (NixOS + Hyprland), for the visualizer commits (§10 row 3): commit a
# `d3abcd4` (timeline load, log, exit), commit b1 `b68652e` (instanced quad
# pipeline + timeline scene) and commit b2 `e7bf95b` (live + headless
# wiring). This is the phase-3 EXIT VALIDATION script — its OSZ_SPECIMEN
# section also closes the two forward-carried Q-10 halves (§14 Q-10 /
# phase-2 close-out): cold start cache-warm ≤ 2 s and full-budget RSS
# ≤ 150 MB.
#
#   [0] build inside the flake devshell + offline gates (fmt/clippy/test)
#   [1] committed fixtures byte-exact (timeline.osz Δ-J + submitted.osz)
#   [2] golden --map: the loader summary line + a non-trivial headless PNG
#   [3] scratch library: import timeline.osz (set 20261119) + submitted.osz
#       (set 20261115 — the stub set)
#   [4] --set 20261119 diff selection: no --diff (first difficulty `Normal`
#       + a warning listing all), --diff Hard (exact, NO warning, Hard's own
#       summary), --diff Extra (miss → warning listing all, first used)
#   [5] failure modes + the b2 conflict matrix: unknown --set fails listing
#       the available sets; the stub set fails with "has no playable .osu";
#       --diff with --map refused; --map with --probe refused
#   [6] headless PNG of the timeline fixture: exists, PNG signature,
#       non-trivial size
#   [7] OSZ_SPECIMEN=/path/to/real.osz — REAL user set (kept OUTSIDE the
#       repo, I-03): import into the scratch library, then a 30 s live run
#       (y/N: timeline visible, playhead synced, no amber band) with the
#       Q-10 halves measured around it: cold start cache-warm ≤ 2 s (the
#       phase-2 warm-up method: wall time of a --timeout 3 run minus 3 s)
#       and full-budget RSS ≤ 150 MB (VmRSS/VmHWM sampled via /proc during
#       the live run). Needs the desktop session; skips with a note
#       otherwise.
#
# Hermetic by design: every import lands in a throwaway --library-root and
# the Q-10 runs redirect XDG_CACHE_HOME, all under $(mktemp -d) — the real
# ~/.local/share/hyprbeat library and ~/.cache are never touched.
#
# Env knobs:
#   BIN=/path/to/hyprbeat   use a prebuilt binary, skip build AND the 0b
#                           gates (for CI/sandbox runs where the gates have
#                           already run in the producing environment)
#   OSZ_SPECIMEN=path       enable step 7 (the real .osz)
#   HYPRBEAT_VIS_SPECIMEN_LIVE=0  skip the live y/N + Q-10 measurements
#                           inside step 7 (the import assertions still run)
#
# The dev machine has no python3 (same constraint as the other phase
# scripts), so timeline.osz is a COMMITTED binary (generated once by
# dev/gen_osz_fixtures.py where python3 exists; sha256 pinned in step 1 —
# every byte invented, I-03). Dependencies: bash + coreutils + grep +
# head/stat/od (no find needed).
#
# Usage: dev/validate-phase3-visualizer.sh
#        OSZ_SPECIMEN=/path/to/real.osz dev/validate-phase3-visualizer.sh

set -euo pipefail

cd "$(dirname "$0")/.."

say()  { printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
note() { printf '\033[1;33mNOTE: %s\033[0m\n' "$*"; }
ask()  { printf '\033[1;33m%s\033[0m' "$*"; read -r ans || ans=""; }

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

# The headless render must be a real PNG with real content (signature +
# size floor), not an empty or placeholder file.
# png_nontrivial <file> <label>
png_nontrivial() {
  local magic size
  if test ! -s "$1"; then
    bad "$2: PNG missing or empty ($1)"
    return
  fi
  magic=$(head -c 8 "$1" | od -An -tx1 | tr -d ' \n')
  if test "$magic" != "89504e470d0a1a0a"; then
    bad "$2: not a PNG (magic $magic)"
    return
  fi
  size=$(stat -c%s "$1")
  if test "$size" -ge 4096; then
    ok "$2: PNG signature ok, $size bytes (non-trivial)"
  else
    bad "$2: only $size bytes — trivial"
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
# fixed mtime, fixed order). timeline.osz = Δ-J; submitted.osz re-pins the
# same hash dev/validate-phase3-import.sh step 1 pins (this script imports
# it too — the stub set of step 5).
printf '%s  %s\n' \
  76b0ba9b7f9ec79639ff2659fd74bbb616d450a224626d644d226af0f13987d1 "$FIXDIR/timeline.osz" \
  4e29bbbdf358f9a2cfeab229e15d8cff49208070311d0b31680273e707f57255 "$FIXDIR/submitted.osz" \
  | sha256sum -c - >/dev/null 2>&1 \
  && ok "timeline.osz (Δ-J) + submitted.osz match their pinned sha256s" \
  || bad "fixture sha256 mismatch — regenerate with dev/gen_osz_fixtures.py and re-pin"

# --------------------------------------------------------- 2. golden --map
say "2. golden --map: loader summary + headless PNG"
RC=0
"$BIN" --map assets/golden/v14-standard.osu --headless \
  --out "$WORK/golden.png" >"$LOGDIR/golden.log" 2>&1 || RC=$?
sanitize "$LOGDIR/golden.log"
if test "$RC" -eq 0; then ok "exit 0"; else bad "exit $RC — check $LOGDIR/golden.log"; fi
GOLDEN=$(grep 'timeline loaded' "$LOGDIR/golden.log" | head -1 || true)
for field in 'set=assets/golden/v14-standard.osu' 'diff=Standard' \
  'objects=10 circles=4 sliders=4 spinners=2 kiai=1' \
  'span_start_ms=0.0 span_end_ms=8500.0'; do
  if printf '%s' "$GOLDEN" | grep -q "$field"; then
    ok "golden summary: $field"
  else
    bad "golden summary missing '$field' — $(grep_line "$LOGDIR/golden.log" 'timeline loaded')"
  fi
done
# the golden corpus's audio is a placeholder: --map never resolves audio
if printf '%s' "$GOLDEN" | grep -qE 'audio="?none"?'; then
  ok "golden summary: audio=none (placeholder audio, expected)"
else
  bad "golden summary should say audio=none — $(grep_line "$LOGDIR/golden.log" 'timeline loaded')"
fi
png_nontrivial "$WORK/golden.png" "golden headless PNG"

# ------------------------------------------------ 3. scratch library imports
say "3. import timeline.osz + submitted.osz into a throwaway library root"
ROOT="$WORK/lib"
RC=0
"$BIN" --import "$FIXDIR/timeline.osz" --library-root "$ROOT" >"$LOGDIR/timeline-import.log" 2>&1 || RC=$?
sanitize "$LOGDIR/timeline-import.log"
if test "$RC" -eq 0; then ok "timeline.osz import exit 0"; else bad "timeline.osz import exit $RC — check $LOGDIR/timeline-import.log"; fi
TSUM=$(grep 'osz import complete' "$LOGDIR/timeline-import.log" | head -1 || true)
for field in 'set=20261119' 'stored=3' 'deduped=0' 'skipped=0' 'index_saved=true'; do
  if printf '%s' "$TSUM" | grep -q "$field"; then
    ok "timeline import summary: $field"
  else
    bad "timeline import missing $field — $(grep_line "$LOGDIR/timeline-import.log" 'osz import complete')"
  fi
done
RC=0
"$BIN" --import "$FIXDIR/submitted.osz" --library-root "$ROOT" >"$LOGDIR/submitted-import.log" 2>&1 || RC=$?
sanitize "$LOGDIR/submitted-import.log"
if test "$RC" -eq 0; then ok "submitted.osz import exit 0"; else bad "submitted.osz import exit $RC — check $LOGDIR/submitted-import.log"; fi
if grep -q 'set=20261115' "$LOGDIR/submitted-import.log"; then
  ok "submitted.osz routed to set 20261115 (the stub set for step 5)"
else
  bad "no set=20261115 — $(grep_line "$LOGDIR/submitted-import.log" 'osz import complete')"
fi

# ------------------------------------------- 4. --set 20261119 diff selection
say "4. --set 20261119: diff selection (ruling 2026-11-17)"
# (a) no --diff: the FIRST difficulty (manifest order: Normal — the entry
#     names carry the sort digit) + a warning listing ALL alternatives.
RC=0
"$BIN" --set 20261119 --library-root "$ROOT" --headless \
  --out "$WORK/timeline-normal.png" >"$LOGDIR/set-nodiff.log" 2>&1 || RC=$?
sanitize "$LOGDIR/set-nodiff.log"
if test "$RC" -eq 0; then ok "no-diff run exit 0"; else bad "no-diff run exit $RC — check $LOGDIR/set-nodiff.log"; fi
NSUM=$(grep 'timeline loaded' "$LOGDIR/set-nodiff.log" | head -1 || true)
if printf '%s' "$NSUM" | grep -q 'diff=Normal'; then
  ok "no --diff → first difficulty Normal"
else
  bad "no --diff should pick Normal — $(grep_line "$LOGDIR/set-nodiff.log" 'timeline loaded')"
fi
if grep -q 'no --diff given — using the first difficulty `Normal`' "$LOGDIR/set-nodiff.log" \
  && grep -q 'available: Normal, Hard' "$LOGDIR/set-nodiff.log"; then
  ok "warning lists the first difficulty + all alternatives (Normal, Hard)"
else
  bad "no-diff warning missing — $(grep_line "$LOGDIR/set-nodiff.log" 'no --diff given')"
fi
# the hand-computed corrected-§7.3 summary (Δ-J, dev/gen_osz_fixtures.py):
# slider 2000..2750 (375 ms/slide × 2), spinner 3000..4200,
# kiai 4000..8000 clamped to the span
for field in 'objects=4 circles=2 sliders=1 spinners=1 kiai=1' \
  'span_start_ms=0.0 span_end_ms=4200.0'; do
  if printf '%s' "$NSUM" | grep -q "$field"; then
    ok "Normal summary: $field"
  else
    bad "Normal summary missing '$field' — $(grep_line "$LOGDIR/set-nodiff.log" 'timeline loaded')"
  fi
done
# tone.wav is a manifest entry: the set form resolves the map's own audio
if printf '%s' "$NSUM" | grep -q 'audio=' && ! printf '%s' "$NSUM" | grep -qE 'audio="?none"?'; then
  ok "Normal summary: audio resolved from the manifest (tone.wav)"
else
  bad "Normal audio should resolve — $(grep_line "$LOGDIR/set-nodiff.log" 'timeline loaded')"
fi

# (b) --diff Hard: EXACT match — no warning, Hard's own summary
#     (slider 1500..1875, spinner 2500..3700, kiai 3000..8000 clamped).
RC=0
"$BIN" --set 20261119 --library-root "$ROOT" --diff Hard --headless \
  --out "$WORK/timeline-hard.png" >"$LOGDIR/set-hard.log" 2>&1 || RC=$?
sanitize "$LOGDIR/set-hard.log"
if test "$RC" -eq 0; then ok "--diff Hard run exit 0"; else bad "--diff Hard run exit $RC — check $LOGDIR/set-hard.log"; fi
HSUM=$(grep 'timeline loaded' "$LOGDIR/set-hard.log" | head -1 || true)
if printf '%s' "$HSUM" | grep -q 'diff=Hard'; then
  ok "--diff Hard → Hard (exact match, not the first difficulty)"
else
  bad "--diff Hard should pick Hard — $(grep_line "$LOGDIR/set-hard.log" 'timeline loaded')"
fi
if grep -qE 'no --diff given|matches no difficulty' "$LOGDIR/set-hard.log"; then
  bad "exact match must not warn — $(grep_line "$LOGDIR/set-hard.log" 'timeline load')"
else
  ok "--diff Hard: no selection warning (exact)"
fi
for field in 'objects=6 circles=4 sliders=1 spinners=1 kiai=1' \
  'span_start_ms=0.0 span_end_ms=3700.0'; do
  if printf '%s' "$HSUM" | grep -q "$field"; then
    ok "Hard summary: $field"
  else
    bad "Hard summary missing '$field' — $(grep_line "$LOGDIR/set-hard.log" 'timeline loaded')"
  fi
done

# (c) --diff Extra: no exact match → first difficulty + a warning listing
#     ALL alternatives.
RC=0
"$BIN" --set 20261119 --library-root "$ROOT" --diff Extra --headless \
  --out "$WORK/timeline-miss.png" >"$LOGDIR/set-miss.log" 2>&1 || RC=$?
sanitize "$LOGDIR/set-miss.log"
if test "$RC" -eq 0; then ok "--diff Extra run exit 0 (fallback is not an error)"; else bad "--diff Extra run exit $RC — check $LOGDIR/set-miss.log"; fi
if grep -q 'matches no difficulty exactly — using the first `Normal`' "$LOGDIR/set-miss.log" \
  && grep -q 'available: Normal, Hard' "$LOGDIR/set-miss.log"; then
  ok "--diff unknown: warning lists all alternatives, first (Normal) used"
else
  bad "--diff unknown warning missing — $(grep_line "$LOGDIR/set-miss.log" 'matches no difficulty')"
fi
if grep -q 'diff=Normal' "$LOGDIR/set-miss.log"; then
  ok "--diff unknown → fell back to Normal"
else
  bad "--diff unknown should fall back to Normal — $(grep_line "$LOGDIR/set-miss.log" 'timeline loaded')"
fi

# --------------------------------- 5. failure modes + the b2 conflict matrix
say "5. failure modes + the b2 conflict matrix (--map/--set refusals)"
# (a) unknown set: failure listing the AVAILABLE sets (both imported ones).
RC=0
"$BIN" --set 999999 --library-root "$ROOT" --headless \
  >"$LOGDIR/set-unknown.log" 2>&1 || RC=$?
sanitize "$LOGDIR/set-unknown.log"
if test "$RC" -ne 0; then ok "unknown --set refused (exit $RC)"; else bad "unknown --set returned 0"; fi
if grep -q 'available sets: 20261115, 20261119' "$LOGDIR/set-unknown.log"; then
  ok "unknown --set lists the available sets (20261115, 20261119)"
else
  bad "available-sets listing missing — $(grep_line "$LOGDIR/set-unknown.log" 'available sets')"
fi

# (b) stub set: submitted.osz's difficulties have no playable .osu (the parser skips them: AudioFilename missing).
RC=0
"$BIN" --set 20261115 --library-root "$ROOT" --headless \
  >"$LOGDIR/set-stub.log" 2>&1 || RC=$?
sanitize "$LOGDIR/set-stub.log"
if test "$RC" -ne 0; then ok "stub --set refused (exit $RC)"; else bad "stub --set returned 0"; fi
if grep -q 'has no playable .osu' "$LOGDIR/set-stub.log"; then
  ok "stub --set fails with \"has no playable .osu\""
else
  bad "stub failure text missing — $(grep_line "$LOGDIR/set-stub.log" 'no playable')"
fi

# (c) --diff with --map: one .osu is one difficulty — refused at parse.
RC=0
"$BIN" --map assets/golden/v14-standard.osu --diff Normal \
  >"$LOGDIR/map-diff.log" 2>&1 || RC=$?
sanitize "$LOGDIR/map-diff.log"
if test "$RC" -ne 0; then ok "--diff with --map refused (exit $RC)"; else bad "--diff with --map returned 0"; fi
if grep -q 'exactly one difficulty' "$LOGDIR/map-diff.log"; then
  ok "refusal names the reason (a --map .osu has exactly one difficulty)"
else
  bad "refusal text missing — $(grep_line "$LOGDIR/map-diff.log" 'exactly one')"
fi

# (d) --map with --probe: the b2 conflict matrix keeps the live
#     diagnostics refused alongside --map/--set.
RC=0
"$BIN" --map assets/golden/v14-standard.osu --probe \
  >"$LOGDIR/map-probe.log" 2>&1 || RC=$?
sanitize "$LOGDIR/map-probe.log"
if test "$RC" -ne 0; then ok "--map with --probe refused (exit $RC)"; else bad "--map with --probe returned 0"; fi
if grep -q 'cannot run with --map/--set' "$LOGDIR/map-probe.log"; then
  ok "refusal: --probe/--latency-test cannot run with --map/--set"
else
  bad "refusal text missing — $(grep_line "$LOGDIR/map-probe.log" 'cannot run')"
fi

# --------------------------------------------------- 6. headless PNG non-trivial
say "6. headless PNG of the timeline fixture is non-trivial"
png_nontrivial "$WORK/timeline-normal.png" "timeline (Normal) headless PNG"

# ------------------------------------------------- 7. real specimen + Q-10
say "7. REAL user-supplied .osz specimen (kept outside the repo, I-03)"
if test -n "${OSZ_SPECIMEN:-}" && test -r "$OSZ_SPECIMEN"; then
  SPECROOT="$WORK/lib-specimen"
  RC=0
  "$BIN" --import "$OSZ_SPECIMEN" --library-root "$SPECROOT" >"$LOGDIR/specimen-import.log" 2>&1 || RC=$?
  sanitize "$LOGDIR/specimen-import.log"
  if test "$RC" -eq 0; then
    ok "real specimen imported (exit 0)"
  else
    bad "real specimen import failed — $(grep_line "$LOGDIR/specimen-import.log" 'error')"
  fi
  SPECSET=$(grep -o 'set=[^ ]*' "$LOGDIR/specimen-import.log" | head -1 | cut -d= -f2 || true)
  if test -n "$SPECSET"; then
    ok "specimen set key: $SPECSET"
  else
    bad "no set= in the specimen import summary — $(grep_line "$LOGDIR/specimen-import.log" 'osz import complete')"
  fi

  # The live visual + Q-10 measurements need the desktop session
  # (compositor + audio device) and a resolved set key; the decode cache
  # is redirected so the cold-start warm-up is hermetic.
  if test -n "$SPECSET" && test -n "${WAYLAND_DISPLAY:-}" && test "${HYPRBEAT_VIS_SPECIMEN_LIVE:-1}" = 1; then
    export XDG_CACHE_HOME="$WORK/cache"
    rm -rf "$XDG_CACHE_HOME"

    # (a) 30 s live run: timeline visible, playhead synced to the music,
    #     no amber band. RSS is sampled via /proc while it plays (Q-10).
    note "watch the background during the next 30 s run: is the timeline"
    note "visible, the playhead parked for the map's lead-in and then"
    note "sweeping WITH the music, and is there NO amber band?"
    ask "timeline + synced playhead + no amber band? [y/N] "
    SPEC_LIVE_VIS="${ans:-n}"

    "$BIN" --set "$SPECSET" --library-root "$SPECROOT" --timeout 30 \
      >"$LOGDIR/specimen-live.log" 2>&1 &
    LPID=$!
    MAXRSS=0
    MAXHWM=0
    while kill -0 "$LPID" 2>/dev/null; do
      if test -r "/proc/$LPID/status"; then
        RSS=$(grep '^VmRSS:' "/proc/$LPID/status" 2>/dev/null | grep -o '[0-9][0-9]*' | head -1) || RSS=""
        HWM=$(grep '^VmHWM:' "/proc/$LPID/status" 2>/dev/null | grep -o '[0-9][0-9]*' | head -1) || HWM=""
        if test -n "$RSS" && test "$RSS" -gt "$MAXRSS" 2>/dev/null; then MAXRSS=$RSS; fi
        if test -n "$HWM" && test "$HWM" -gt "$MAXHWM" 2>/dev/null; then MAXHWM=$HWM; fi
      fi
      sleep 0.3
    done
    wait "$LPID" 2>/dev/null || true
    sanitize "$LOGDIR/specimen-live.log"

    SSUM=$(grep 'timeline loaded' "$LOGDIR/specimen-live.log" | head -1 || true)
    echo "    summary (record in §10 row 3):"
    printf '%s\n' "$SSUM" | sed 's/^/      /'
    if test -n "$SSUM"; then
      ok "specimen loader summary logged"
    else
      bad "no 'timeline loaded' line — check $LOGDIR/specimen-live.log"
    fi
    if printf '%s' "$SSUM" | grep -q 'audio=' && ! printf '%s' "$SSUM" | grep -qE 'audio="?none"?'; then
      ok "specimen audio resolved from the manifest"
    else
      bad "specimen audio did not resolve — no music to sync to"
    fi
    if grep -q 'amber degraded band is ON' "$LOGDIR/specimen-live.log"; then
      bad "run degraded (amber band ON) — $(grep_line "$LOGDIR/specimen-live.log" 'degrading')"
    elif grep -q 'audio clock' "$LOGDIR/specimen-live.log" \
      && grep 'audio clock' "$LOGDIR/specimen-live.log" | grep -q 'degraded=false'; then
      ok "audio clock clean all run (degraded=false, no amber band)"
    else
      bad "no clean 'audio clock degraded=false' lines — $(grep_line "$LOGDIR/specimen-live.log" 'audio clock')"
    fi

    # (b) Q-10 half 1 — full-budget RSS ≤ 150 MB (153600 kB), the /proc
    #     samples from the live run above (VmHWM = the peak = the budget).
    if test "$MAXHWM" -gt 0; then
      note "RSS samples: peak VmRSS=$(( MAXRSS )) kB, VmHWM=$(( MAXHWM )) kB ($(( MAXHWM / 1024 )) MiB)"
      if test "$MAXHWM" -le 153600; then
        ok "full-budget RSS $MAXHWM kB ≤ 150 MB (Q-10 half)"
      else
        bad "full-budget RSS $MAXHWM kB > 150 MB (Q-10 half)"
      fi
    else
      bad "no /proc RSS samples collected (run too short?)"
    fi

    # (c) Q-10 half 2 — cold start, cache-warm ≤ 2 s. Phase-2 warm-up
    #     method: the live run above warmed the (redirected) decode cache;
    #     time a --timeout 3 run and subtract the 3 s of runtime — the
    #     remainder is startup + teardown (2026-11-13 baseline: 3.066 s
    #     wall ≈ 66 ms).
    T0=$(date +%s%N)
    "$BIN" --set "$SPECSET" --library-root "$SPECROOT" --timeout 3 \
      >"$LOGDIR/specimen-cold.log" 2>&1 || true
    T1=$(date +%s%N)
    sanitize "$LOGDIR/specimen-cold.log"
    WALL_MS=$(( (T1 - T0) / 1000000 ))
    START_MS=$(( WALL_MS - 3000 ))
    note "cache-warm cold start: ${WALL_MS} ms wall for a 3 s run → ≈${START_MS} ms startup+teardown"
    # guard: a run that died early would make the subtraction negative and
    # pass vacuously — require the clean exit path before judging the time
    if ! grep -q 'hyprbeat stopped cleanly' "$LOGDIR/specimen-cold.log"; then
      bad "cold-start run did not complete cleanly — check $LOGDIR/specimen-cold.log"
    elif test "$START_MS" -le 2000; then
      ok "cold start cache-warm ≈${START_MS} ms ≤ 2 s (Q-10 half)"
    else
      bad "cold start cache-warm ≈${START_MS} ms > 2 s (Q-10 half)"
    fi
    if grep -qE 'cache="?hit"?' "$LOGDIR/specimen-cold.log"; then
      ok "the timed run was served from the decode cache (cache=hit)"
    else
      note "no cache=hit line in the timed run — check $LOGDIR/specimen-cold.log (decode may predate the first stat line)"
    fi
  else
    skip "live visual + Q-10 measurements (needs a resolved set key + WAYLAND_DISPLAY; or HYPRBEAT_VIS_SPECIMEN_LIVE=0)"
  fi
  note "hermetic run — your real library and ~/.cache are untouched."
else
  skip "real-specimen section — set OSZ_SPECIMEN=/path/to/real.osz (kept outside the repo)"
fi

# ------------------------------------------------------------------ summary
say "summary — paste into CONTEXT.md §10 row 3 / §14 (Q-10)"
printf 'synthetic visualizer suite: see PASS/FAIL above\n'
printf 'real specimen             : %s\n' "${OSZ_SPECIMEN:-<not provided — SKIP>}"
printf 'live visuals (user y/N)   : %s\n' "${SPEC_LIVE_VIS:-<not run>}"
printf 'result: %s passed, %s failed, %s skipped\n' "$PASS" "$FAIL" "$SKIP"
if (( FAIL > 0 )); then exit 1; fi
exit 0
