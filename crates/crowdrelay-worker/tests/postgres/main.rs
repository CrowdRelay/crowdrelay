// Postgres-backed integration suites. One binary per crate: each test
// provisions its own cloned database through common::test_pool, so suites
// share a target without sharing state. CI and the just recipe run this
// target with --ignored; every test below stays #[ignore]d.

mod agent_decision_trace;
mod agent_outcome_guards;
mod agent_run_assignment;
mod beacon_candidate_scout;
mod bootstrap_team_idempotence;
mod city_geocoding;
mod common;
mod community_cooldown;
mod community_draft_language;
mod community_drip_order;
mod community_recovery;
mod community_relay_batch;
mod community_relay_revisions;
mod community_replies_lane;
mod community_tracked_links;
mod contact_research_outcome;
mod contact_research_sweep;
mod content_source_upsert;
mod growth_metric_sync_schedule;
mod import_opportunities;
mod import_outreach;
mod join_ask;
mod latarnik_sweep;
mod no_agent_service;
mod ops_watchdog;
mod osm_venue_sweep;
mod outbox_http;
mod outbox_materialization;
mod owned_social_channel;
mod peer_observation;
mod prospect_sweep;
mod proactive_fan_prospect_outcome;
mod publication_artifact;
mod receipt_reconciliation;
mod retention_approvals;
mod retention_outbox;
mod room_reading_gate;
mod sheet_intake;
mod sheet_intake_scout;
mod standing_approvals;
mod strategy_proposals;
mod ticketmaster_sweep;
mod video_promotion_links;
mod video_release_plan;
mod zz_wire_dates;
