//! Community Intelligence — observation layer for community surfaces.
//!
//! This module implements the source adapter pipeline:
//! ```text
//! SourceAdapter.fetch()
//!       ↓
//! ParsedObservation ── entities ────────────────→ community_entities
//!       │                metrics ───────────────→ community_observations
//!       └── items (dated posts fans engaged with) → fan_observations
//!       ↓
//! Worker → Repository.insert_observation()
//! ```
//!
//! Adapters today: Brutalland (forum index) and Reddit (through the agent
//! service's logged-in browser — the only read path Reddit left open).

pub mod adapter;
pub mod brutalland;
pub mod reddit;
pub mod worker;
