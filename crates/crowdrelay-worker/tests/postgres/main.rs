// Postgres-backed integration suites. One binary per crate: each test
// provisions its own cloned database through common::test_pool, so suites
// share a target without sharing state. CI and the just recipe run this
// target with --ignored; every test below stays #[ignore]d.

mod agent_decision_trace;
mod agent_outcome_guards;
mod agent_run_assignment;
mod bootstrap_team_idempotence;
mod city_geocoding;
mod common;
mod community_recovery;
mod community_relay_batch;
mod content_source_upsert;
mod growth_metric_sync_schedule;
mod import_opportunities;
mod import_outreach;
mod join_ask;
mod no_agent_service;
mod ops_watchdog;
mod osm_venue_sweep;
mod outbox_http;
mod outbox_materialization;
mod peer_observation;
mod publication_artifact;
mod receipt_reconciliation;
mod retention_approvals;
mod retention_outbox;
mod sheet_intake;
mod standing_approvals;
mod strategy_proposals;
mod ticketmaster_sweep;
