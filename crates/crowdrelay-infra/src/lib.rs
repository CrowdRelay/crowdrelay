#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used,
    )
)]
#![deny(clippy::dbg_macro)]

//! Infrastructure shared by the CrowdRelay API and worker.
//!
//! Contains PostgreSQL repository implementations for all application ports,
//! environment-based configuration, database pool lifecycle, and structured
//! tracing initialization.

pub mod acquisition;
pub mod admission;
pub mod area_admin;
pub mod autopilot;
pub mod config;
pub mod database;
pub mod ecosystem;
pub mod events;
pub mod fan_lifecycle;
pub mod fan_privacy;
pub mod observability;
pub mod proofs;
pub mod referrals;
pub mod sensitive_response;
