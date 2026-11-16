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
# 2026-11-13 fixes after validation run 1 (9 pass / 4 fail):
#   - run 1's failures were two script bugs + one real defect:
#     · cache greps missed because tracing prints string fields QUOTED
#       (`cache="miss"`) — greps are now quote-tolerant (D-38);
#     · field greps (rate=/song_ms=) missed on this machine's redirected
#       logs — logs are now ANSI-stripped in place before any grep, and key
#       checks print the FULL matched line so failures self-diagnose;
#     · the REAL defect: the device default buffer is 32 ms (1536 frames @
#       48 kHz) so the §3.6 input→sound budget (p95 audible 62.58 ms) could
#       never pass — the engine now requests a 256-frame fixed buffer (D-37,
#       `--buffer`), with device-default fallback.
#   - the decode cache is wiped at start so the miss→hit sequence is
#     deterministic even when re-running the script;
#   - the pause check is marker-based (paused=true/false stat lines) instead
#     of position-based, and the run is 26 s so several stat lines exist;
#   - logs are KEPT (path printed) when anything fails, for diagnosis.
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

PASS=0
FAIL=0
ok()   { printf '  \033[1;32mPASS\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '  \033[1;31mFAIL\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }

# Keep logs when anything failed (printed at exit); remove them when clean.
cleanup() {
  if (( FAIL == 0 )); then
    rm -rf "$LOGDIR"
  else
    printf '\nlogs kept for diagnosis: %s\n' "$LOGDIR" >&2
  fi
}
trap cleanup EXIT

# D-38: strip ANSI escapes (and CR) from a log in place before grepping —
# log greps must survive colored output regardless of how the subscriber
# decided to colorize on a given machine.
sanitize() {
  sed -i -e $'s/\x1b\\[[0-9;]*[mK]//g' -e 's/\r$//' "$1" 2>/dev/null || true
}

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
# Wipe OUR OWN decode cache (regenerable, I-07) so the miss→hit sequence below
# is deterministic no matter how often this script was run before.
rm -rf "${XDG_CACHE_HOME:-$HOME/.cache}/hyprbeat/decoded"
"$BIN" --audio "$TONE" --timeout 10 >"$LOGDIR/audio1.log" 2>&1 || true
sanitize "$LOGDIR/audio1.log"

# quote-tolerant: tracing may print cache=miss or cache="miss" (D-38)
if grep -qE 'cache="?miss"?' "$LOGDIR/audio1.log"; then
  ok "decode through cache, first run = miss"
else
  bad "expected cache=miss on the first run — check $LOGDIR/audio1.log"
fi

STREAM_LINE=$(grep 'audio stream started' "$LOGDIR/audio1.log" | head -1 || true)
RATEV=$(printf '%s\n' "$STREAM_LINE" | grep -o 'rate=[0-9]*' | head -1 || true)
CHANV=$(printf '%s\n' "$STREAM_LINE" | grep -o 'channels=[0-9]*' | head -1 || true)
if test -n "$RATEV" && test -n "$CHANV"; then
  ok "audio stream started ($RATEV $CHANV — record in §14 Q-8)"
else
  # self-diagnosing: print the whole line when the fields did not extract
  ok "audio stream started: $STREAM_LINE"
fi
if grep -q 'audio clock' "$LOGDIR/audio1.log"; then
  ok "audio clock reporting (song_ms lines)"
else
  bad "no 'audio clock' stats lines"
fi
BUFREQ=$(grep -o 'buffer=[^ ]*' "$LOGDIR/audio1.log" | head -1 || true)
note "buffer request: ${BUFREQ:-<not logged>} (D-37 amended: default 512 ≈ 10.7 ms @ 48 kHz — was 32 ms device default)"

ask "watch the background during the next 10 s run: do you HEAR the tone and SEE the gradient pulse in time with the beats? [y/N] "
MUSIC_SYNC="${ans:-n}"

say "2b. second --audio run (decode cache HIT expected + shorter cold start)"
"$BIN" --audio "$TONE" --timeout 6 >"$LOGDIR/audio2.log" 2>&1 || true
sanitize "$LOGDIR/audio2.log"
if grep -qE 'cache="?hit"?' "$LOGDIR/audio2.log"; then
  ok "second run served from ~/.cache/hyprbeat/decoded (§5.2)"
else
  bad "expected cache=hit on the second run — check $LOGDIR/audio2.log"
fi

# ---------------------------------------------------- 3. pause/rebase (§3.3)
say "3. pause/rebase, scripted: play 2 s -> SIGUSR1 (pause 5 s) -> SIGUSR2 (resume 3 s) -> pause -> stop at 26 s"
"$BIN" --audio "$TONE" --timeout 26 >"$LOGDIR/pause.log" 2>&1 &
HBPID=$!
sleep 2            # 2 s of playing
kill -USR1 "$HBPID"   # pause
sleep 5            # 5 s frozen (device counter keeps flowing, §3.3)
kill -USR2 "$HBPID"   # resume (rebase)
sleep 3            # 3 s more of song
kill -USR1 "$HBPID" 2>/dev/null || true   # final pause to pin the end state
wait "$HBPID" 2>/dev/null || true
sanitize "$LOGDIR/pause.log"

PAUSED_LINE=$(grep -o 'SIGUSR1: paused.*' "$LOGDIR/pause.log" | head -1 || true)
RESUMED_LINE=$(grep -o 'SIGUSR2: resumed.*' "$LOGDIR/pause.log" | head -1 || true)
if test -n "$PAUSED_LINE"; then ok "pause logged: $PAUSED_LINE"; else bad "no SIGUSR1 pause log"; fi
if test -n "$RESUMED_LINE"; then ok "resume logged (rebase)"; else bad "no SIGUSR2 resume log"; fi

# P1 = song time at the first pause (int part; the warn line prints 2020.0)
P1=$(grep 'SIGUSR1: paused' "$LOGDIR/pause.log" | head -1 | grep -o 'song_ms=[0-9]*' | head -1 | cut -d= -f2 || true)

# marker-based: song_ms values from the periodic 'audio clock' stat lines,
# split by the paused=true / paused=false markers those lines carry.
mapfile -t PSONG < <(grep 'audio clock' "$LOGDIR/pause.log" | grep 'paused=true' | grep -o 'song_ms=[0-9-]*' | cut -d= -f2)
mapfile -t LSONG < <(grep 'audio clock' "$LOGDIR/pause.log" | grep 'paused=false' | grep -o 'song_ms=[0-9-]*' | cut -d= -f2)

absdiff() { local d=$(( $1 - $2 )); d=${d#-}; echo "$d"; }

if test -n "$P1" && test "${#PSONG[@]}" -ge 2 && test "${#LSONG[@]}" -ge 1; then
  # (a) while paused: the reported song time sits at the pause point
  FROZEN_OK=0
  for v in "${PSONG[@]}"; do
    if (( $(absdiff "$v" "$P1") <= 60 )); then FROZEN_OK=1; fi
  done
  if (( FROZEN_OK )); then
    ok "song time frozen while paused (pause point ${P1} ms)"
  else
    bad "song_ms moved during pause: ${PSONG[*]} vs pause point ${P1}"
  fi
  # (b) after resume + 3 s of play, the final pause froze the CONTINUATION
  # point: pause point + 3000 ms (±400 for signal/tick skew), not +13 s of
  # wall time — that is the §3.3 rebase proving itself.
  LASTP="${PSONG[-1]}"
  EXPECT=$(( P1 + 3000 ))
  if (( $(absdiff "$LASTP" "$EXPECT") <= 400 )); then
    ok "resume continued exactly from the pause point (final freeze ${LASTP}, expected ≈${EXPECT})"
  else
    bad "resume jumped: final freeze ${LASTP}, expected ≈${EXPECT} (pause ${P1} + 3 s)"
  fi
else
  bad "not enough marker lines (pause-point=${P1:-none} paused-lines=${#PSONG[@]} playing-lines=${#LSONG[@]}) — check $LOGDIR/pause.log"
fi

# ------------------------------------------------------- 4. degraded path
say "4. degraded wall-clock path (--no-audio, D-16): 8 s"
"$BIN" --no-audio --timeout 8 >"$LOGDIR/degraded.log" 2>&1 || true
sanitize "$LOGDIR/degraded.log"
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
sanitize "$LOGDIR/latency.log"
LAT_REPORT=$(sed -n '/=== hyprbeat latency report/,/note: no microphone/p' "$LOGDIR/latency.log" || true)
if test -n "$LAT_REPORT"; then
  printf '%s\n' "$LAT_REPORT"
  if printf '%s' "$LAT_REPORT" | grep -q 'budget: input→sound ≤ 25 ms  : PASS'; then
    ok "latency budget PASS (record p50/p95 in §10 row 2)"
  else
    bad "latency budget FAIL"
    cat <<'EOF'
    tuning (D-37): re-run with a smaller/larger fixed buffer and compare
    p95 "audible" numbers, e.g.:
      nix develop -c cargo run --release -- --latency-test --buffer 128 --timeout 30
      nix develop -c cargo run --release -- --latency-test --buffer 512 --timeout 30
      nix develop -c cargo run --release -- --latency-test --buffer 0   --timeout 30
    (0 = the device default — the 2026-11-13 baseline: 32 ms period,
     p95 audible 62.58 ms.) If no fixed size fits the budget, record the
    best numbers + a dated waiver in §14 per §3.6.
EOF
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
  RT callback cost        : nix develop -c cargo bench --bench audio
                            NOTE: criterion takes ~30–60 s for the four benches
                            (3 s warmup + 5 s measurement each, after ~30 s of
                            compiling) — it is NOT stuck; the summary tables
                            print at the end.
EOF

printf '\n%s\n' "result: $PASS passed, $FAIL failed"
if (( FAIL > 0 )); then exit 1; fi
exit 0
