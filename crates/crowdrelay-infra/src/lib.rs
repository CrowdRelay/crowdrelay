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
pub mod audience_graph;
pub mod autopilot;
pub mod band_listing;
pub mod beacon_signal;
pub mod commerce;
pub mod commerce_inventory;
pub mod community_intelligence;
pub mod concert_qr;
pub mod config;
pub mod content_arcs;
pub mod content_engine;
pub mod content_suggestions;
pub mod content_trends;
pub mod database;
pub mod ecosystem;
pub mod events;
pub mod fan_identity;
pub mod fan_import;
pub mod fan_lifecycle;
pub mod fan_privacy;
pub mod fanbase;
pub mod gdrive;
pub mod mobile_fan;
pub mod observability;
pub mod portfolio;
pub mod proofs;
pub mod provider_verification;
pub mod push_preferences;
pub mod reddit_proxy;
pub mod referrals;
pub mod regional;
pub mod representation;
pub mod sensitive_response;
pub mod signal_installations;
pub mod tenant_settings;
