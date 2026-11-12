#!/usr/bin/env bash
# hyprbeat phase-1 validation — run this ON THE DEV MACHINE (NixOS + Hyprland).
#
# It walks the phase-1 exit criteria from CONTEXT.md §10:
#   [1] build inside the flake devshell
#   [2] --headless PNG works (no compositor involved)
#   [3] gradient visible on the Hyprland background (live surface)
#   [4] test card correct at 100% and at 125% fractional scale
#   [5] Q-1/Q-2 probe (§14): on_demand keyboard + relative pointer on a
#       background-layer surface
#
# Dependencies: bash + hyprctl + coreutils ONLY. (The 2026-11-12 first run
# died at step 3 — the dev machine is a minimal NixOS install with no python3
# in PATH — so hyprctl TEXT output is parsed in pure bash, no python3/jq.)
#
# The wallpaper takes over the background while a check runs; every live run
# is bounded by --timeout so a broken build cannot strand the desktop.
#
# Usage:  dev/validate-phase1.sh [monitor-name]
#         MONITOR=eDP-1 dev/validate-phase1.sh

set -euo pipefail

MONITOR="${1:-${MONITOR:-}}"
cd "$(dirname "$0")/.."

say()  { printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
ask()  { printf '\033[1;33m%s\033[0m' "$*"; read -r ans; }

# -- hyprctl text parsing (pure bash; no python3/jq) -------------------------

first_monitor() {
  # name of the first monitor in `hyprctl monitors`
  local line
  while IFS= read -r line; do
    if [[ "$line" == "Monitor "* ]]; then
      line="${line#Monitor }"
      printf '%s' "${line%% *}"
      return 0
    fi
  done < <(hyprctl monitors)
  return 1
}

monitor_field() { # $1 = monitor name, $2 = field (e.g. "scale")
  local in_block=0 line val
  while IFS= read -r line; do
    if [[ "$line" == "Monitor "* ]]; then
      if [[ "$line" == "Monitor $1 "* ]]; then in_block=1; else in_block=0; fi
    elif [[ $in_block -eq 1 && "$line" == *"$2: "* ]]; then
      val="${line##*: }"
      printf '%s' "${val//[[:space:]]/}"
      return 0
    fi
  done < <(hyprctl monitors)
  return 1
}

# ---------------------------------------------------------------- 0. devshell
say "0. building inside the flake devshell (nix develop)"
nix develop -c cargo build --release
BIN=./target/release/hyprbeat
test -x "$BIN"

# ------------------------------------------------------------------ 1. headless
say "1. headless PNG (logical 1920x1080 @ scale 1.25 -> buffer 2400x1350)"
"$BIN" --headless --size 1920x1080 --scale 1.25 --out /tmp/hyprbeat-p1-125.png
test -s /tmp/hyprbeat-p1-125.png
echo "wrote /tmp/hyprbeat-p1-125.png ($(stat -c%s /tmp/hyprbeat-p1-125.png) bytes)"
ask "open it in an image viewer — look for: pink 2px border, faint 1px grid, concentric rings right of centre, checker strip along the bottom, vertical pink band. All present and straight? (Enter alone = no) [y/N] "
[[ "${ans:-n}" == "y" ]] || echo "NOTE: headless output flagged as wrong — describe what you saw for CONTEXT.md §14"

# ----------------------------------------------------------- 2. live at 100%
say "2. live wallpaper at current scale for 8 s — watch your desktop background"
"$BIN" --timeout 8
ask "was the animated gradient visible on the background? [y/N] "
GRADIENT="${ans:-n}"

say "2b. live test card at current scale for 8 s"
"$BIN" --testcard --timeout 8
ask "test card sharp at your CURRENT scale? [y/N] "
SHARP_100="${ans:-n}"

# ---------------------------------------------------------- 3. live at 125%
if [[ -z "$MONITOR" ]]; then
  MONITOR="$(first_monitor)" || { echo "cannot list monitors — is hyprctl in PATH?"; exit 1; }
  echo "auto-picked monitor: $MONITOR"
fi
OLD_SCALE="$(monitor_field "$MONITOR" scale)" || { echo "cannot read scale of $MONITOR"; exit 1; }
restore_scale() { hyprctl output "$MONITOR" scale "$OLD_SCALE" >/dev/null; echo "restored $MONITOR scale to $OLD_SCALE"; }
trap restore_scale EXIT

say "3. setting $MONITOR to 125% (was $OLD_SCALE) and running the test card 10 s"
hyprctl output "$MONITOR" scale 1.25
sleep 1
"$BIN" --testcard --timeout 10
ask "test card still crisp at 125% (border 2 px, squares square, not smeared)? [y/N] "
SHARP_125="${ans:-n}"

# ----------------------------------------------------------------- 4. probe
say "4. Q-1/Q-2 probe for 10 s"
echo "    FIRST switch to an EMPTY workspace (no windows — D-28: windows above"
echo "    the background layer capture pointer/keyboard, which invalidates the probe)."
echo "    Then: CLICK the desktop once, TYPE a few keys, WIGGLE the mouse."
"$BIN" --probe --timeout 10

# ------------------------------------------------------------------ summary
say "summary — paste into CONTEXT.md §10 row 1 / §14"
hyprctl version | head -1
cat <<EOF
monitor under test          : $MONITOR (restored to scale $OLD_SCALE)
gradient visible            : $GRADIENT
test card sharp at 100%     : $SHARP_100
test card sharp at 125%     : $SHARP_125
probe Q-1/Q-2 verdict       : see the report above
headless PNG                : /tmp/hyprbeat-p1-125.png
EOF

# optional §3.6 spot checks (manual, 60 s):
cat <<'EOF'

Optional §3.6 budget spot checks (not automated):
  occluded CPU : run `hyprbeat --timeout 0 &` under a fullscreen window and
                 watch `top` (~0% expected — no frame callbacks arrive)
  visible CPU  : DEMO @30fps ≤ 2% of one core; check `top` during step 2
  cold start   : `time ./target/release/hyprbeat --timeout 3` ≤ 2 s to first frame
EOF
