//! hyprbeat — an open-source rhythm game that lives on your desktop
//! background, compatible with the osu! beatmap format. Built for
//! Hyprland/Wayland.
//!
//! This library facade exists so `benches/` (criterion) and external tests
//! can reach the modules; the binary in `main.rs` is a thin shell over them
//! (§12.3 benches, phase 2).
//!
//! Phase status (CONTEXT.md §10): 1–2 DONE, 3 in progress — beatmap parser
//! (`beatmap`) landed first; .osz import + timeline visualizer next.

pub mod audio;
pub mod beatmap;
pub mod cli;
pub mod config;
pub mod render;
pub mod wayland;

/// Crate-wide error type. Phase 1 keeps it dependency-free (§4); a dedicated
/// error enum can replace it when the config parser lands in phase 5.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
