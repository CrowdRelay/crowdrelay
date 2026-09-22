// Postgres-backed integration suites. One binary per crate: each test
// provisions its own cloned database through common::test_pool, so suites
// share a target without sharing state. CI and the just recipe run this
// target with --ignored; every test below stays #[ignore]d.

mod attestation_anchor;
mod common;
