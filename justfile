# CrowdRelay task runner — replaces the previous Makefile 1:1.
# `just --list` shows everything; `just <recipe>` runs it.

set shell := ["bash", "-uc"]

CARGO := env_var_or_default("CARGO", "cargo")
COMPOSE := env_var_or_default("COMPOSE", "docker compose")
API_BASE_URL := env_var_or_default("API_BASE_URL", "http://127.0.0.1:8080/v1")

PG_URL := "postgres://crowdrelay:crowdrelay-local-only@127.0.0.1:5432/crowdrelay_autopilot_test"

# Every ignored Postgres integration test, against a disposable database.
# Creates and migrates the database itself; safe to re-run at any time.
_test_pg_env := '''
export CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL="{{PG_URL}}"
export CROWDRELAY_TEST_DATABASE_URL="{{PG_URL}}"
export CROWDRELAY_ADMISSION_TEST_DATABASE_URL="{{PG_URL}}"
export CROWDRELAY_ECOSYSTEM_TEST_DATABASE_URL="{{PG_URL}}"
export CROWDRELAY_EVENT_TEST_DATABASE_URL="{{PG_URL}}"
export CROWDRELAY_FAN_LIFECYCLE_TEST_DATABASE_URL="{{PG_URL}}"
export CROWDRELAY_MOBILE_FAN_TEST_DATABASE_URL="{{PG_URL}}"
export CROWDRELAY_REFERRAL_TEST_DATABASE_URL="{{PG_URL}}"
'''

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
    python3 scripts/test-modularity-contract.py
    python3 scripts/test_platform_vocabulary_v1.py
    python3 scripts/test_sql_identifiers_v1.py
    python3 scripts/test_alert_policy_v1.py
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

# Everything a push should have passed
ci: check validate-contract-assets contract-tests policy-checks

# The #[ignore]d Postgres integration tests against a disposable database.
# Creates and migrates the database itself; safe to re-run at any time.
test-postgres-env:
    #!/usr/bin/env bash
    set -euo pipefail
    export CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL=postgres://crowdrelay:crowdrelay-local-only@127.0.0.1:5432/crowdrelay_autopilot_test
    export CROWDRELAY_TEST_DATABASE_URL=$CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL
    export CROWDRELAY_ADMISSION_TEST_DATABASE_URL=$CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL
    export CROWDRELAY_ECOSYSTEM_TEST_DATABASE_URL=$CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL
    export CROWDRELAY_EVENT_TEST_DATABASE_URL=$CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL
    export CROWDRELAY_FAN_LIFECYCLE_TEST_DATABASE_URL=$CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL
    export CROWDRELAY_MOBILE_FAN_TEST_DATABASE_URL=$CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL
    export CROWDRELAY_REFERRAL_TEST_DATABASE_URL=$CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL
    # The outbox, reminder and retention suites live in unit-test modules rather
    # than their own integration targets. CI exports these three and runs them by
    # name; the local recipe exported neither, so widening it to the workspace
    # surfaced three failures that were really a missing variable.
    export CROWDRELAY_OUTBOX_TEST_DATABASE_URL=$CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL
    export CROWDRELAY_REMINDER_TEST_DATABASE_URL=$CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL
    export CROWDRELAY_RETENTION_TEST_DATABASE_URL=$CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL
    {{COMPOSE}} up --detach --wait postgres
    {{COMPOSE}} exec -T postgres psql -U crowdrelay -d postgres \
        -c "DROP DATABASE IF EXISTS crowdrelay_autopilot_test;" \
        -c "CREATE DATABASE crowdrelay_autopilot_test;"
    # The recipe dropped and recreated the database but never migrated it, so
    # every test that did not migrate itself failed on a missing column and the
    # suite could not pass on a clean checkout. Same `setup` entrypoint CI uses;
    # it is idempotent, so re-running the recipe stays safe. `setup` validates
    # the full runtime config before touching the database, so the tenant
    # variables below are required even though migrating uses none of them.
    export CROWDRELAY_DATABASE_URL=$CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL
    export CROWDRELAY_ENV=test
    export CROWDRELAY_BIND_ADDR=127.0.0.1:8080
    export CROWDRELAY_ALLOWED_ORIGINS=http://localhost:4321
    export CROWDRELAY_PUBLIC_SITE_BASE_URL=http://localhost:4321
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
    export CROWDRELAY_DATABASE_MAX_CONNECTIONS=5
    export CROWDRELAY_BOOTSTRAP_JSON='{"workspace_name":"CrowdRelay local test","cities":[{"slug":"wroclaw","name":"Wroclaw","country":"PL","region":"Dolnoslaskie","lat":51.1079,"lng":17.0385}],"campaigns":[],"webhook_endpoints":[]}'
    {{CARGO}} run --locked --all-features --package crowdrelay-worker -- setup
    # Every crate, not just crowdrelay-infra. This recipe ran
    # `-p crowdrelay-infra` alone while CI globs `crates/*/tests/*_postgres.rs`
    # across the workspace, so it reported green while CI ran tests it had never
    # executed -- crowdrelay-worker's postgres targets (outbox, city geocoding,
    # metric sync schedule, agent decision trace) were all invisible here. A
    # local gate narrower than CI is worse than none: it teaches you to trust it.
    #
    # One invocation per target, exactly as CI does. Not a formality: several
    # suites share the one database rather than creating a disposable copy, and
    # claims like `claim_deliveries` are workspace-wide, so two suites live at
    # once makes one claim a row the other left pending. Running them together
    # is stricter than CI and fails on that coupling alone. The coupling is
    # worth fixing; reproducing CI is what this recipe is for.
    targets=()
    for path in crates/*/tests/*_postgres.rs; do
      package="$(basename "$(dirname "$(dirname "$path")")")"
      targets+=("${package}:$(basename "$path" .rs)")
    done
    if [ "${#targets[@]}" -eq 0 ]; then
      echo "no *_postgres.rs integration targets found; the glob is wrong" >&2
      exit 1
    fi
    printf 'running %s integration targets\n' "${#targets[@]}"
    for entry in "${targets[@]}"; do
      {{CARGO}} test --locked --package "${entry%%:*}" --test "${entry##*:}" \
        -- --ignored --test-threads=1
    done
    # The outbox, reminder and retention suites live in unit-test modules rather
    # than their own integration target, so the glob above cannot see them.
    for filter in \
      postgres_outbox_round_trip \
      due_reminder_is_enqueued_exactly_once \
      cycle_deletes_expired_rows_scrubs_safe_payloads_and_preserves_audit
    do
      {{CARGO}} test --locked --package crowdrelay-worker "$filter" -- --ignored --test-threads=1
    done

# Alias kept for muscle memory from the Makefile days
test-postgres: test-postgres-env

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
