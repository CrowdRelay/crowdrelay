# CrowdRelay task runner — replaces the previous Makefile 1:1.
# `just --list` shows everything; `just <recipe>` runs it.

set shell := ["bash", "-uc"]

default := "check"

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

# Python contract suite (source-reading policy pins)
@contract-tests:
    python3 -m unittest discover -s scripts -p 'test_*.py'

# Runtime contract scripts executed against repository source
@runtime-contracts:
    bash scripts/audit-public-tree.sh
    python3 scripts/check-ci-policy.py
    python3 scripts/source-size-ratchet.py
    python3 scripts/api-sql-ratchet.py
    python3 scripts/test-modularity-contract.py
    python3 scripts/test_audience_graph_v1.py
    python3 scripts/test_label_portfolio_v1.py
    python3 scripts/test_tenant_settings_v1.py
    python3 scripts/test_pilot_import_v1.py
    python3 scripts/test_synesthesia_module_v1.py
    python3 scripts/test_tenant_settings_http_v1.py
    python3 scripts/test_discovery_reddit_v1.py
    python3 scripts/check-postgres-major.py
    python3 scripts/postgres18_runtime_contract.py
    python3 scripts/area_wallet_authority_v2_contract.py
    python3 scripts/staff_device_sessions_v2_contract.py
    python3 scripts/test-ecosystem-contract-v2.py
    python3 scripts/test-ops-control-plane-v2.py
    python3 scripts/test-ecosystem-design-contract.py
    python3 scripts/test-image-provenance-policy.py

# Everything a push should have passed
ci: check validate-contract-assets contract-tests runtime-contracts

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
    {{COMPOSE}} up --detach --wait postgres
    {{COMPOSE}} exec -T postgres psql -U crowdrelay -d postgres \
        -c "DROP DATABASE IF EXISTS crowdrelay_autopilot_test;" \
        -c "CREATE DATABASE crowdrelay_autopilot_test;"
    {{CARGO}} test --locked -p crowdrelay-infra --tests -- --ignored

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

# Deploy via the release script
deploy:
    bash scripts/deploy.sh
