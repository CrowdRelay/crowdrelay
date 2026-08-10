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

pub mod autopilot;
pub mod bootstrap;
pub mod draws;
pub mod event_sync;
pub mod outbox;
pub mod reminders;
pub mod retention;
