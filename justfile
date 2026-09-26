# CrowdRelay task runner — replaces the previous Makefile 1:1.
# `just --list` shows everything; `just <recipe>` runs it.

set shell := ["bash", "-uc"]

CARGO := env_var_or_default("CARGO", "cargo")
COMPOSE := env_var_or_default("COMPOSE", "docker compose")
API_BASE_URL := env_var_or_default("API_BASE_URL", "http://127.0.0.1:8080/v1")

[private]
default:
    @just --list

# Format all Rust code
fmt:
    {{CARGO}} fmt --all

# Clippy across the workspace, warnings denied
lint:
    {{CARGO}} clippy --locked --workspace --all-targets --all-features -- -D warnings

# Unit and integration tests that need no external services
test:
    {{CARGO}} test --locked --workspace --all-targets --all-features

# fmt + lint + test
check: fmt lint test

# Static validation of contract assets (openapi shape, route manifests)
@validate-contract-assets:
    node --disable-warning=ExperimentalWarning --experimental-strip-types scripts/validate-contract-assets.ts

# The source-reading contract suite: ~937 assertions over 150+ scripts.
# `unittest discover` imports every `test_*.py`, so module-level assert
# scripts run too — that is how most of these are written.
@contract-tests:
    python3 -m unittest discover -s scripts -p 'test_*.py'

# Security, schema and deployment checks that complement compiler-backed tests.
# Scripts whose names `unittest discover` cannot match (hyphens, or no `test_`
# prefix) must be listed here explicitly or they never run.
@policy-checks:
    bash scripts/audit-public-tree.sh
    python3 scripts/check-ci-policy.py
    python3 scripts/source-size-ratchet.py
    python3 scripts/api-sql-ratchet.py
    python3 scripts/workspace-scope-ratchet.py
    python3 scripts/sql-result-types.py
    python3 scripts/test_sql_row_shapes_v1.py
    python3 scripts/test_sql_check_vocabulary_v1.py
    python3 scripts/test_sql_typed_params_v1.py
    python3 scripts/test_serialized_dates_v1.py
    python3 scripts/test-modularity-contract.py
    python3 scripts/test_platform_vocabulary_v1.py
    python3 scripts/test_sql_identifiers_v1.py
    python3 scripts/test_sql_scalar_types_v1.py
    python3 scripts/test_sql_columns_v1.py
    python3 scripts/test_alert_policy_v1.py
    python3 scripts/test_audience_segment_filters_v1.py
    python3 scripts/test_operator_reachability_v1.py
    python3 scripts/test_bluegreen_recovery_v1.py
    python3 scripts/check-postgres-major.py
    python3 scripts/postgres18_runtime_contract.py
    python3 scripts/area_wallet_authority_v2_contract.py
    python3 scripts/staff_device_sessions_v2_contract.py
    python3 scripts/test-ecosystem-contract-v2.py
    python3 scripts/test-ops-control-plane-v2.py
    python3 scripts/test-ecosystem-design-contract.py
    python3 scripts/test-image-provenance-policy.py
    python3 scripts/test_release_receipt.py
    python3 scripts/test_ecosystem_deploy_contract.py

# Browse the OpenAPI contract as Redoc, on this machine only.
#
# Binds 127.0.0.1 and re-reads openapi/openapi.yaml on every request, so editing
# the spec and refreshing the browser shows the change. Redoc found a duplicated
# `components.parameters` key that `validate-contract-assets` had reported clean
# for as long as it existed — reading the rendered contract is worth doing.
docs PORT="8088":
    cargo run --quiet --package crowdrelay-docs -- {{PORT}}

# The workspace rustdoc tree that `just docs` serves at /rustdoc/.
# --document-private-items is deliberate: this map exists for navigating the
# internals, and the interesting machinery is pub(crate), not pub.
rustdoc *ARGS:
    cargo doc --no-deps --document-private-items {{ARGS}}

# The dark-mode system reference PDF, measured from this tree.
#
# Stage one reads the repository and writes HTML; stage two prints it through the
# Chromium that ../crowdrelay-agents installs for Playwright, which is the only
# CSS-to-PDF renderer on this machine. Output: ../CrowdRelay-System-Reference.pdf
reference:
    python3 ops/docs/build_reference.py
    node ops/docs/render_reference.mjs

# Everything a push should have passed
ci: check validate-contract-assets contract-tests policy-checks

# The #[ignore]d Postgres integration tests against ONE disposable database.
# One database, one pool per test binary, every test scoped by its own
# workspace; the trap drops the database on success AND on failure so a run
# never leaves a pending test database behind.
test-postgres-env:
    #!/usr/bin/env bash
    set -euo pipefail
    # A per-run name keeps a parallel session's suite out of this one —
    # foreign writes land in their own database, never in this run's rows.
    pgdb="cr_pg_suite_$$_$RANDOM"
    url="postgres://crowdrelay:crowdrelay-local-only@127.0.0.1:5432/${pgdb}"
    # Admin SQL goes through whatever postgres container publishes 5432 — the
    # shared dev container or this project's own — so a worktree never spawns
    # a second postgres competing for the port.
    pgc() { docker ps -qf publish=5432 | head -1; }
    pg_admin() { docker exec -i "$(pgc)" psql -U crowdrelay -d postgres -qAt "$@"; }
    cleanup() {
      pg_admin -c "DROP DATABASE IF EXISTS \"${pgdb}\" WITH (FORCE)" >/dev/null 2>&1 || true
    }
    trap cleanup EXIT
    export CROWDRELAY_DATABASE_URL="$url"
    export CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL="$url"
    export CROWDRELAY_TEST_DATABASE_URL="$url"
    export CROWDRELAY_ADMISSION_TEST_DATABASE_URL="$url"
    export CROWDRELAY_ECOSYSTEM_TEST_DATABASE_URL="$url"
    export CROWDRELAY_EVENT_TEST_DATABASE_URL="$url"
    export CROWDRELAY_FAN_LIFECYCLE_TEST_DATABASE_URL="$url"
    export CROWDRELAY_MOBILE_FAN_TEST_DATABASE_URL="$url"
    export CROWDRELAY_REFERRAL_TEST_DATABASE_URL="$url"
    export CROWDRELAY_OUTBOX_TEST_DATABASE_URL="$url"
    export CROWDRELAY_REMINDER_TEST_DATABASE_URL="$url"
    export CROWDRELAY_RETENTION_TEST_DATABASE_URL="$url"
    export CROWDRELAY_COMMUNITY_TEST_DATABASE_URL="$url"
    export CROWDRELAY_AGENTS_TEST_DATABASE_URL="$url"
    # `setup` validates the full runtime config before touching the database,
    # so the tenant variables below are required even though migrating uses
    # none of them.
    export CROWDRELAY_ENV=test
    export CROWDRELAY_BIND_ADDR=127.0.0.1:8080
    export CROWDRELAY_ALLOWED_ORIGINS=http://localhost:4321
    export CROWDRELAY_PUBLIC_SITE_BASE_URL=http://localhost:4321
    export CROWDRELAY_PUBLIC_ORIGIN=http://localhost:4321
    export CROWDRELAY_WORKSPACE_SLUG=example
    export CROWDRELAY_DEFAULT_COUNTRY_CODE=PL
    export CROWDRELAY_TENANT_REGION=eu
    export CROWDRELAY_TENANT_LOCALE=pl-PL
    export CROWDRELAY_TENANT_TIMEZONE=Europe/Warsaw
    export CROWDRELAY_TENANT_CURRENCY=PLN
    export CROWDRELAY_TENANT_DATE_FORMAT=dmy
    export CROWDRELAY_TENANT_NUMBER_FORMAT=comma_decimal
    export CROWDRELAY_TENANT_DATA_REGION=eu
    export CROWDRELAY_RANDOM_DRAWS_ENABLED=false
    export CROWDRELAY_DATABASE_MAX_CONNECTIONS=4
    export CROWDRELAY_BOOTSTRAP_JSON='{"workspace_name":"CrowdRelay local test","cities":[{"slug":"wroclaw","name":"Wrocław","country":"PL","region":"Dolnoslaskie","lat":51.1079,"lng":17.0385}],"campaigns":[],"webhook_endpoints":[]}'
    # The compose file requires an env_file that exists; worktrees do not
    # carry the gitignored .env, so fall back to the tracked example.
    export CROWDRELAY_ENV_FILE="${CROWDRELAY_ENV_FILE:-$( [ -f .env ] && echo .env || echo .env.example )}"
    # Start postgres only when nothing already serves 5432 — a worktree reuses
    # the dev container instead of fighting it for the port.
    if [ -z "$(pgc)" ]; then {{COMPOSE}} up --detach --wait postgres; fi
    pg_admin -c "DROP DATABASE IF EXISTS \"${pgdb}\" WITH (FORCE);" \
        -c "CREATE DATABASE \"${pgdb}\";"
    {{CARGO}} run --locked --all-features --package crowdrelay-worker -- setup
    # The e2e proofs, each once, serially, on the one database — the same set
    # CI runs. The remaining #[ignore]d tests stay runnable on demand:
    # `cargo test -p <crate> --test postgres -- --ignored <name>`.
    {{CARGO}} test --locked --all-features --package crowdrelay-infra --test postgres -- --ignored --test-threads=1 \
      click_writes_interaction_and_signup_links_it_to_the_fan \
      fan_arrival_writes_provenance_not_only_acquisition \
      single_action_attribution_is_exact \
      unlabelled_link_records_interaction_but_never_converts \
      what_left_and_what_never_did \
      a_seed_sheet_lands_as_attributed_facts \
      a_workspace_without_email_gets_no_assignments \
      an_empty_org_and_a_silent_act_both_render \
      a_silence_resolves_to_a_measured_zero \
      b_a_reply_inside_the_window_resolves_to_one \
      queued_team_assignment_email_uses_fast_lane_and_emits_bridge_event \
      a_far_out_approval_is_held_for_the_briefing \
      an_anchor_is_one_room_whoever_claims_it
    {{CARGO}} test --locked --all-features --package crowdrelay-worker --test postgres -- --ignored --test-threads=1 \
      the_outcome_appears_in_the_timeline_it_caused \
      the_drip_claims_one_post_per_batch_per_interval \
      signed_http_delivery_is_exact_and_durable \
      a_late_executor_report_is_not_duplicated \
      a_second_pass_changes_nothing \
      the_sweep_reaches_every_workspace
    {{CARGO}} test --locked --all-features --package crowdrelay-infra --test peer_act_seed_postgres -- --ignored --test-threads=1 \
      a_band_sheet_lands_as_attributed_peer_facts
    {{CARGO}} test --locked --all-features --package crowdrelay-infra --test team_reminder_drain_postgres -- --ignored --test-threads=1 \
      the_drain_clears_schedules_without_mailing
    {{CARGO}} test --locked --all-features --package crowdrelay-infra --test workspace_secrets_postgres -- --ignored --test-threads=1 \
      seals_reveals_lists_and_deletes \
      secrets_do_not_cross_workspaces
    {{CARGO}} test --locked --all-features --package crowdrelay-api --test postgres -- --ignored --test-threads=1 \
      an_attestation_is_anchored_once_and_the_anchor_says_so \
      a_mailed_link_renders_then_decides_the_ask \
      material_counts_expired_sources_as_aged_out_and_folds_song_copies \
      a_city_night_nobody_measured_reads_null_not_zero
    {{CARGO}} test --locked --all-features --package crowdrelay-worker --lib -- --ignored --test-threads=1 \
      postgres_outbox_round_trip
    {{CARGO}} test --locked --all-features --package crowdrelay-api --lib -- --ignored --test-threads=1 \
      archive_confirmation_is_not_organic_growth

# Alias kept for muscle memory from the Makefile days
test-postgres: test-postgres-env

# The CrowdRelay <-> crowdrelay-agents boundary, at runtime.
#
# Starts a disposable database and the REAL agents service from the sibling
# checkout, then drives CrowdRelay's own executors and HTTP clients across a
# real network hop. This is deliberately not part of `just ci`: it needs a
# second repository and a Node toolchain, and it takes about a minute because
# the hung-dependency test waits out the executor's real 60s request timeout.
#
# Skips cleanly when ../crowdrelay-agents is not checked out.
test-agents-boundary:
    #!/usr/bin/env bash
    set -euo pipefail
    agents=../crowdrelay-agents
    if [ ! -d "$agents" ]; then
      echo "no crowdrelay-agents checkout beside this repository; skipping"
      exit 0
    fi
    export CROWDRELAY_AGENTS_TEST_DATABASE_URL=postgres://crowdrelay:crowdrelay-local-only@127.0.0.1:5432/crowdrelay_agents_boundary_test
    export CROWDRELAY_AGENTS_TEST_AUTH_KEY=agents-boundary-suite-master-key
    export CROWDRELAY_AGENTS_TEST_URL=http://127.0.0.1:18095
    {{COMPOSE}} up --detach --wait postgres
    {{COMPOSE}} exec -T postgres psql -U crowdrelay -d postgres \
        -c "DROP DATABASE IF EXISTS crowdrelay_agents_boundary_test;" \
        -c "CREATE DATABASE crowdrelay_agents_boundary_test;"
    # The agents service migrates its own tables on startup, so it has to run
    # against the same database the tests read. Tickers off: this suite is
    # about the request path, and a scraper tick would drive a real browser.
    #
    # AGENT_SERVICE_ALLOW_LEGACY_TOKENS is deliberately left unset. Its
    # default is what the capability test asserts; setting it here would make
    # the suite test a configuration nothing deploys.
    (
      cd "$agents"
      AGENT_SERVICE_BIND=127.0.0.1:18095 \
      DATABASE_URL="$CROWDRELAY_AGENTS_TEST_DATABASE_URL" \
      AGENT_SERVICE_AUTH_KEY="$CROWDRELAY_AGENTS_TEST_AUTH_KEY" \
      AGENT_SERVICE_ENCRYPTION_KEY=00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff \
      AGENT_SCHEDULER_ENABLED=false \
      REDDIT_SCRAPER_ENABLED=false \
      AGENT_OUTCOMES_ENABLED=false \
      npx tsx src/server.ts
    ) > /tmp/crowdrelay-agents-boundary.log 2>&1 &
    agents_pid=$!
    trap 'kill $agents_pid 2>/dev/null || true' EXIT
    for _ in $(seq 1 60); do
      if curl -fsS -m 2 -o /dev/null "$CROWDRELAY_AGENTS_TEST_URL/health"; then break; fi
      sleep 1
    done
    if ! curl -fsS -m 2 -o /dev/null "$CROWDRELAY_AGENTS_TEST_URL/health"; then
      echo "the agents service never became healthy; see /tmp/crowdrelay-agents-boundary.log" >&2
      tail -30 /tmp/crowdrelay-agents-boundary.log >&2
      exit 1
    fi
    {{CARGO}} test --locked --package crowdrelay-worker \
        --test agents_boundary_postgres -- --ignored --test-threads=1

# Copy .env.example if .env is missing
@env:
    @test -f .env || cp .env.example .env
    @echo "Using .env (development defaults are copied only when it is missing)."

# Start the Postgres service only
db-up: env
    {{COMPOSE}} up --detach --wait postgres

# Apply migrations to the compose database
migrate: db-up
    {{COMPOSE}} run --rm --build migrate migrate

# Migrations plus first-workspace bootstrap
bootstrap: db-up
    {{COMPOSE}} run --rm --build migrate bootstrap

# Full local setup
setup: db-up
    {{COMPOSE}} run --rm --build migrate setup

# Build production images for both architectures
build-images:
    docker buildx bake --load

# Build arm64 images locally
build-arm64:
    API_IMAGE=crowdrelay-api:arm64 WORKER_IMAGE=crowdrelay-worker:arm64 \
        docker buildx bake --set '*.platform=linux/arm64' --load

# Full local stack, rebuilt
up: env
    {{COMPOSE}} up --build --detach

down:
    {{COMPOSE}} down

logs:
    {{COMPOSE}} logs -f

ps:
    {{COMPOSE}} ps

# Liveness/readiness probe against the local stack
@health:
    #!/usr/bin/env bash
    set -euo pipefail
    for path in health/live health/ready; do
      code=$(curl --silent --output /dev/null --write-out '%{http_code}' "{{API_BASE_URL}}/$path")
      echo "$path -> $code"
      [[ "$code" == "200" ]]
    done

# Deploy CrowdRelay alone via the release script
deploy:
    bash scripts/deploy.sh

# Does not wait for GitHub Actions — this machine is the build host. Requires a
# clean worktree on main that is already pushed, because the deployed revision
# has to be the one anyone else can check out.
# Gates, native arm64 build, push, digest-pinned blue-green. Deploys production.
ship *ARGS:
    CROWDRELAY_DEPLOY_IMAGE_SOURCE=local bash scripts/deploy.sh {{ARGS}}

# For when the gates already passed in this tree a moment ago. Skips them —
# nothing else validates the revision before it reaches production.
ship-nogate *ARGS:
    CROWDRELAY_DEPLOY_IMAGE_SOURCE=local CROWDRELAY_LOCAL_GATES=true \
        bash scripts/deploy.sh {{ARGS}}

# Every component ships its own origin/main; a stale or dirty checkout aborts
# before anything mutates.
# Deploy the whole stack: CrowdRelay, Control Plane, agent service
deploy-ecosystem *ARGS:
    bash scripts/deploy-ecosystem.sh {{ARGS}}

# Every pre-deploy gate, no mutations — run this before `deploy-ecosystem`.
deploy-ecosystem-check *ARGS:
    bash scripts/deploy-ecosystem.sh --dry-run {{ARGS}}

# Roll the stack back to a previously deployed 40-char SHA.
deploy-ecosystem-rollback sha:
    bash scripts/deploy-ecosystem.sh --rollback {{sha}}

# Deploy all active tenants with a runtime from the Mac. Fails fast on first error.
ship-fleet *ARGS:
    bash scripts/ship-fleet.sh {{ARGS}}
