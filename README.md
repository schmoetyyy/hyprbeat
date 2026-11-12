# hyprbeat

**An open-source rhythm game that lives on your desktop background, compatible
with the osu! beatmap format. Built for Hyprland/Wayland.**

hyprbeat renders on your compositor's background layer via the wlr layer
shell. Click the exposed desktop and the wallpaper becomes a playable rhythm
game; focus another window and it pauses; leave it idle and an autoplay demo
bot keeps your desktop alive. No servers, no accounts, no hosted content.

> **Status — phase 1 of 8** (see [`CONTEXT.md`](CONTEXT.md) §10): the
> foundation is in place — layer-shell background surface, frame-callback
> driven render loop, fractional-scale-correct wgpu rendering, `--headless`
> offscreen rendering. **Gameplay, audio, beatmaps and the config file arrive
> in phases 2–8.** What you get today is an animated gradient wallpaper with
> a scale test card.

![phase-1 headless test card at 125% scale](docs/img/headless-testcard-125.png)

*A headless `--testcard` render: logical 1920×1080 at 1.25× scale becomes a
2400×1350 buffer; the 2 px border, 1 px grid, rings and checker strip stay
locked to logical pixels.*

## Requirements

- Linux + a [wlr-layer-shell](https://gitlab.freedesktop.org/wlroots/wlr-protocols/-/blob/master/unstable/wlr-layer-shell-unstable-v1.xml)
  compositor: **Hyprland**, sway or niri
- A Vulkan or GLES driver (any Mesa stack; software lavapipe works too)
- Rust ≥ 1.85 (edition 2024)

## Build & run

### NixOS (primary target)

```sh
nix develop          # devshell: toolchain + wayland/libxkbcommon/vulkan-loader/alsa-lib
cargo run --release -- --testcard --timeout 10
```

### Other distros

System deps: `wayland`, `libxkbcommon`, `vulkan-loader` (+ `pkg-config` at
build time), then:

```sh
cargo build --release
./target/release/hyprbeat --testcard --timeout 10
```

Every live run carries a **safety timeout** (default 120 s, `--timeout 0`
disables) so a broken build can never strand your desktop. This default is
a phase-1 development guard and will retire when real mode switching lands
(phase 7).

## CLI (phase-1 dev/diagnostic set)

```
--headless          render one frame offscreen to a PNG and exit (no compositor)
--size WxH          headless logical size [default: 1920x1080]
--scale SCALE       headless simulated output scale [default: 1.0]
--out PATH          headless PNG output path
--testcard          draw the scale test pattern over the gradient
--probe             CONTEXT.md §14 Q-1/Q-2: check on_demand keyboard focus and
                    relative-pointer deltas on a background-layer surface
--fps FPS           idle frame-rate cap [default: 30]
--timeout SECS      quit after SECS seconds; 0 disables [default: 120]
--version / --help
```

Examples:

```sh
hyprbeat --testcard --timeout 10                    # watch the background
hyprbeat --headless --scale 1.25 --out /tmp/card.png
hyprbeat --probe --timeout 8                        # input diagnostics
RUST_LOG=hyprbeat=debug hyprbeat --timeout 0        # verbose run
```

## Phase-1 validation (dev machine)

Run [`dev/validate-phase1.sh`](dev/validate-phase1.sh) on the Hyprland
machine — it walks the exit criteria: devshell build, headless PNG, live
gradient, test card at 100 % and 125 % (`hyprctl output <name> scale 1.25`),
and the Q-1/Q-2 input probes.

Quality gates that run anywhere: `cargo fmt --check`,
`cargo clippy -- -D warnings`, `cargo test` (27 unit tests; parser/rules
tests grow per phase — all pure modules are I/O-free by design).

## Architecture

The single source of truth is [`CONTEXT.md`](CONTEXT.md) — threading model,
the audio-clock invariant, the two-mode state machine, decision log and
build phases. Quick sketch:

- **smithay-client-toolkit** + **calloop** on the main thread (no winit —
  its layer-shell support is still unmerged)
- one **wgpu** surface per output on the background layer; instanced-quad
  rendering arrives with gameplay sprites (phase 4+)
- frame-callback driven rendering with an idle fps cap; occlusion pause is
  compositor-dependent — Hyprland keeps sending callbacks under fullscreen
  (measured ~1 % of one core; a real pause arrives with phase-7 mode
  switching)
- fractional scaling via `wp_fractional_scale_v1` + `wp_viewporter`:
  buffer = `round(logical × scale)`, mapped 1:1 back onto logical pixels
- empty input region by default (desktop keeps every click); input handling
  is phase 4, mode switching phase 7

## Legal

This is a standalone tool. We do not host or distribute copyrighted
beatmaps; what users do with their own files is their responsibility.
hyprbeat is an independent reimplementation of publicly documented mechanics
and file formats — it is **not** affiliated with osu! or ppy, contains zero
code from osu!lazer, and the product name deliberately never contains
"osu".

Bundled assets (once they exist) will be CC-licensed or original only.

## License

MIT — see [LICENSE](LICENSE).
