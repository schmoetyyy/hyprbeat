//! Configuration (§5 module layout).
//!
//! Phase 1 ships only the theme seed — the palette types and presets that
//! drive the ambient wallpaper gradient. The full hot-reloaded TOML config
//! system lands in phase 5 (§6); `Color` and `Palette` here are designed to be
//! reused verbatim by it (hex parsing already fails loudly, the posture I-06
//! requires).

pub mod theme;
