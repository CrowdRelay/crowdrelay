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

//! Background processing owned by the CrowdRelay worker binary.
//!
//! Contains the idempotent workspace bootstrap command, the transactional
//! outbox worker for signed webhook delivery, and the durable event reminder
//! scheduler.

pub mod ad_conversion;
pub mod agent_outcomes;
pub mod attribution;
pub mod audience_graph;
pub mod auto_post_platforms;
pub mod autopilot;
pub mod backfill_outreach_verdicts;
pub mod bootstrap;
pub mod city_geocoding;
pub mod community_executor;
pub mod community_intelligence;
pub mod community_join_executor;
pub mod community_vetting;
pub mod discord_executor;
pub mod discovery;
pub mod draws;
pub mod event_sync;
pub mod executor_registry;
pub mod fan_source_snapshot;
mod foreign_relation;
pub mod gdrive_contacts_sync;
pub mod github_registry_sync;
pub mod gmail_contacts_sync;
pub mod gmail_outreach_ledger;
pub mod gmail_sightings;
pub mod google_oauth;
pub mod growth_metric_sync;
pub mod growth_readiness;
pub mod import_opportunities;
pub mod import_outreach;
pub mod leadership;
pub mod nearby_gigs;
pub mod ops_watchdog;
pub mod osm_venue_sweep;
pub mod outbox;
pub mod peer_observation;
pub mod push_delivery;
pub mod receipt_reconciliation;
pub mod release_source_sync;
pub mod reminders;
pub mod replay;
pub mod retention;
pub mod sheet_intake;
pub mod social_post_executor;
pub mod social_post_source_sync;
pub mod telegram_executor;
pub mod ticketmaster_sweep;
pub(crate) mod tracked_link_text;
pub mod venue_fact_expiry;
pub mod video_source_sync;
pub mod youtube_replies;
