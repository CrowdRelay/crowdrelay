# Signup promise → welcome → deliberate activation

The v2 foundation is merged in #461. This follow-up finishes its tested rollout path and fixes an SQL parameter-type ambiguity found by executing the observer against PostgreSQL/PGlite. It also fixes doubled expression prefixes in the lifecycle workflow's claim/report URLs. No provider action is executed by these tests.

## What the system delivers

- New welcome actions name `crowdrelay.fan.welcome.v2`. Old pending v1 requests keep their original copy; historical v1 actions suppress a replacement v2 request.
- The immutable initial fan capture context selects a tenant-owned video or an upcoming published concert. Cancelled/past shows, future/invalid video identifiers and other tenants' resources are excluded. An unavailable promised resource falls back to a verified resource of the same kind. Without a suitable public resource or configured tenant site, the welcome remains honest and linkless.
- The CTA is one action-owned `/l/welcome-...` redirect. The exact resource and URL ride the committed outbox payload; the executor does not reconstruct a route, invent a benefit, ask for an install or demand a referral. Backend consent and action attempt guards remain binding.
- Only a live executor advertising `fan.lifecycle.welcome.v2` can serve new welcomes, including before any registry exists. Missing capability produces an honest recommendation, not repeated doomed sends. It can become executable after capability registration.
- The success receipt schedules a separate binary `fan_lifecycle_activation_7d` observation under `lifecycle_deliberate_activation_v2`, with a separate welcome template identity. It uses the existing deliberate-engagement helper and current consent, clamped to the observed time. Confirmation, sessions, clicks and push endpoint creation are excluded. Ticket/merch purchases, qualified referrals, check-ins, redeemed passes, event interest and non-synthetic Synesthesia completion can count. Several actions by one fan still yield 1.
- This is an observational per-fan outcome, not incremental acquisition, causal lift or proof of inbox delivery. Old metrics, observations and checkpoints are untouched.

## Rollout without a startup deadlock

Apply migration 0401 before the worker can write the new measurement kind. Deploy the upgraded lifecycle workflow through the existing attestation/provider gates. Add optional `template_capabilities=fan.lifecycle.welcome.v2` to the existing lifecycle row of the private production manifest, keeping its original event capability. The generated smoke template asks for `welcomeActivation`; its passing evidence must be bound to the exact exported workflow. The heartbeat builder rejects v1 smoke or a template capability attached to another route, and derives the additional capability through the regular independent heartbeat. Do not wait for the first action's claim to bootstrap the capability. Empty/absent optional columns preserve current manifests.

No production manifest, attestation, grant, provider credential or deployment is changed here. No email is sent.

## Reproduction

```sh
node --test scripts/check-welcome-activation.mjs
python scripts/test_n8n_heartbeat_builder.py
python scripts/test_n8n_workflow_attestation.py
python scripts/test_lifecycle_template_contract_v1.py
python scripts/test_executor_capability_parity_v1.py
python scripts/test_autopilot_measurement_coverage_v1.py
validation_dir=$(mktemp -d)
npm install --prefix "$validation_dir" --no-save @electric-sql/pglite@0.5.8
CROWDRELAY_VALIDATION_NODE_ROOT="$validation_dir" node scripts/check-welcome-activation-sql.mjs
CROWDRELAY_VALIDATION_NODE_ROOT="$validation_dir" node scripts/check-lifecycle-episodes.mjs
```

Verified here: 7 workflow tests, 23 Python tests including DB vocabulary alignment, 25 new PostgreSQL/PGlite assertions and 32 existing lifecycle SQL assertions. The SQL probes execute the actual source queries and migration-0397 helper with reduced fixtures. Native migrated PostgreSQL tests remain necessary to verify all schema constraints and Rust integration. Rust sources were parsed/formatted with actual rustfmt WASM; changed files satisfy size limits and the diff passes whitespace checks.

Added native regressions exercise missing/legacy/upgraded executors, v1→v2 replay suppression, the promised resource/action owner, consent withdrawal and real event-interest activation. An application test pins welcome v2 and Observe authority. Native cargo/rustc and a native disposable database are unavailable here, so compilation/clippy/native test execution are not claimed.

```sh
cargo test -p crowdrelay-application lifecycle_tests
cargo test -p crowdrelay-infra --test postgres autopilot_standing_approval -- --ignored
```

After authorized deployment, follow a real publication's signup and confirmation on a second device, the resulting provider receipt, the resource redirect, and a real deliberate engagement event. Repeated cycles/retries must not create another welcome. Verify the old 20 fans are not reclassified as new CrowdRelay acquisitions. Production growth is not established by this offline proof.
