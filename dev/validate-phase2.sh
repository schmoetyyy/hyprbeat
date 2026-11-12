#!/usr/bin/env bash
# hyprbeat phase-2 validation — run this ON THE DEV MACHINE (NixOS + Hyprland).
#
# It walks the phase-2 exit criteria from CONTEXT.md §10 row 2:
#   [0] build inside the flake devshell + offline gates (fmt/clippy/test)
#   [1] --headless PNG (the explicit wall-clock path — D-16)
#   [2] wallpaper animation SYNCED TO MUSIC (--audio + the committed test tone)
#       + decode cache: miss on first run, hit on the second (§5.2)
#   [3] pause/rebase correct (SIGUSR1/2 while playing, §3.3/D-33) — scripted,
#       no human timing needed
#   [4] degraded wall-clock path (--no-audio: amber band + wall animation)
#   [5] --latency-test: input → sound → photon report (§3.6 budget ≤ 25 ms)
#   [6] optional §3.6 budget spot checks
#
# Dependencies: bash + coreutils + kill only (hyprctl optional). The dev
# machine has no python3 — same constraint as validate-phase1.sh.
#
# Usage: dev/validate-phase2.sh

set -euo pipefail

cd "$(dirname "$0")/.."

say()  { printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
ask()  { printf '\033[1;33m%s\033[0m' "$*"; read -r ans; }
note() { printf '\033[1;33mNOTE: %s\033[0m\n' "$*"; }

TONE=assets/test-tone.wav
LOGDIR=$(mktemp -d)
trap 'rm -rf "$LOGDIR"' EXIT

PASS=0
FAIL=0
ok()   { printf '  \033[1;32mPASS\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '  \033[1;31mFAIL\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }

# ------------------------------------------------------------------ 0. build
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

# ------------------------------------------------------------------ 1. headless
say "1. headless PNG (explicit wall-clock path, D-16 — no audio device involved)"
"$BIN" --headless --size 1920x1080 --scale 1.25 --out /tmp/hyprbeat-p2-125.png >/dev/null 2>&1
if test -s /tmp/hyprbeat-p2-125.png; then
  ok "headless PNG written ($(stat -c%s /tmp/hyprbeat-p2-125.png) bytes)"
else
  bad "headless PNG missing"
fi

# ------------------------------------------------- 2. live audio + cache
say "2. live music-synced wallpaper: first --audio run (decode cache MISS expected)"
"$BIN" --audio "$TONE" --timeout 10 >"$LOGDIR/audio1.log" 2>&1 || true

if grep -q 'music decoded' "$LOGDIR/audio1.log" && grep -q 'cache=miss' "$LOGDIR/audio1.log"; then
  ok "decode through cache, first run = miss"
else
  bad "expected cache=miss on the first run — check $LOGDIR/audio1.log"
fi
if grep -q 'audio stream started' "$LOGDIR/audio1.log"; then
  ok "audio stream started: $(grep -o 'rate=[0-9]*' "$LOGDIR/audio1.log" | head -1) $(grep -o 'channels=[0-9]*' "$LOGDIR/audio1.log" | head -1) (record in §14 Q-8)"
else
  bad "no 'audio stream started' log — device/stream open failed"
fi
if grep -q 'audio clock' "$LOGDIR/audio1.log"; then
  ok "audio clock reporting (song_ms lines)"
else
  bad "no 'audio clock' stats lines"
fi

ask "watch the background during the next 10 s run: do you HEAR the tone and SEE the gradient pulse in time with the beats? [y/N] "
MUSIC_SYNC="${ans:-n}"

say "2b. second --audio run (decode cache HIT expected + shorter cold start)"
"$BIN" --audio "$TONE" --timeout 6 >"$LOGDIR/audio2.log" 2>&1 || true
if grep -q 'cache=hit' "$LOGDIR/audio2.log"; then
  ok "second run served from ~/.cache/hyprbeat/decoded (§5.2)"
else
  bad "expected cache=hit on the second run — check $LOGDIR/audio2.log"
fi

# ---------------------------------------------------- 3. pause/rebase (§3.3)
say "3. pause/rebase, scripted: play 2 s -> SIGUSR1 (pause 5 s) -> SIGUSR2 (resume 3 s) -> stop"
"$BIN" --audio "$TONE" --timeout 16 >"$LOGDIR/pause.log" 2>&1 &
HBPID=$!
sleep 2            # 2 s of playing
kill -USR1 "$HBPID"   # pause
sleep 5            # 5 s frozen (device counter keeps flowing, §3.3)
kill -USR2 "$HBPID"   # resume (rebase)
sleep 3            # 3 s more of song
kill -USR1 "$HBPID" 2>/dev/null || true   # final pause to pin the end state
wait "$HBPID" 2>/dev/null || true

PAUSED_LINE=$(grep -o 'SIGUSR1: paused.*' "$LOGDIR/pause.log" | head -1 || true)
RESUMED_LINE=$(grep -o 'SIGUSR2: resumed.*' "$LOGDIR/pause.log" | head -1 || true)
if test -n "$PAUSED_LINE"; then ok "pause logged: $PAUSED_LINE"; else bad "no SIGUSR1 pause log"; fi
if test -n "$RESUMED_LINE"; then ok "resume logged (rebase)"; else bad "no SIGUSR2 resume log"; fi

# song_ms values from the periodic 'audio clock' stat lines ONLY (reported
# every 5 s of loop time: line 1 lands while paused, line 2 3 s after the
# resume, line 3 while paused again; the SIGUSR lines carry song_ms too and
# must not pollute this array)
mapfile -t SONGMS < <(grep 'audio clock' "$LOGDIR/pause.log" | grep -o 'song_ms=[0-9-]*' | cut -d= -f2)
PAUSE_VAL=$(grep 'SIGUSR1: paused' "$LOGDIR/pause.log" | head -1 | grep -o 'song_ms=[0-9]*' | cut -d= -f2 || true)

absdiff() { local d=$(( $1 - $2 )); d=${d#-}; echo "$d"; }

if test "${#SONGMS[@]}" -ge 3 && test -n "$PAUSE_VAL"; then
  # while paused: the reported song time sits at the pause point
  if (( $(absdiff "${SONGMS[0]}" "$PAUSE_VAL") <= 60 )); then
    ok "song time frozen while paused (report ${SONGMS[0]} vs pause point ${PAUSE_VAL}, ±60 ms)"
  else
    bad "song_ms moved during pause: report ${SONGMS[0]} vs pause point ${PAUSE_VAL}"
  fi
  # after resume (5 s pause + 3 s play): continues from the pause point,
  # NOT from the 10 s of wall time that actually elapsed
  EXPECT=$(( PAUSE_VAL + 3000 ))
  if (( $(absdiff "${SONGMS[1]}" "$EXPECT") <= 400 )); then
    ok "resume continued exactly from the pause point (report ${SONGMS[1]}, expected ≈${EXPECT})"
  else
    bad "resume jumped: report ${SONGMS[1]}, expected ≈${EXPECT} (pause ${PAUSE_VAL} + 3 s)"
  fi
  # the second pause pins the same continuation point
  EXPECT2=$(( PAUSE_VAL + 3000 ))
  if (( $(absdiff "${SONGMS[2]}" "$EXPECT2") <= 400 )); then
    ok "final pause froze at the continued point (report ${SONGMS[2]})"
  else
    bad "final pause report ${SONGMS[2]} vs expected ≈${EXPECT2}"
  fi
else
  bad "not enough 'audio clock' lines to verify freeze/continue — check $LOGDIR/pause.log"
fi

# ------------------------------------------------------- 4. degraded path
say "4. degraded wall-clock path (--no-audio, D-16): 8 s"
"$BIN" --no-audio --timeout 8 >"$LOGDIR/degraded.log" 2>&1 || true
if grep -q 'forced degraded wall-clock path' "$LOGDIR/degraded.log"; then
  ok "degraded mode entered (D-16)"
else
  bad "no degraded-mode log line"
fi
ask "during that run: did the gradient KEEP ANIMATING (wall clock) with a pulsing AMBER BAND along the top edge? [y/N] "
DEGRADED_VIS="${ans:-n}"

# ------------------------------------------------------- 5. latency test
say "5. input -> sound -> photon latency test (§3.6 budget: p95 audible ≤ 25 ms)"
echo "    FIRST switch to an EMPTY workspace (D-28), then:"
echo "    click the desktop once (keyboard focus), and TAP keys ~10 times."
"$BIN" --latency-test --timeout 30 >"$LOGDIR/latency.log" 2>&1 || true
LAT_REPORT=$(sed -n '/=== hyprbeat latency report/,/note: no microphone/p' "$LOGDIR/latency.log" || true)
if test -n "$LAT_REPORT"; then
  printf '%s\n' "$LAT_REPORT"
  if printf '%s' "$LAT_REPORT" | grep -q 'budget: input→sound ≤ 25 ms  : PASS'; then
    ok "latency budget PASS (record p50/p95 in §10 row 2)"
  else
    bad "latency budget FAIL — record the numbers + a waiver in §14, or tune the buffer"
  fi
else
  bad "no latency report printed — check $LOGDIR/latency.log"
fi

# ------------------------------------------------------------------ summary
say "summary — paste into CONTEXT.md §10 row 2 / §14"
cat <<EOF
music-synced animation (user)   : $MUSIC_SYNC
decode cache (miss then hit)    : see PASS/FAIL above
pause freeze + resume rebase    : see PASS/FAIL above
degraded path visible (user)    : $DEGRADED_VIS
latency report                  : see above
logs kept until script exit     : $LOGDIR
EOF

cat <<'EOF'

Optional §3.6 budget spot checks (not automated):
  cold start (cache warm): time ./target/release/hyprbeat --audio assets/test-tone.wav --timeout 3
                            → first frame ≤ 2 s (decode cache hit path)
  RAM with one map loaded : ps -o rss= -C hyprbeat   (≤ 150 MB; f32 of a 12 s
                            mono tone ≈ 2.3 MB, real songs ≈ 50 MB)
  visible CPU             : top during the music run (≤ 2% of one core)
  RT callback cost        : cargo bench --bench audio  (mix 512-frame callback ≈ 3 µs)
EOF

printf '\n%s\n' "result: $PASS passed, $FAIL failed"
if (( FAIL > 0 )); then exit 1; fi
exit 0
