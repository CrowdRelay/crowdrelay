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
pub mod attestation;
pub mod audience_graph;
pub mod autopilot;
pub mod band_listing;
pub mod beacon_signal;
pub mod booking_agents;
pub mod commerce;
pub mod commerce_inventory;
pub mod community_intelligence;
pub mod concert_qr;
pub mod config;
pub mod content_arcs;
pub mod content_engine;
pub mod content_peers;
pub mod content_suggestions;
pub mod content_trends;
pub mod cross_tenant_priors;
pub mod database;
pub mod ecosystem;
pub mod events;
pub mod fan_identity;
pub mod fan_import;
pub mod fan_lifecycle;
pub mod fan_privacy;
pub mod fanbase;
pub mod gdrive;
pub mod gig_outreach;
pub mod gig_planning;
pub mod lapsed_approvals;
pub mod latarnik;
pub mod measurement_queries;
pub mod mobile_fan;
pub mod night;
pub mod observability;
pub mod organization_settings;
pub mod place_reach;
pub mod portfolio;
pub mod proofs;
pub mod provider_verification;
pub mod push_preferences;
pub mod reddit_proxy;
pub mod referrals;
pub mod regional;
pub mod representation;
pub mod roster_act_report;
pub mod roster_catalogue_rotation;
pub mod roster_counterparty_archive;
pub mod roster_overview;
pub mod roster_portfolio;
pub mod roster_release_calendar;
pub mod roster_source_roi;
pub mod roster_weekly_brief;
pub mod sensitive_response;
pub mod sent_record;
pub mod show_helpers;
pub mod signal_installations;
pub mod tenant_settings;
pub mod venue_directory;
pub mod venue_seed;
