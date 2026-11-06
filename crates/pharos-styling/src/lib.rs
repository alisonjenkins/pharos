//! Pharos-styled animated subtitles (spec 028, ADR-0067).
//!
//! A pure crate: no I/O, no async, no randomness. An [`plan::EffectPlan`]
//! says what each source line should look like; the compiler turns plan,
//! source lines, theme, device tier and effects level into ASS v4+ text.
//! The same inputs always give the same bytes.

pub mod budget;
pub mod compile;
mod effects;
pub mod event;
pub mod plan;
pub mod source;
mod tags;
mod text;
pub mod theme;
pub mod version;

pub use compile::compile;
pub use plan::{EffectPlan, EffectsLevel, Intent, Karaoke, LineEffect, Placement, Tier};
pub use version::{COMPILER_VERSION, DIRECTOR_VERSION};
