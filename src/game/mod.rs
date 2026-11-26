//! Phase-4 game core (CONTEXT.md §10 row 4, module path per §5).
//!
//! Judgement, scoring and HP are pure, I/O-free state machines over
//! caller-supplied song time (I-09): the Wayland input pipeline (later
//! commits) feeds taps and clock ticks into [`GameplayCore`] and never
//! the other way around — the same one-way design as the visualizer's
//! `Timeline` model. `src/audio`, `src/wayland` and `src/render` are
//! untouched by commit (a); live wiring lands with commits b–d.
//!
//! Commit (a) implements Δ-P (simplified notelock), Δ-Q (HP stand-ins
//! plus the fail latch), Δ-R (active drain windows), Δ-S (ScoreV1 pins)
//! and Δ-T (inferred breaks) of the phase-4 Δ registry in CONTEXT.md.

pub mod judge;
pub mod score;
pub mod session;

pub use judge::{Judgement, Windows, circle_radius_osupx, fade_in_ms, judge_delta, preempt_ms};
pub use score::{ScoreState, difficulty_multiplier};
pub use session::{BREAK_GAP_MS, CircleNote, DrainPlan, GameplayCore, Tap};
