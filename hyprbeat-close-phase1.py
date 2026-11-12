#!/usr/bin/env python3
# hyprbeat phase-1 closure — CONTEXT.md §13 end-of-session update (2026-11-12).
#
# Writes the run-6 verdict and all §3.6 measurements into CONTEXT.md (+ one
# README fix): header, §3.5 free-pause bullet, §3.6 occluded budget row,
# §10 phase-1 row -> DONE, new D-29, new Q-7.
#
# Run from the repo root:      python3 hyprbeat-close-phase1.py
# No python3 on PATH?          nix shell nixpkgs#python3 --command python3 hyprbeat-close-phase1.py
#
# Safety: backs up to CONTEXT.md.bak + README.md.bak, verifies EVERY anchor
# matches before writing anything, aborts untouched otherwise. Re-running
# after success is a no-op.

import pathlib, re, shutil, sys

CM = pathlib.Path("CONTEXT.md")
RM = pathlib.Path("README.md")
for f in (CM, RM):
    if not f.exists():
        sys.exit(f"cannot find {f} — run me from the repo root (~/hyprbeat)")

cm = CM.read_text(encoding="utf-8")
rm = RM.read_text(encoding="utf-8")

if "D-29" in cm:
    sys.exit("CONTEXT.md already contains D-29 — nothing to do.")

# ------------------------------------------------------------------ new text
HEADER_NEW = '''> **Last updated**: 2026-11-12 — **phase 1 → DONE** (validation run 3 + §3.6
> spot-checks complete, dev machine, Hyprland 0.57.1). Exit criteria and
> probes all green: gradient live at 100 % (~27.4 fps @ 30 cap), test card
> sharp at 100 % + 125 %, headless PNG confirmed, input probes PASS on an
> empty workspace (pointer/buttons, 4596 relative deltas, 65 keys — D-28).
> §3.6 measured: visible 0.7–1.0 % of one core (≤ 2 % PASS), RAM 53 MB
> (≤ 150 MB PASS), cold start ~0.3 s (PASS); **occluded ≈ visible ≈ 1 % — the
> "~0 % free pause" does not exist on Hyprland 0.57.1** (callbacks keep
> flowing under verified true fullscreen — D-29/Q-7; phase 7 inherits real
> pause duty).
> **Current phase**: 1 — **DONE**. Next: phase 2 (audio engine, §10 row 2).
'''

S35_OLD = '''- **Frame-callback driven**: render only when the compositor sends frame
  callbacks. When fully occluded (fullscreen window on top), Hyprland stops
  sending them → the wallpaper costs ~0. This is our "free pause".'''
S35_NEW = '''- **Frame-callback driven**: render only when the compositor sends frame
  callbacks. ~~When fully occluded, Hyprland stops sending them → cost ~0
  ("free pause").~~ **[FALSIFIED on Hyprland 0.57.1 — D-29]**: under verified
  true fullscreen (opaque window, bar killed) callbacks keep arriving at
  ~the full output rate; occluded cost ≈ visible cost ≈ 1 %. The real pause
  is the phase-7 mode switch (stop requesting frames / detach);
  occlusion-pause remains a sway/niri portability question (verify later).'''

S36_OLD = "| DEMO, fully occluded | ~0% CPU (no frame callbacks arriving — verify!) |"
S36_NEW = ('| DEMO, fully occluded | **measured ≈ visible ≈ 0.7–1.0 %** — Hyprland '
           '0.57.1 keeps sending callbacks under true fullscreen (D-29); "~0 %" '
           'withdrawn, real pause arrives with phase-7 mode switching |')

ROW1_NEW = ('| **1** | Cargo project, layer-shell surface, render loop, frame '
            'callbacks | Gradient/textured surface on Hyprland background, '
            '`--headless` PNG works, correct at 125% fractional scale | '
            '✅ **DONE (2026-11-12, Hyprland 0.57.1)** — gradient live at 100 % '
            '(~27.4 fps @ 30 cap, 165 Hz output); test card sharp at 100 % AND '
            '125 %; headless PNG confirmed; probes on an empty workspace: '
            'pointer enter/buttons, relative deltas (4596), sustained keys '
            '(65) — Q-1/Q-2/Q-6 PASS (D-28; D-27 superseded). 2 live render '
            'bugs fixed en route (D-26, frame-gate mix-up). §3.6 budgets: '
            'visible 0.7–1.0 % of one core across all runs (≤ 2 % — PASS), '
            'RAM 53 MB engine-only (≤ 150 MB — PASS), cold start ~0.3 s '
            '(≤ 2 s — PASS), occluded ≈ visible ≈ 1 % — the "~0 % free pause" '
            'is falsified on Hyprland 0.57.1 (D-29/Q-7; callbacks keep flowing '
            'under verified true fullscreen); real pause duty → phase 7 mode '
            'switch. |')

D29 = ('| D-29 | Hyprland 0.57.1 does **not** stop frame callbacks to a '
       'background-layer surface under true fullscreen occlusion (run 6: '
       'EasyEffects, opaque, `fullscreen: 2` verified via `hyprctl '
       'activewindow`, waybar killed, 35 s) — callbacks continued at ~165 Hz '
       '(two reports dipped to ~132/106 Hz during the window\'s startup '
       'churn; never stopped), rendered=137/5 s, watchdog_rearms=0, CPU '
       '0.8–1.0 % ≈ visible cost. §3.5 free-pause assumption falsified on '
       'this machine; §3.6 occluded budget amended to ≈ visible (≤ 2 %); '
       'real pause = phase-7 mode switch (stop requesting frames). Caveat: '
       'gpu-screen-recorder was running during the test (capture may force '
       'full-scene compositing — optional retest without it) | Run 6 of the '
       '§3.6 spot-checks; runs 1–5 hit confounds (maximize-variant keybind, '
       'translucent browser/kitty, one exit-127 no-op run) |')

Q7 = '''- **Q-7 (new, 2026-11-12, §3.6 spot-check run 6)**: ~~do frame callbacks stop
  arriving when an opaque window truly fullscreens over the background layer
  (the §3.5 "free pause")?~~ **ANSWERED 2026-11-12 (Hyprland 0.57.1): NO** —
  fullscreen verified (`fullscreen: 2`), waybar killed, opaque EasyEffects
  covering the output for 35 s: callbacks continued (~165 Hz; rendered=137
  per 5 s, watchdog_rearms=0), CPU stayed 0.8–1.0 %. Recorded as D-29;
  §3.5/§3.6 amended; real pause duty moved to phase 7. (gpu-screen-recorder
  was running; optional retest without it.)
'''

RM_OLD = '''- frame-callback driven rendering with an idle fps cap; an occluded surface
  receives no callbacks and costs ~0 CPU'''
RM_NEW = '''- frame-callback driven rendering with an idle fps cap; occlusion pause is
  compositor-dependent — Hyprland keeps sending callbacks under fullscreen
  (measured ~1 % of one core; a real pause arrives with phase-7 mode
  switching)'''

# ------------------------------------------------------------------ apply
fail = []

# 1. header block (works for every spot-check vintage) incl. Current phase
pat = re.compile(
    r"> \*\*Last updated\*\*: 2026-11-12, validation run 3.*?pending\)\.\n",
    re.DOTALL)
cm2, n = pat.subn(HEADER_NEW, cm)
if n != 1:
    fail.append("header block")

# 2. §3.5 free-pause bullet
if S35_OLD in cm2:
    cm2 = cm2.replace(S35_OLD, S35_NEW, 1)
else:
    fail.append("§3.5 free-pause bullet")

# 3. §3.6 occluded budget row
if S36_OLD in cm2:
    cm2 = cm2.replace(S36_OLD, S36_NEW, 1)
else:
    fail.append("§3.6 occluded budget row")

# 4. §10 phase-1 row -> DONE
lines = cm2.split("\n")
idx = [i for i, l in enumerate(lines) if l.startswith("| **1** | Cargo project")]
if len(idx) == 1:
    lines[idx[0]] = ROW1_NEW
    cm2 = "\n".join(lines)
else:
    fail.append("§10 phase-1 row")

# 5. D-29 appended after the D-28 row
lines = cm2.split("\n")
idx = [i for i, l in enumerate(lines) if l.startswith("| D-28 |")]
if len(idx) == 1:
    lines.insert(idx[0] + 1, D29)
    cm2 = "\n".join(lines)
else:
    fail.append("D-28 row")

# 6. Q-7 appended after Q-6
q6_tail = "`dev/validate-phase1.sh` instructions\n  updated accordingly.\n"
if q6_tail in cm2:
    cm2 = cm2.replace(q6_tail, q6_tail + Q7, 1)
else:
    fail.append("§14 Q-6 tail")

# 7. README occlusion bullet
if RM_OLD not in rm:
    fail.append("README occlusion bullet")

if fail:
    sys.exit("ABORTED — anchors not found: " + ", ".join(fail) +
             "\nNo files were changed. Upload your CONTEXT.md instead.")

shutil.copy2(CM, str(CM) + ".bak")
shutil.copy2(RM, str(RM) + ".bak")
CM.write_text(cm2, encoding="utf-8")
RM.write_text(rm.replace(RM_OLD, RM_NEW, 1), encoding="utf-8")
print("OK — CONTEXT.md + README.md updated (backups: .bak)")
print("  header          -> phase 1 DONE")
print("  §3.5 free-pause -> falsified, D-29")
print("  §3.6 occluded   -> measured ≈ 0.7–1.0 %")
print("  §10 row 1       -> DONE with all budgets")
print("  §9              -> D-29 appended")
print("  §14             -> Q-7 appended (answered)")
print("Next: git add -A && git commit -m 'phase 1 done: budgets measured; D-29 free-pause falsified'")
