CARGO ?= cargo
COMPOSE ?= docker compose
API_BASE_URL ?= http://127.0.0.1:8080/v1

.PHONY: fmt lint test check validate-contract-assets contract-tests runtime-contracts ci deploy env db-up migrate bootstrap setup build-images build-arm64 up down logs ps health

fmt:
	$(CARGO) fmt --all

lint:
	$(CARGO) clippy --locked --workspace --all-targets --all-features -- -D warnings

test:
	$(CARGO) test --locked --workspace --all-targets --all-features

check: fmt lint test

validate-contract-assets:
	node --disable-warning=ExperimentalWarning --experimental-strip-types scripts/validate-contract-assets.ts

contract-tests:
	python3 -m unittest discover -s scripts -p 'test_*.py'

runtime-contracts:
	bash scripts/audit-public-tree.sh
	python3 scripts/check-ci-policy.py
	python3 scripts/source-size-ratchet.py
	python3 scripts/api-sql-ratchet.py
	python3 scripts/test-modularity-contract.py
	python3 scripts/check-postgres-major.py
	python3 scripts/postgres18_runtime_contract.py
	python3 scripts/area_wallet_authority_v2_contract.py
	python3 scripts/staff_device_sessions_v2_contract.py
	python3 scripts/test-ecosystem-contract-v2.py
	python3 scripts/test-ops-control-plane-v2.py
	python3 scripts/test-ecosystem-design-contract.py
	python3 scripts/test-image-provenance-policy.py

ci: check validate-contract-assets contract-tests runtime-contracts

deploy:
	bash scripts/deploy.sh

env:
	@test -f .env || cp .env.example .env
	@echo "Using .env (development defaults are copied only when it is missing)."

db-up: env
	$(COMPOSE) up --detach --wait postgres

migrate: db-up
	$(COMPOSE) run --rm --build migrate migrate

bootstrap: db-up
	$(COMPOSE) run --rm --build migrate bootstrap

setup: db-up
	$(COMPOSE) run --rm --build migrate setup

build-images:
	docker buildx bake --load

build-arm64:
	API_IMAGE=crowdrelay-worker:arm64 WORKER_IMAGE=crowdrelay-worker:arm64 \
		docker buildx bake --set '*.platform=linux/arm64' --load

up: env
	$(COMPOSE) up --build --detach

health:
	curl --fail --silent --show-error "$(API_BASE_URL)/health/live"
	@echo
	curl --fail --silent --show-error "$(API_BASE_URL)/health/ready"
	@echo

down:
	$(COMPOSE) down

logs:
	$(COMPOSE) logs --follow --tail=200

ps:
	$(COMPOSE) ps
