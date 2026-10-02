#!/usr/bin/env python3
"""FAN SCOUT prospect-spine contract."""

from pathlib import Path

R = Path(__file__).resolve().parents[1]
migration = (R / "migrations/0404_fan_scout_prospect_spine.sql").read_text()
infra = (R / "crates/crowdrelay-infra/src/fan_scout.rs").read_text()
domain = (R / "crates/crowdrelay-domain/src/fan_scout.rs").read_text()

assert "CREATE TABLE scout_prospects" in migration
assert "CREATE TABLE scout_prospect_observations" in migration
assert "linked_fan_id" in migration
assert "UNIQUE (workspace_id, platform, identity_kind, identity_key)" in migration
assert "INSERT INTO fans" not in migration
assert "INSERT INTO fans" not in infra

assert "ON CONFLICT (workspace_id, platform, identity_kind, identity_key)" in infra
upsert = infra.split("ON CONFLICT (workspace_id, platform, identity_kind, identity_key)", 1)[1]
upsert = upsert.split("RETURNING id", 1)[0]
assert "status =" not in upsert

assert "identifier.verified_at IS NOT NULL" in infra
assert "fan.status = 'active'" in infra
assert "prospect.status NOT IN ('refused', 'suppressed')" in infra
assert "status = 'converted'" in infra

assert "PlatformUserId" in domain
assert "Handle" in domain
assert "raw.to_lowercase()" in domain
assert "raw.to_owned()" in domain

print("FAN_SCOUT_PROSPECT_SPINE=PASS")
