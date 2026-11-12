# hyprbeat — Project Context & Architecture

> **PURPOSE OF THIS FILE** — This is the single source of truth for AI-assisted
> development of this project. Any new chat/session MUST read this file
> completely before writing a single line of code. Decisions recorded here are
> final unless explicitly marked `[REVISITABLE]`.
>
> **Last updated**: 2026-11-12 — **phase 1 → DONE** (validation run 3 + §3.6
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

---

## 1. What this project is

**hyprbeat** is a **Wayland-native rhythm game
that runs as your desktop wallpaper** on Hyprland (and other wlr-layer-shell
compositors such as sway/niri).

- It renders on the compositor's **background layer** via layer-shell.
- It is **playable**: click the exposed desktop → the wallpaper receives input,
  you play the currently loaded map. Focus another window → the game pauses.
- In idle "wallpaper mode" it runs an **autoplay demo bot** (attract mode)
  so the desktop looks alive, heavily dimmed.
- It reads the **osu! beatmap format** (`.osz` / `.osu` files) — it is a
  *client* for that format, not a clone of any specific game.
- Written in **Rust**. No servers, no hosting, no accounts.

**One-liner for README**: *"An open-source rhythm game that lives on your
desktop background, compatible with the osu! beatmap format. Built for
Hyprland/Wayland."*

### 1.1 What it is NOT

- **NOT a fork of osu!lazer.** Zero code is taken from ppy/osu. This is an
  independent reimplementation of publicly documented mechanics and file
  formats. (Lazer is MIT, so porting would be legal — it is simply not the
  chosen path. See §9, decision D-07.)
- **NOT a service.** No central beatmap mirror, no user accounts, no hosted
  content. Tool-maker posture (see §2).
- **NOT affiliated with osu!/ppy.** Never put "osu" in the product name.

---

## 2. Legal posture (READ BEFORE ANY RELEASE)

Position: **tool maker**, not service provider. Same category as VLC/mpv.

| Rule | Detail |
|---|---|
| No hosting | We never distribute beatmaps or audio from any server we run. Users import their own files. |
| DMCA machinery | **Not required.** DMCA §512 applies to service providers hosting user content. We host nothing, so there is nothing to take down. Do not build takedown flows. |
| Trademark | Product name must never contain "osu". Describe as *"compatible with the osu! beatmap format"*. peppy actively enforces the mark. |
| Bundled content | Default song pack and default hitsounds must be **CC-licensed or original only**. Nothing copyrighted ships with the binary. |
| Banlist (optional) | A hardcoded SHA-256 denylist of known-problematic files is *community optics*, not a legal shield. One const array, checked at load. Only add hashes if a specific complaint arrives. |
| Facilitation line | A file-picker for the user's own library = fine. A "search the internet for beatmaps" feature pointing at piracy sites = NOT fine. Never build the latter. |
| License | Code: MIT **or** Apache-2.0 (pick at phase 1 scaffold; default MIT). |
| README disclaimer | Must include: *"This is a standalone tool. We do not host or distribute copyrighted beatmaps; what users do with their own files is their responsibility."* |

**Trigger condition**: if a server-side beatmap listing, user accounts, or any
hosted community feature is ever added, the legal posture changes categorically
and a DMCA/DSA compliance layer becomes mandatory. This is a deliberate
non-goal for v0.x.

---

## 3. System architecture

### 3.1 High-level diagram

```
┌────────────────────────────── Hyprland (compositor) ──────────────────────────────┐
│                                                                                   │
│   ┌── background layer ──┐   ┌── normal windows ────────────────────────────┐    │
│   │  hyprbeat surface   │   │   terminals, browser, ... (above us always)   │    │
│   │  (zwlr_layer_surface)│   └───────────────────────────────────────────────┘    │
│   └──────────┬───────────┘                                                        │
└──────────────┼────────────────────────────────────────────────────────────────────┘
               │ wl_pointer / wl_keyboard / wl_touch / wl_tablet
               │ frame callbacks (damage-driven)
               │ wl_surface buffers (DMA-BUF, zero-copy)
┌──────────────▼────────────────────────────────────────────────────────────────────┐
│ hyprbeat (single process)                                                        │
│                                                                                   │
│  ┌────────────┐   AtomicU64    ┌──────────────────────────────────────────┐       │
│  │ cpal audio │──sample count─▶│ main thread (calloop)                    │       │
│  │ RT callback│                │  ├─ wayland dispatch (events, frame cb)  │       │
│  │ NO alloc   │                │  ├─ game logic (state machine)           │       │
│  │ NO locks   │                │  └─ wgpu render (instanced quads)        │       │
│  └────────────┘                └──────────────────────────────────────────┘       │
│  ┌────────────┐   RwLock<Config>                                                  │
│  │ config     │──hot reload────────────▶ read by everything                      │
│  │ watcher    │                                                                  │
│  │ (std thr)  │                                                                  │
│  └────────────┘                                                                  │
└───────────────────────────────────────────────────────────────────────────────────┘
```

### 3.2 Threading model

Minimum viable: **two threads + one watcher.**

| Thread | Role | Constraints |
|---|---|---|
| **Audio callback** (cpal, real-time) | Mix music + hitsound samples into the output buffer; increment the global sample counter. | **NEVER allocate. NEVER take a lock. ONLY `AtomicU64::fetch_add` and buffer writes.** (INVARIANT I-02) |
| **Main thread** (calloop event loop) | Wayland dispatch, input events, game logic, wgpu rendering driven by frame callbacks. | Single-threaded logic keeps the state machine trivial. All shared state owned here. |
| **Config watcher** (`std::thread` + `notify`) | Watch `config.toml`, validate, publish via `RwLock<Config>`. | On parse/validation failure: log + keep previous config. Never crash (INVARIANT I-06). |

A dedicated render thread is a **later optimization** only if profiling demands
it. Do not add it preemptively.

### 3.3 The audio clock — THE core invariant

Game time derives **exclusively** from the audio sample counter, never from
`Instant::now()`:

```rust
// audio/clock.rs
pub struct Clock {
    samples: AtomicU64,      // written ONLY by the audio callback
    sample_rate: u32,        // e.g. 48000
    started_at_sample: u64,  // sample index when the song started
    offset_ms: f64,          // per-map AudioLeadIn + universal offset (config)
}

impl Clock {
    /// THE time source for all game logic and rendering.
    pub fn time_ms(&self) -> f64 {
        let s = self.samples.load(Ordering::Relaxed);
        ((s - self.started_at_sample) as f64 / self.sample_rate as f64) * 1000.0
            + self.offset_ms
    }
}
```

Why: the player judges hits by **what they hear**. Deriving time from the audio
stream makes desync structurally impossible (this is how osu! itself works).
`Instant::now()` drifts against the DAC buffer and must never be used for
gameplay time. (INVARIANT I-01)

**Rendering lookahead**: draw frame *N+1* aimed at `time_ms + frame_time_ms`
to compensate display latency; make the lookahead a config value
(`[general] render_lookahead_ms`).

**Pausing**: pause = stop advancing `started_at_sample` mapping (store a
pause-point sample count; on resume, rebase `started_at_sample`). Audio device
time never rewinds — the mapping does.

**No-audio fallback (D-16)**: if the output stream cannot be opened at startup
or is lost at runtime, the clock degrades to `ClockSource::Wall`
(`Instant::now()`-based) and the HUD shows `"⚠ timing degraded — no audio"`.
Gameplay remains possible but scores in that session are marked unranked.
`--headless` and CI use this path explicitly. Never a hard crash on missing
audio — a wallpaper that dies with PipeWire is unacceptable (I-08, I-12).

### 3.4 Two-mode state machine (input switching)

Layer-shell lets us toggle input handling with two double-buffered requests —
this IS the wallpaper/game duality:

```
WALLPAPER_DEMO ⇄ PLAYING ⇄ PAUSED     (+ HIDDEN)
```

**The transition table below is normative** — the implementer codes against
it, not against prose:

| # | Current | Event | Next | Side effects |
|---|---|---|---|---|
| T1 | WALLPAPER_DEMO | click on exposed desktop OR `toggle_play` keybind | PLAYING | input region ON + kbd `on_demand`; map **restarts from the top** with lead-in (taking over a demo mid-map would be unjudged/unfair) |
| T2 | WALLPAPER_DEMO | song ends | WALLPAPER_DEMO | auto-advance playlist, autoplay continues |
| T3 | PLAYING | focus lost (any other window focused) | PAUSED | input region OFF, kbd `none`; audio frozen (§3.3 rebase) |
| T4 | PLAYING | `pause` keybind / Esc | PAUSED | same as T3 |
| T5 | PLAYING | song ends | WALLPAPER_DEMO | results overlay ~5 s → demo advances |
| T6 | PAUSED | click on exposed desktop OR `toggle_play` keybind | PLAYING | resume from pause point (audio rebase) |
| T7 | PAUSED | `paused_timeout_s` elapsed (default 30; 0 = never) | WALLPAPER_DEMO | demo bot takes over from the pause point |
| T8 | any | config `mode` becomes `hidden` | HIDDEN | flat theme `background` colour only — no map, no demo, no input; surface stays mapped (no flash on return) |
| T9 | HIDDEN | config `mode` leaves `hidden` | WALLPAPER_DEMO | demo resumes |
| T10 | any | `hyprbeat-ctl quit` | exit | clean shutdown |

Notes:
- **Song select is an overlay** (`render/ui.rs`), not a state — it can appear
  over DEMO or PAUSED; picking a song enters DEMO unless the user starts play.
- Every transition flips input atomically: `set_input_region(...)` +
  `set_keyboard_interactivity(...)` + `wl_surface.commit()` — double-buffered,
  no flicker. (INVARIANT I-05)

Keyboard interactivity: use **`on_demand`**, not `exclusive` — `exclusive`
would steal the keyboard from the whole session. **Phase-1 outcome (probe,
2026-11-12, Hyprland 0.57.1): WORKS FULLY on the background layer** —
`wl_keyboard.enter`/`leave` delivered, focus AUTO-GRANTED when the surface
maps on an empty workspace, and 65 key events delivered while focus held.
Keys go to any window that takes focus — exactly the T3 pause behaviour we
want. No fallback needed.

### 3.5 Rendering architecture

- **wgpu**, one instanced-quad pipeline for 95% of drawing
  (circles, approach rings, cursor, UI panels, slider bodies as triangle fans
  in a second small pipeline or via poly quads).
- **Texture atlas**: all sprites (hit circles, numbers, cursor, UI) in one
  atlas, one bind group → one draw call per layer batch.
- **Colors as instance attributes** → theming is free per-quad (§6).
- **Frame-callback driven**: render only when the compositor sends frame
  callbacks. ~~When fully occluded, Hyprland stops sending them → cost ~0
  ("free pause").~~ **[FALSIFIED on Hyprland 0.57.1 — D-29]**: under verified
  true fullscreen (opaque window, bar killed) callbacks keep arriving at
  ~the full output rate; occluded cost ≈ visible cost ≈ 1 %. The real pause
  is the phase-7 mode switch (stop requesting frames / detach);
  occlusion-pause remains a sway/niri portability question (verify later).
- **Idle mode**: in WALLPAPER_DEMO, self-throttle to `target_fps_idle`
  (default 30) by skipping frames between callbacks.
- Text: `cosmic-text` (or `glyphon`) rendered into atlas updates; avoid
  re-rasterizing per frame (cache glyphs by (font, size, color) key).
- Coordinates: osu! playfield is **512×384** virtual units, letterboxed into
  the output with configurable margins (see `[layout] playfield_margin`).
- **Scale (D-17)**: render at the compositor's per-output scale (§8 HiDPI);
  game math stays in logical osu!px and converts only at draw time.

### 3.6 Performance budgets (checked at phase exit, not vibes)

Performance is the reason this project is native Rust (D-01) — so it gets
numbers. Measured on the dev machine (discrete GPU + iGPU), recorded in the
§10 status row when a phase exits:

| Scenario | Budget |
|---|---|
| DEMO, fully occluded | **measured ≈ visible ≈ 0.7–1.0 %** — Hyprland 0.57.1 keeps sending callbacks under true fullscreen (D-29); "~0 %" withdrawn, real pause arrives with phase-7 mode switching |
| DEMO, visible @ 30 fps | ≤ 2% of one core |
| PLAYING | frame time ≤ 4 ms on iGPU-class hardware; ≤ 15% of one core |
| Input → sound → photon | ≤ 25 ms at 60 Hz (via `--latency-test`, phase 2) |
| RAM with one map + decode cache | ≤ 150 MB |
| Cold start → first frame | ≤ 2 s |

CI cannot measure these (no GPU/audio); they are **manual release gates**. A
phase whose exit criteria pass but budgets blow up is NOT done — investigate,
or record a dated waiver in §14.

---

## 4. Crate stack (pin versions at phase-1 scaffold)

| Concern | Crate | Why / notes |
|---|---|---|
| Wayland client | `smithay-client-toolkit` (sctk) | Native layer-shell. **winit is NOT usable** — its layer-shell support is still unmerged (rust-windowing/winit#2582). |
| Event loop | `calloop` + `calloop-wayland-source` | Clean main-loop; integrates Wayland dispatch. |
| Rendering | `wgpu` | Portable, instanced quads, mature. |
| Audio output | `cpal` (**not** rodio) | We need the raw output callback — it is both our clock and our latency floor. rodio hides it. |
| Audio decode | `symphonia` | mp3/ogg/flac/wav — the formats actually found in `.osz` zips. Decode to f32 in a worker (NOT in the RT callback; pre-decode whole file to RAM — songs are ~50 MB f32, fine). |
| Beatmap math | `rosu-pp` | Parses `.osu` internally for PP calc. **Verify in phase 3** how much of the parsed model it exposes (hit objects, slider points, timing points); supplement with our own parser where incomplete. |
| Config | `serde` + `toml` + `notify` + `parking_lot` | See §6. |
| Archives / hashing | `zip` + `sha2` | `.osz` import, content-addressed dedup, optional banlist. |
| Text | `cosmic-text` (or `glyphon`) | Song select, score, hints. |
| Paths | `directories` | XDG base dirs. |

Everything else std-only where possible. Keep the dependency tree small — this
runs 24/7 on people's desktops.

*Phase-1 status: versions are pinned in `Cargo.toml` with a committed
`Cargo.lock` (see D-25 for the realized stack and the wgpu-30 specifics).*

---

## 5. Module layout

```
src/
├── main.rs               entry: config load, wayland connect, calloop setup
├── config/
│   ├── mod.rs            Config struct, load/validate/merge (defaults<file<CLI)
│   ├── theme.rs          Color newtype (hex parse, fail-loud), presets
│   └── watcher.rs        notify-based hot reload thread
├── wayland/
│   ├── mod.rs            connection, registry, outputs (one surface per output)
│   ├── surface.rs        layer-shell surface, mode switching (§3.4)
│   └── input.rs          pointer/kbd/touch/tablet → game events; raw mouse deltas via zwp_relative_pointer_v1 for [input] sensitivity (§6)
├── audio/
│   ├── mod.rs            cpal stream setup (f32, 48kHz, output callback)
│   ├── clock.rs          §3.3
│   ├── mixer.rs          sample-accurate mix of music + hitsounds
│   └── decode.rs         symphonia → f32 mono/stereo buffer cache
├── beatmap/
│   ├── mod.rs
│   ├── osu.rs            .osu text parser (v14; see §5.1)
│   ├── osz.rs            zip import, SHA-256 dedup, library index
│   ├── model.rs          Beatmap, HitObject, TimingPoint, Difficulty attrs
│   └── slider.rs         path math: Catmull-Rom / perfect arcs / linear,
│                         equidistant arc-length reparam, slider ticks
├── game/
│   ├── mod.rs            state machine (Demo/Playing/Paused)
│   ├── rules.rs          AR/CS/OD/HP formulas (§7.2), timing windows
│   ├── logic.rs          hit detection, judgement, combo, HP, score
│   ├── autoplay.rs       demo bot (always-perfect at T-0; grade target config)
│   └── stacking.rs       note-stack offset algorithm (phase 6)
├── render/
│   ├── mod.rs            wgpu init, frame loop, damage/throttle policy
│   ├── pipeline.rs       instanced quad pipeline + atlas bind groups
│   ├── sprites.rs        circle/approach/slider/follow/cursor instances
│   └── ui.rs             song select, HUD, key hints, dim overlays
└── util/
    ├── color.rs          if not in config/theme
    └── math.rs           catmull, arc-length, easing
```

### 5.1 `.osu` format — condensed parser spec (v14)

Text format, `[Section]` headers, `key: value` / `key,value,...` lines.

- **[General]**: `AudioFilename`, `AudioLeadIn` (ms), `Mode` (0 = osu!standard
  only for v0.1; others → reject with clear error), `StackLeniency`.
- **[Metadata]**: `Title`, `TitleUnicode`, `Artist`, `ArtistUnicode`,
  `Creator`, `Version` (difficulty name), `BeatmapID`, `BeatmapSetID`.
- **[Difficulty]**: `HPDrainRate`, `CircleSize`, `OverallDifficulty`,
  `ApproachRate`, `SliderMultiplier` (base slider velocity, 100 osu-px/beat),
  `SliderTickRate`.
- **[TimingPoints]**: `time,beatLength,meter,sampleSet,sampleIndex,volume,
        uninherited,effects` — `uninherited=1` lines are BPM anchors
  (`beatLength` = ms per beat); `uninherited=0` inherit: slider velocity
  multiplier = `-100 / beatLength` (e.g. beatLength=-50 → 2× speed), volume,
  kiai flag in `effects` bit 1.
- **[HitObjects]**: `x,y,time,type,hitSound,objectParams,hitSample`
  - `type` bitmask: `1` circle, `2` slider, `8` spinner, `4` new combo,
    bits 4–6 = colour skip.
  - circle: no params. spinner: `endTime`.
  - slider params: `curveType:curvePoints|slides|length|edgeHitsounds|
    edgeAdditions` — curveType `B` (bezier; repeated ctrl points = segment
    break), `C` (Catmull-Rom), `L` (linear), `P` (perfect circle through pts).
  - Coordinates 0–512 × 0–384.
- Numbers may be `int` or `float` depending on field; parse leniently, store f32.

**Golden-file tests**: commit 5–10 tiny synthetic `.osu` fixtures (CC audio
placeholders, no copyrighted metadata) covering circles, all curve types,
repeats, inherited points, kiai. Parser must be unit-testable with zero I/O.

### 5.2 Library layout on disk (XDG)

```
~/.config/hyprbeat/config.toml
~/.local/share/hyprbeat/
    songs/<set-id>/{audio.mp3, *.osu, bg.jpg}     # extracted from .osz
    index.sqlite (or .json in v0.1)               # sha256 → metadata cache
~/.cache/hyprbeat/
    decoded/<sha256>.f32                          # audio decode cache
```

Import flow: pick `.osz` → hash → dedupe (skip if seen) → extract → index →
available in song select. **Content-addressed by SHA-256 of each file** — same
audio across maps is stored once (mirrors lazer's hash-based storage).

### 5.3 Default assets & first-run behaviour

- **Hitsounds**: a small CC0 default set ships embedded in the binary
  (`include_bytes!` from `assets/hitsounds/` in the repo, a few KB) so gameplay
  audio works with zero setup. Skins override later (v0.2+, §7.6).
- **Default song pack**: CC-licensed demo maps live in the repo under
  `assets/default-pack/` — NOT embedded (keeps the binary lean and the I-03
  audit trivial: `git ls-files assets/`). First run offers to install them
  into the library (`hyprbeat-ctl install-defaults` or hint-bar prompt).
- **First run**: if `config.toml` is missing, write a fully-commented default
  (§6) before loading, so users discover options by reading the file.
- **Empty-library state** (no songs → no demo possible): slow animated
  gradient in theme colours + hint bar ("no maps — `hyprbeat-ctl
  install-defaults` or press SUPER+G"). Never a black screen, never a crash.

---

## 6. Configuration system (fully designed — phase 5 implements this)

`~/.config/hyprbeat/config.toml`, TOML, hot-reloaded, validated-on-load with
fallback-to-previous. Hierarchy: **defaults < file < CLI flags**.

```toml
[general]
mode = "auto"                  # "auto" | "demo" | "play" | "hidden"
                               #   hidden = flat theme background only, no
                               #   demo/input (transition T8)
target_fps_idle = 30
target_fps_active = 240        # render cap while playing
volume = 0.7
render_lookahead_ms = 8.0      # §3.3 display-latency compensation
play_output = "primary"        # "primary" | "all" — which output shows the
                               #   playfield on multi-monitor (§8 layer)

[render]
# On Wayland the COMPOSITOR owns output scale — "auto" follows it (sharp at
# any fractional scale). An explicit number is a PERF OVERRIDE for weak GPUs:
# render lower-res, let the compositor upscale; values above the output's
# scale are clamped to it. NOT display DPI (D-17); mouse speed → `[input]` (D-18).
scale = "auto"                 # "auto" | 0.5–2.0
ui_scale = 1.0                 # HUD / song-select / panel text+size multiplier

[theme]
preset = "midnight"            # "" | "sunset" | "midnight" | "mint" | "mono"
accent       = "#ff66aa"       # overrides preset fields individually
background   = "#0a0a14"
panel        = "#1a1a2e"
text         = "#ffffff"
text_dim     = "#8888aa"
hit_300      = "#66eeff"
hit_100      = "#88ff66"
hit_50       = "#ffcc44"
hit_miss     = "#ff4466"
slider_track = "#ff66aa"
slider_fill  = "#ffffff"

[layout]
menu_form       = "panel"      # "panel" | "minimal" | "fullscreen" | "hidden"
menu_position   = "right"      # "left" | "center" | "right"
menu_width_pct  = 35
corner_radius   = 12
playfield_margin = 5.0         # % of screen kept empty around the 512×384 box
show_song_info  = true
show_score      = true
key_hints       = true

[gameplay]
key_main        = "z"
key_alt         = "x"
background_dim  = 0.4
show_hit_error  = true
universal_offset_ms = 0.0
paused_timeout_s = 30         # PAUSED → demo after N s (T7); 0 = stay paused

[input]
# Mouse / pointer behaviour during PLAYING. In wallpaper/demo mode the pointer
# is always passthrough to the desktop — these apply only while playing.
#
# Hardware mouse DPI is NOT a software setting (adjust it on your mouse);
# mouse_sensitivity below is the user-side multiplier only.
mouse_sensitivity = 1.0     # multiplier on pointer deltas; 1.0 = native feel
raw_input         = true    # true  = zwp_relative_pointer_v1 deltas, bypasses
                            #         compositor acceleration (recommended)
                            # false = absolute wl_pointer position; sensitivity
                            #         is then ignored
invert_horizontal = false
invert_vertical   = false
edge_clamp        = true    # cursor stops at playfield edge, not screen edge

[wallpaper_demo]
autoplay        = true
autoplay_grade  = "S"          # "SS" | "S" | "A" | "B" | "random"
dim_during_demo = 0.7

[keybinds]                     # resolved through Hyprland config, see §8 note
toggle_play     = "SUPER+O"
cycle_songs     = "SUPER+BRACKETRIGHT"
pause           = "SUPER+P"
open_songselect = "SUPER+G"

[paths]
beatmap_dirs = [
    "~/.local/share/hyprbeat/songs",
]
```

Implementation notes:
- `Color` is a newtype with a custom `Deserialize` — malformed hex **fails
  loudly at load**, the watcher keeps the previous config and logs.
- Presets: `[theme] preset` loads a palette, then any explicitly-set field
  overrides it. Implement via serde `#[serde(flatten)]` intermediate struct.
- Config struct is `Arc<RwLock<Config>>` (`parking_lot`); render reads it
  every frame, watcher writes rarely. No per-field atomics.
- CLI overrides via `clap` (add to stack in phase 5): `--volume`, `--config`,
  `--map <path>` for dev testing.
- **First run**: missing `config.toml` → write fully-commented default (§5.3),
  then load it.

---

## 7. Gameplay specification (v0.1 = osu!standard mode only)

### 7.1 Hit objects

| Object | Behaviour |
|---|---|
| **Circle** | Tap within hit window. Judged by time delta: 300/100/50/miss. |
| **Slider** | Head judged as circle; follow the moving ball (ball snaps to cursor if inside follow circle, radius ~2.4×); ticks at `SliderTickRate` intervals; each repeat/reverse re-judges; tail judged; final 300/100/50/miss from fraction of ticks+edges hit. |
| **Spinner** | Rotate cursor (accumulated angle ≥ required revolutions before endTime). Required revolutions from OD. |

### 7.2 Difficulty formulas (classic osu! values — implement these for v0.1)

```
HIT WINDOW 300:  80  − 6·OD   ms      (OD10→20ms, OD0→80ms)
HIT WINDOW 100:  140 − 8·OD   ms
HIT WINDOW 50:   200 − 10·OD  ms

CIRCLE RADIUS:   54.4 − 4.48·CS   osu!px  (playfield 512×384)

APPROACH (preempt, time visible before hit):
  AR < 5:  1200 + 600·(5−AR)/5   ms
  AR ≥ 5:  1200 − 750·(AR−5)/5   ms     (AR10→450ms)
FADE-IN:
  AR < 5:   800 + 400·(5−AR)/5   ms
  AR ≥ 5:   800 − 500·(AR−5)/5   ms
```

> Note: osu!lazer introduced slightly different "lazer-mode" windows; v0.1
> targets the classic values above. If a `[gameplay] ruleset = "lazer"` option
> is ever added, do it as a formula set swap, never a fork of behaviour.

Score (v0.1, classic simplified): each judgement adds its base value (300/
100/50) multiplied by `1 + combo · multiplier / 25`, multiplier from map
difficulty. Accuracy = Σ(50·n50 + 100·n100 + 300·n300) / (300·Σn).
Exact multipliers get pinned in phase 4 with unit tests — treat the above as
the shape, verify constants against the community-documented formula when
implementing.

### 7.3 Slider path math (phase 6 — the boss fight)

- Parse curve into segments (type per segment from control points, §5.1).
- **Bézier** segments: repeated control points mark breaks; concatenate.
- **Catmull-Rom**: uniform, through all control points.
- **Perfect (P)**: circle through first three points, ignore extras.
- **Length**: slider duration = `length / (SliderMultiplier · SV) · beatLength`
  with SV from the active inherited timing point.
- Convert every path to a **polyline with equidistant arc-length
  parameterization** (sample ~2 osu-px steps, build a cumulative-length LUT,
  binary-search the LUT for `pos_at(d)`). Follow ball, ticks, and rendering all
  consume `pos_at(d)` — one LUT to rule them all.
- Rendering: triangle strip along the path with round caps; track colour from
  combo, fill via border/snaking per config.

### 7.4 Stacking

Consecutive stacked notes (same position, close in time) get rendered with a
small downward offset per stack level; algorithm derived from lazer's
`OsuHitObject` stacking (re-derive from behaviour, do not port code). Phase 6.

### 7.5 Autoplay bot (phase 7) — input simulation, NOT reward injection

**Design (D-15)**: the game loop has exactly ONE input path, fed by a source
abstraction:

```rust
enum InputSource {
    Wayland,   // real pointer/keyboard/touch events (phase 4+)
    Scripted,  // autoplay bot, replay playback (phase 7)
}

// both sources emit the same event type into the same pipeline:
struct InputEvent { at_ms: f64, pos: Option<(f32, f32)>, key: Option<KeyId>, down: bool }
```

The bot is a **pure function `(beatmap, time) → cursor position + taps at
T-0`** that emits the same `InputEvent`s a perfect human would. Judgement,
scoring, HP, hit-error, sounds — all flow through the one real pipeline.
Spinner: rotate the scripted cursor at max RPM.

Grade degradation (`autoplay_grade`) = timing jitter applied to scripted
input events (e.g. `target_acc` 98% → Gaussian jitter until accuracy matches) —
so the hit-error bar, HP drain and combo breaks look *authentic*.

**Why NOT "tell the reward function it hit" (direct judgement injection):**
- One judgement path: if scores can come from two places, they *will* drift
  apart and you chase phantom bugs. Demo mode must be **the real game played
  perfectly**, not a fake score feed.
- Demo mode doubles as a 24/7 pipeline self-test: any logic bug shows up on
  the idle desktop.
- **Unlocks replays for free**: replay playback = the same `Scripted` source
  fed a recorded input stream (osu! `.osr` is exactly that — a recorded
  input stream). Watching replays on your wallpaper becomes a v0.2 feature
  for ~zero extra architecture.
- Recording your own play for replays: log `InputEvent`s during PLAYING —
  the abstraction works in both directions.

Direct `judge(PERFECT)` calls are allowed **ONLY inside unit tests** of
scoring math (§11 I-09), never in the live game.

### 7.6 Deferred to v0.2+ (explicitly NOT in v0.1)

| Feature | Hook already in place |
|---|---|
| `.osr` replay recording + playback | `InputSource::Scripted` (§7.5) — record `InputEvent`s, feed them back |
| osu! skin format support | texture atlas is already swappable; needs a skin-element spec |
| Tablet area mapping (extend `[input]` config section) | `wl_tablet` already handled in `wayland/input.rs`; `[input]` mouse settings ship in v0.1 |
| Difficulty + PP display in song select | `rosu-pp` already in the stack (phase 3 verification) |
| In-game beatmap editor | out of scope; enormous |
| `ruleset = "lazer"` timing option | formula-set swap only (§7.2 note) |

---

## 8. Wayland integration details

- **Layer**: `ZWLR_LAYER_SHELL_LAYER_BACKGROUND`, anchored all edges, per
  output (one `LayerSurface` per `wl_output`; render the same scene, accept
  that multi-monitor means mirrored gameplay on all — pick "primary output"
  config for v0.1, `[general] play_output = "primary" | "all"`).
- **Keyboard interactivity**: `on_demand` when PLAYING, `none` otherwise
  (§3.4 — validate in phase 1, record fallback decision here).
- **Frame pacing**: only render inside `frame` callbacks; in PLAYING render
  every callback up to `target_fps_active`; in DEMO skip to hold
  `target_fps_idle`.
- **Keybinds**: Hyprland owns global keybinds. The `[keybinds]` config
  documents what the USER should put in their `hyprland.conf` (e.g.
  `bind = SUPER, O, exec, hyprctl dispatch focuswindow ...` or a small
  `hyprbeat-ctl` helper that talks to the app via a Unix socket control
  interface — decide in phase 7; the socket approach is likely cleaner and
  also enables `hyprbeat-ctl next`, `hyprbeat-ctl pause`).
- **Control socket security (D-14)**: `$XDG_RUNTIME_DIR/hyprbeat.sock`,
  mode `0700`, peer credentials checked via `SO_PEERCRED` (same-uid only);
  refuse to open the socket if `XDG_RUNTIME_DIR` is missing. Another local
  user must not be able to drive your wallpaper.
- **HiDPI / fractional scaling**: per-output scale comes from the compositor
  (`wl_output` scale + `wp_fractional_scale_v1`; buffers via `wp_viewporter`).
  Never assume 1.0. Layout math in logical px; buffer = logical × scale.
  `[render] scale = "auto"` follows it; explicit values are perf overrides
  only, clamped (D-17). Phase 1 MUST exit having tested 100% AND 125%.
- **Minimum compositor**: expect Hyprland ≥ 0.41 (fractional-scale v1),
  layer-shell v4, `wp_viewporter`, `zwp_relative_pointer_v1` (raw input,
  §6 `[input]`; fallback `raw_input = false`). **Confirmed working on the dev
  machine: Hyprland 0.57.1** (Nix build, 2026-11-12 — viewporter,
  fractional-scale, background layer-shell all bind; 165 Hz output). Versions
  below 0.57.1 remain untested.
- **Cursor**: hide the compositor cursor over the surface when PLAYING
  (`wl_pointer` hide via `set_cursor` empty) and draw our own — osu! style.
  In PLAYING, consume `zwp_relative_pointer_v1` motion deltas for raw-input
  aim (§6 `[input]`); fall back to absolute `wl_pointer` position when
  `raw_input = false`. Pointer is always passthrough in wallpaper/demo mode.
  **Phase-1 probe finding (2026-11-12, Hyprland 0.57.1, empty workspace):
  pointer enter/button AND relative deltas all reach the background layer —
  raw input confirmed viable; `raw_input = true` stays the default (D-28).**
- **Debug tooling**: `WAYLAND_DEBUG=1` env for protocol traces; `wgpu`
  validation on in dev builds; `--headless` flag (no layer-shell, offscreen
  render to PNG) for CI and quick iteration without touching the live desktop.

---

## 9. Decision log (do not re-litigate unless `[REVISITABLE]`)

| ID | Decision | Rationale |
|---|---|---|
| D-01 | Native Rust client, not lazer-wrap, not webview | Performance ranking: input latency + 24/7 idle cost dominate; webview last (JS GC, WebKitGTK DMA-BUF issues). |
| D-02 | `smithay-client-toolkit` + `calloop`, **no winit** | winit layer-shell unmerged (rust-windowing/winit#2582). |
| D-03 | `cpal` raw callback, not rodio | The callback is the clock + latency floor. |
| D-04 | Audio clock is the only time source | Sync-to-heard-audio is a structural invariant; wall-clock drifts. |
| D-05 | wgpu + instanced quads + single atlas | Matches the workload (thousands of sprites); theming via instance colors. |
| D-06 | Two-mode input switching via layer-shell double-buffered state | No flicker, no focus hacks. |
| D-07 | Clean reimplementation, zero lazer code ported | Format spec is public; porting C#→Rust is more work than reimplementing, and keeps the story clean. |
| D-08 | Tool-maker legal posture; no hosting ever (v0.x non-goal) | No DMCA machinery needed; VLC/mpv category. |
| D-09 | Name must never contain "osu" | Trademark enforcement. "hyprbeat" is the product name (settled — see D-19). |
| D-10 | Classic timing/scoring formulas for v0.1 | Matches community expectations; lazer ruleset deferred. |
| D-11 | Single main thread for logic+render; RT audio separate | Simplicity; split render thread only if profiling demands. |
| D-12 | osu!standard mode only in v0.1 (`Mode: 0`) | Taiko/Catch/Mania = explicit non-goals for v0.1. |
| D-13 | SQLite deferred; JSON index for v0.1 | One less dep; migrate when library ops get slow. `[REVISITABLE]` at phase 8. |
| D-14 | Unix-socket control interface (`hyprbeat-ctl`) for keybind actions; socket at `$XDG_RUNTIME_DIR`, mode `0700`, `SO_PEERCRED` same-uid only | Hyprland binds shell out; app keeps state; local privilege separation. (Confirm in phase 7.) |
| D-15 | Autoplay demo + replays use **simulated input** through a single `InputSource` abstraction; direct judgement injection is unit-test-only | One judgement path (two score sources = phantom bugs); demo = real game played perfectly; input abstraction unlocks `.osr` replay playback later. |
| D-16 | Audio clock has an explicit **degraded wall-clock fallback** (no/lost audio device) | I-01 assumes a working stream; without a fallback phase 2 stalls on any PipeWire failure. Degraded state is HUD-visible, scores unranked. |
| D-17 | Output scale follows the compositor; user config exposes only `ui_scale` and a perf-only `[render] scale` override (clamped) | Wayland-correct: client-side "DPI" settings fight the compositor → blur / mis-sized rendering. |
| D-18 | Mouse speed = software sensitivity multiplier on pointer deltas (NOT a DPI setting); hardware DPI is the user's mouse config. Default `raw_input = true` bypasses compositor acceleration for predictable aim. Sensitivity applies only in PLAYING, never in wallpaper passthrough. | Wayland clients cannot set mouse DPI (that's hardware/libinput). What rhythm players want — "1 pad unit = N osu!px" — is a sensitivity multiplier; raw input removes inter-compositor aim inconsistency. |
| D-19 | Product name is `hyprbeat` (was working title "beatpaper"). Settled pre-code while the rename is free. | Satisfies D-09/I-04 (no "osu"). Aware of namespace overlap with Hyprland's `hyprpaper` (Vaxry's wallpaper daemon — different function: rhythm game vs static wallpaper); accepted the association as net-positive ecosystem fit. Do not re-litigate without a concrete conflict (trademark claim or documented user confusion). |
| D-20 | Phase-1 CLI is a minimal hand-rolled parser (`--headless --timeout --size --scale --out --testcard --probe --fps`); clap still lands in phase 5 with the config flags (§6) | §4 "keep the dependency tree small"; every phase-1 flag is a dev/diagnostic flag |
| D-21 | Live rendering = one wgpu surface per output created over raw Wayland handles; fractional scale = buffer `round(logical×scale)` mapped 1:1 onto logical size via `wp_viewporter` source/dest; without viewporter, fall back to `wl_surface.set_buffer_scale(round(scale))` + warn. The wgpu 30 instance is handed the `wl_display` (`InstanceDescriptor::display`) so the GLES backend can present on Wayland. | Pixel-correct fractional scaling (D-17/I-11); wgpu-30-specific requirement discovered during the phase-1 build |
| D-22 | Wallpaper surfaces default to an EMPTY input region + `keyboard_interactivity=none` (pointer passthrough, I-13); `--probe` temporarily maps full input + `on_demand` to measure Q-1/Q-2 from the live desktop | Phase 1 must never eat desktop input; the probe doubles as the §14 instrument |
| D-23 | Frame loop: render only inside frame callbacks; idle cap by skipping with an EMPTY commit (some compositors only fire callbacks after a commit); a 250 ms watchdog re-requests callbacks (occlusion ⇒ callbacks stop ⇒ ~0 % CPU is expected and logged once) | §3.5 made concrete + self-healing; whether the empty commits are actually needed on Hyprland is Q-3 |
| D-24 | Live mode defaults to a 120 s safety `--timeout` (0 disables; probe defaults 10 s) | §12.3 "a broken build can't strand the user"; retire the default in phase 7 when real mode switching exists |
| D-25 | Toolchain pinned at phase-1 scaffold: edition 2024 / rust-version 1.85; wgpu 30 (vulkan+gles features only), sctk 0.21.1, wayland-client 0.31 with `system`+`dlopen` (raw `wl_display`/`wl_surface` pointers for wgpu surface creation; libwayland dlopened at runtime), wayland-protocols 0.32 (`staging`+`unstable`), calloop 0.14 + calloop-wayland-source 0.4; `Cargo.lock` committed | §4 "pin versions at phase-1 scaffold"; wgpu 30 API deltas (present moved to `Queue`, sRGB targets want linear shader input) were absorbed during the build |
| D-26 | First frame after a configure is presented **eagerly** (request callback → present, in that order); the starvation watchdog distinguishes "never rendered" (present one — deadlock fallback) from "rendered before, starved" (re-arm by request+commit or a bare commit, never a forced second concurrent callback, never a buffer present — occlusion must stay ~0 cost) | Live finding 2026-11-12: Hyprland does not composite a layer surface that has never attached a buffer, so no frame callback ever arrives to trigger frame 1 (observed: rendered=0, watchdog_rearms=20 for 10 s); a bare `wl_surface.frame` re-request without a following commit arms nothing. Also removes the double-arm hazard: each on_frame_done begets exactly one request, so a forced second pending callback would persist forever. |
| D-27 | **[SUPERSEDED by D-28 — probe conclusions (b) and (c) were confounded artifacts; (a) stands]** Phase-1 input probe v1 (Hyprland 0.57.1): (a) background-layer `on_demand` keyboard interactivity is protocol-supported; (b) relative pointer delivers nothing → `raw_input = false`; (c) zero pointer events observed | Probe v1 ran with a terminal window on the desktop, which correctly captured all pointer/keyboard input (windows sit above the background layer). (a) was confirmed by run 3; (b)/(c) were measurement error |
| D-28 | Probe v2 (2026-11-12, Hyprland 0.57.1, EMPTY workspace): background-layer surfaces receive FULL input — `wl_pointer` enter/button/motion, `zwp_relative_pointer_v1` deltas (4596 events in 10 s), sustained keyboard delivery (65 key events under auto-granted focus). `raw_input = true` stays the v0.1 default as designed; no `Layer::Bottom` workaround needed. Protocol for all future input probes: run on an EMPTY workspace (windows above the background layer legitimately consume input — that IS the wallpaper-mode passthrough working). | Corrects D-27(b)/(c); best-case outcome for §6 `[input]` and the §3.4 T1 "click desktop → play" transition — both mechanisms confirmed deliverable |
| D-29 | Hyprland 0.57.1 does **not** stop frame callbacks to a background-layer surface under true fullscreen occlusion (run 6: EasyEffects, opaque, `fullscreen: 2` verified via `hyprctl activewindow`, waybar killed, 35 s) — callbacks continued at ~165 Hz (two reports dipped to ~132/106 Hz during the window's startup churn; never stopped), rendered=137/5 s, watchdog_rearms=0, CPU 0.8–1.0 % ≈ visible cost. §3.5 free-pause assumption falsified on this machine; §3.6 occluded budget amended to ≈ visible (≤ 2 %); real pause = phase-7 mode switch (stop requesting frames). Caveat: gpu-screen-recorder was running during the test (capture may force full-scene compositing — optional retest without it) | Run 6 of the §3.6 spot-checks; runs 1–5 hit confounds (maximize-variant keybind, translucent browser/kitty, one exit-127 no-op run) |

---

## 10. Build phases & status

| Phase | Scope | Exit criteria | Status |
|---|---|---|---|
| **0** | Architecture (this document) | CONTEXT.md complete | ✅ **DONE** |
| **1** | Cargo project, layer-shell surface, render loop, frame callbacks | Gradient/textured surface on Hyprland background, `--headless` PNG works, correct at 125% fractional scale | ✅ **DONE (2026-11-12, Hyprland 0.57.1)** — gradient live at 100 % (~27.4 fps @ 30 cap, 165 Hz output); test card sharp at 100 % AND 125 %; headless PNG confirmed; probes on an empty workspace: pointer enter/buttons, relative deltas (4596), sustained keys (65) — Q-1/Q-2/Q-6 PASS (D-28; D-27 superseded). 2 live render bugs fixed en route (D-26, frame-gate mix-up). §3.6 budgets: visible 0.7–1.0 % of one core across all runs (≤ 2 % — PASS), RAM 53 MB engine-only (≤ 150 MB — PASS), cold start ~0.3 s (≤ 2 s — PASS), occluded ≈ visible ≈ 1 % — the "~0 % free pause" is falsified on Hyprland 0.57.1 (D-29/Q-7; callbacks keep flowing under verified true fullscreen); real pause duty → phase 7 mode switch. |
| **2** | Audio engine: cpal callback, clock, decode cache, mixer, universal offset | Wallpaper animation synced to music; pause/rebase correct; degraded wall-clock path tested | ⬜ |
| **3** | `.osz`/`.osu` parse, golden tests, library index, hash dedup | Map loads; circles+timing visible on timeline (visualizer) | ⬜ |
| **4** | Hit detection, judgement windows, combo/HP/score, hitsounds; pointer input (`wl_pointer` + `zwp_relative_pointer_v1`, §6 `[input]`) | Fully playable circles-only map; score/Acc/HP live | ⬜ |
| **5** | Config system (§6 exactly), presets, hot reload, CLI flags | Edit config.toml live → theme/layout changes without restart | ⬜ |
| **6** | Sliders (§7.3), spinners, stacking, new-combo colours | A modern tech map plays correctly end-to-end | ⬜ |
| **7** | Mode switching, autoplay bot, `hyprbeat-ctl` socket, keybind docs | Click desktop → play; focus loss → pause; demo mode visible; socket `0700` + same-uid only | ⬜ |
| **8** | Song select UI, HUD polish, README, license, release tarball, **NixOS flake package output** (§12.1) | **v0.1.0** tag | ⬜ |

**Rules**: one phase per chat session. A phase may be split across sessions;
never merge two phases into one session if either is non-trivial. A phase is
DONE when its exit criteria pass AND the §3.6 budgets hold where measurable.

---

## 11. Invariants (must hold in every phase)

| ID | Invariant |
|---|---|
| **I-01** | Gameplay time comes ONLY from the audio clock (`Clock::time_ms`). `Instant::now()` is never used for gameplay time. |
| **I-02** | The audio callback never allocates, never locks, never panics. Only atomics and pre-allocated buffers. |
| **I-03** | The binary ships zero copyrighted bytes (audio, images, fonts bundled must be CC/PD/original; fonts: use a CC0 font e.g. Inter under OFL — check OFL terms, fine). |
| **I-04** | Product naming/branding never contains "osu". |
| **I-05** | Mode switching uses double-buffered layer-shell state; no window recreation, no flicker. |
| **I-06** | A broken `config.toml` can never crash or freeze the app: log, keep previous, keep rendering. |
| **I-07** | XDG paths only (`directories` crate). No writes outside config/data/cache dirs + temp. |
| **I-08** | Panic in any thread ≠ dead wallpaper: catch at thread boundary, log, keep the surface alive with last-good frame where feasible. |
| **I-09** | All core modules (parser, rules, logic, slider math, clock) are unit-testable with zero Wayland/audio I/O. |
| **I-10** | This file is updated at the end of every session: status table + new decisions appended. |
| **I-11** | Surface scale always follows the compositor (`scale = "auto"`); a manual `[render] scale` is a perf override only, clamped to output scale. |
| **I-12** | Degraded timing (wall clock) is always HUD-visible; silent timing degradation is a bug. |
| **I-13** | Mouse sensitivity (§6 `[input]`) affects only the in-playfield cursor during PLAYING; wallpaper/demo-mode pointer passthrough to the desktop is always 1:1, never scaled. |

---

## 12. Environment, testing & tooling

### 12.1 Development environment — **NixOS** (target machine is NixOS)

The dev machine runs NixOS + Hyprland. Implications:

- **No rustup.** Use the Rust toolchain from nixpkgs inside a flake devshell.
  Mixing rustup toolchains with nix-provided system libs is THE classic
  NixOS Rust link failure — don't do it.
- **Devshell** (`flake.nix` — written in phase 1 as the sketch below refined
  for the realized stack; syntax checked offline, `nix develop` still to be
  validated on the machine):

```nix
# flake.nix (devshell sketch — validate in phase 1)
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";   # adjust for aarch64 if needed
      pkgs = nixpkgs.legacyPackages.${system};
      libs = with pkgs; [
        wayland          # wl protocols/client headers (sctk build)
        libxkbcommon     # keymap handling
        vulkan-loader    # wgpu link + runtime
        alsa-lib         # cpal (cpal speaks ALSA directly on Linux)
      ];
    in {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          rustc cargo clippy rustfmt rust-analyzer
          pkg-config
        ] ++ libs;
        # wgpu/vulkan loader must be discoverable at runtime
        shellHook = ''
          export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath libs}:$LD_LIBRARY_PATH"
        '';
      };
    };
}
```

- **Runtime audio**: cpal opens ALSA devices; on a standard NixOS desktop,
  PipeWire's ALSA compatibility layer satisfies this (standard Hyprland
  setups already enable it). If stream-open fails: check
  `services.pipewire.alsa.*` in the NixOS config.
- **Runtime graphics**: Vulkan loader + Mesa drivers are already present on
  any Hyprland NixOS box (the compositor itself needs them) — the devshell
  `LD_LIBRARY_PATH` covers dev; the final packaged build wires it via
  `runtimeDependencies`.
- **QoL**: `direnv` + `nix-direnv` (`.envrc` = `use flake`) so the shell
  activates on `cd`.
- **Editor**: Helix/Neovim work natively (rust-analyzer from the shell or
  system). VSCode only via `vscode-fhs` (unwrapped VSCode breaks extensions
  on NixOS).
- **Debugging**: `gdb`/`lldb` from nixpkgs; `WAYLAND_DEBUG=1` works as-is;
  `RUST_LOG=hyprbeat=trace` for internal tracing.
- **Packaging (phase 8)**: ship a `packages.hyprbeat` flake output (build via
  `naersk`/`buildRustPackage` or crane) + optional Home-Manager module — for a
  NixOS user this is the natural distribution channel; also a plain `cargo
  build --release` tarball for everyone else.

### 12.2 Repo location (developer choice, not architectural)

The project folder location is irrelevant to the app: **all runtime state is
absolute XDG paths** (§5.2) and never relative to the repo. Conventions:
- Fine: `~/dev/hyprbeat`, `~/projects/hyprbeat`, anywhere except…
- **Never inside the app's own XDG dirs** (`~/.local/share/hyprbeat` is the
  *runtime library*, not the source tree — cloning the repo there would make
  Cargo outputs and song storage collide).
- Nothing in this document needs updating if the repo moves; do not
  hardcode repo paths in code or config.

### 12.3 General tooling

- **Rust**: stable, edition 2021 (bump to 2024 in phase 1 if toolchain current).
  *Phase-1 status: edition 2024 on rustc 1.99 — done.*
- **Dev loop on Hyprland**: `cargo run` in a TTY session or from a terminal —
  the surface appears behind everything immediately; kill with `hyprbeat-ctl
  quit` or `--timeout <s>` dev flag so a broken build can't strand the user
  (`--timeout` is implemented — default 120 s in live mode, 0 disables, D-24;
  phase-1 exit-criteria runner: `dev/validate-phase1.sh`). Also test on sway
  for layer-shell compat.
- **CI (phase 3+)**: `cargo fmt --check`, `cargo clippy -- -D warnings`,
  `cargo test` (parser + rules + slider math are pure — no GPU needed).
  *Phase-1 status: all three gates already pass offline (27 tests).*
- **Benchmarks** (phase 2+): criterion for clock math + slider LUT; manual
  latency measurement via `--latency-test` flag (tap-to-sound offset report).
- **Logging**: `tracing` + `tracing-subscriber`, `RUST_LOG=hyprbeat=debug`.
  Add to crate stack in phase 1.

---

## 13. Session handoff protocol

**Opening message for every new session** (paste verbatim):

> Read CONTEXT.md at the repo root completely. You are continuing the
> **hyprbeat** project — a Wayland layer-shell rhythm-game wallpaper client
> in Rust. Current phase is in §10. Follow the invariants in §11. Decisions in
> §9 are final unless marked `[REVISITABLE]`; do not re-open them. Begin.

**End of every session**:
1. Update the §10 status table (mark what's done, set NEXT).
2. Append any new decisions to §9 with one-line rationale.
3. Append any new invariants to §11 (rare).
4. Update "Last updated / Current phase" at the top.
5. Note open questions discovered during the session in a short
   `## 14. Open questions` running list (keep it, append-only, resolve by
   striking through with date).

**Hard rule**: if a session discovers a decision in §9 was wrong, it does not
silently change course — it marks the old decision `[SUPERSEDED by D-xx]`,
adds the new one, and updates dependent sections.

---

## 14. Open questions

- **Q-1 (verify phase 1)**: ~~does `on_demand` keyboard interactivity actually
  work on background-layer surfaces in Hyprland / sway?~~ **ANSWERED
  2026-11-12 (Hyprland 0.57.1): YES, fully** — enter/leave delivered, focus
  auto-granted at map on an empty workspace, 65 key events delivered while
  focus held (probe v2). §3.4 updated; no fallback needed.
- **Q-2 (verify phase 1)**: ~~does `zwp_relative_pointer_v1` deliver motion
  deltas to a background-layer surface with input region ON?~~ **ANSWERED
  2026-11-12 (Hyprland 0.57.1, empty workspace): YES** — 4596 relative-motion
  events in a 10 s probe. The run-2 "NO" was a confound (terminal window
  captured the pointer). `raw_input = true` stays the v0.1 default (D-28).
- **Q-3 (new, phase 1)**: ~~do frame callbacks keep arriving on Hyprland for a
  background surface that commits an EMPTY buffer between throttled frames?~~
  **ANSWERED 2026-11-12 (dev machine, Hyprland, 165 Hz output): YES** — run 2
  logged skipped=824 empty-commit skips over 5 s with callbacks continuous at
  ~165 Hz and watchdog_rearms=0. The empty-commit skip path works on Hyprland;
  keep it (it is the portability belt for compositors that do require a
  commit per callback). The D-26 half (never-attached buffer ⇒ no callbacks)
  stays recorded in the decision log.
- **Q-4 (new, phase 1)**: hybrid-GPU boxes — the adapter is currently picked
  without a compatible surface (phase-1 simplification, see `render::Gpu`
  docs); if validation ever shows a black surface on dGPU/iGPU setups, select
  the adapter after the first surface exists (sctk's wgpu example pattern).
- **Q-5 (new, 2026-11-12, validation run 1)**: ~~the user answered **"no"** to
  "does the headless test card look geometric?"~~ **RESOLVED same day:
  prompt misunderstanding** — the user read the question as being about the
  desktop background, not a file to open. On the rerun (prompt reworded:
  "open it in an image viewer") the answer was **"y"** — border, grid, rings,
  checker strip, sweep band all present and straight. No renderer defect;
  the RADV-produced PNG matches the committed reference.
- **Q-6 (new, 2026-11-12, probe v1)**: ~~do POINTER events (enter / button /
  absolute motion) reach a background-layer surface on Hyprland 0.57.1 at
  all?~~ **ANSWERED 2026-11-12 (probe v2, empty workspace): YES** — pointer
  entered, 25 buttons, continuous motion + relative deltas. Probe v1's
  zero-pointer result was the terminal-window confound (windows above the
  background layer legitimately consume input — that is wallpaper-mode
  passthrough working as designed). Lesson recorded in D-28: input probes
  must run on an empty workspace; `dev/validate-phase1.sh` instructions
  updated accordingly.
- **Q-7 (new, 2026-11-12, §3.6 spot-check run 6)**: ~~do frame callbacks stop
  arriving when an opaque window truly fullscreens over the background layer
  (the §3.5 "free pause")?~~ **ANSWERED 2026-11-12 (Hyprland 0.57.1): NO** —
  fullscreen verified (`fullscreen: 2`), waybar killed, opaque EasyEffects
  covering the output for 35 s: callbacks continued (~165 Hz; rendered=137
  per 5 s, watchdog_rearms=0), CPU stayed 0.8–1.0 %. Recorded as D-29;
  §3.5/§3.6 amended; real pause duty moved to phase 7. (gpu-screen-recorder
  was running; optional retest without it.)

---

*End of CONTEXT.md. If you are an AI session reading this: welcome back.
Check §10, do phase 1, keep the invariants, leave the file better than you
found it.*
