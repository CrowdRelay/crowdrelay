//! Tables this worker reads but another service owns.
//!
//! `agent_service_tasks` is created by the agent service, not by a CrowdRelay
//! migration (`FOREIGN_RELATIONS` in `scripts/test_sql_identifiers_v1.py`).
//! Production shares one database with the agent service, so the table is
//! there. A tenant stack without an agent service — the demo-label and
//! demo-roster stacks, or any tenant not yet given one — has no such table.
//! Every statement naming it fails with `undefined_table`.
//!
//! The executors join it only to turn finished agent drafts into posts. With
//! no agent service there are no drafts, so the honest answer is "nothing to
//! materialise", not an error. The failure also aborted the enclosing
//! transaction, which took the claim of already-pending posts down with it,
//! and it logged three warnings a minute per stack, around three thousand a
//! day, burying every warning that mattered.

/// Whether `error` is PostgreSQL's `undefined_table` (42P01).
pub(crate) fn is_undefined_table(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|database| database.code().as_deref() == Some("42P01"))
}
