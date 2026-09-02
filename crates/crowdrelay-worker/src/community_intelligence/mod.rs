//! Community Intelligence — observation layer for community surfaces.
//!
//! This module implements the source adapter pipeline:
//! ```text
//! SourceAdapter.fetch()
//!       ↓
//! ParsedObservation
//!       ↓
//! ValidatedObservation (domain validation)
//!       ↓
//! Worker → Repository.insert_observation()
//! ```
//!
//! Sprint A implements one adapter (Brutalland). Sprint B will add
//! Metal Archives and Orbis Metallum adapters following the same trait.

pub mod adapter;
pub mod brutalland;
pub mod reddit;
pub mod worker;
