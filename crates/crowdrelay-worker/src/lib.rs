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
pub mod autopilot;
pub mod bootstrap;
pub mod community_executor;
pub mod community_intelligence;
pub mod discovery;
pub mod draws;
pub mod event_sync;
pub mod growth_metric_sync;
pub mod leadership;
pub mod ops_watchdog;
pub mod outbox;
pub mod push_delivery;
pub mod receipt_reconciliation;
pub mod reminders;
pub mod replay;
pub mod retention;
