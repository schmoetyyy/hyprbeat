//! Q-1/Q-2 diagnostics (CONTEXT.md §14).
//!
//! `--probe` temporarily maps FULL input (input region left at default) and
//! `keyboard_interactivity = on_demand` on the background-layer surface, then
//! records what the compositor actually delivers:
//!
//! * Q-1 — does a background layer surface receive `wl_keyboard.enter`
//!   under `on_demand` interactivity on Hyprland/sway?
//!   (§3.4 fallback if not: Hyprland keybind + `hyprctl` focus dispatch.)
//! * Q-2 — does `zwp_relative_pointer_v1` deliver motion deltas to a
//!   background-layer surface with the input region on?
//!   (§6 `[input] raw_input` fallback: `raw_input = false` default.)
//!
//! This module is deliberately diagnostic-only; the real input stack lands in
//! phase 4 and mode switching in phase 7. Results print to stdout so a script
//! can grep them, and to the log.

use crate::cli::Args;

#[derive(Debug, Default)]
pub struct ProbeReport {
    pub keyboard_focus_gained: bool,
    pub keyboard_focus_lost: bool,
    pub key_events: u32,
    pub pointer_entered: bool,
    pub pointer_buttons: u32,
    pub relative_motion_events: u32,
    pub last_relative_delta: (f64, f64),
}

impl ProbeReport {
    pub fn print_summary(&self) {
        println!("\n=== hyprbeat phase-1 probe report (CONTEXT.md §14 Q-1/Q-2) ===");
        let q1 = if self.keyboard_focus_gained {
            "PASS — wl_keyboard.enter arrived (on_demand works on the background layer)"
        } else {
            "FAIL — no wl_keyboard.enter observed; use the §3.4 fallback (Hyprland keybind + hyprctl focus dispatch)"
        };
        let q2 = if self.relative_motion_events > 0 {
            "PASS — relative motion deltas received (raw_input = true is viable)"
        } else {
            "FAIL — no relative motion; ship v0.1 defaulting to raw_input = false"
        };
        println!("Q-1 keyboard on_demand on background layer : {q1}");
        println!("Q-2 zwp_relative_pointer_v1 deltas         : {q2}");
        println!(
            "    keyboard: focus_gained={} focus_lost={} key_events={}",
            self.keyboard_focus_gained, self.keyboard_focus_lost, self.key_events
        );
        println!(
            "    pointer : entered={} buttons={}",
            self.pointer_entered, self.pointer_buttons
        );
        println!(
            "    relative: events={} last_delta=({:.1}, {:.1})",
            self.relative_motion_events, self.last_relative_delta.0, self.last_relative_delta.1
        );
        println!("record the outcome in CONTEXT.md §14, then strike Q-1/Q-2 when decided.");
    }
}

pub fn print_instructions(args: &Args) {
    let seconds = args
        .timeout
        .map(|d| d.as_secs_f64())
        .unwrap_or(f64::INFINITY);
    println!(
        "hyprbeat probe running for {seconds:.0}s — while it runs:\n  \
         1. click on the exposed desktop (gives the layer surface keyboard focus),\n  \
         2. type a few keys,\n  \
         3. wiggle the mouse over the desktop.\n\
         A PASS/FAIL report prints at exit (Q-1/Q-2, CONTEXT.md §14)."
    );
}
