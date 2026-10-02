#!/usr/bin/env python3
"""FAN SCOUT canonical prospect-spine contract."""

from pathlib import Path

R = Path(__file__).resolve().parents[1]
migration = (R / "migrations/0406_reconcile_fan_scout_prospect_spines.sql").read_text()
infra = (R / "crates/crowdrelay-infra/src/fan_prospects.rs").read_text()
domain = (R / "crates/crowdrelay-domain/src/fan_prospect.rs").read_text()
domain_lib = (R / "crates/crowdrelay-domain/src/lib.rs").read_text()
infra_lib = (R / "crates/crowdrelay-infra/src/lib.rs").read_text()

assert "legacy_0404_scout_prospects" in migration
assert "platform_user_id" in migration
assert "fan_prospects_one_per_person_platform" in migration
assert "DROP INDEX IF EXISTS fan_prospects_one_per_handle" in migration

assert "pub platform_user_id: Option<&'a str>" in infra
assert "person_id=$2 AND platform=$3" in infra
assert "identifier.verified_at IS NOT NULL" in infra
assert "fan.status='active'" in infra
assert "prospect.status NOT IN ('refused','suppressed')" in infra
assert "INSERT INTO fans" not in infra

assert "normalize_platform_user_id" in domain
assert "pub mod fan_scout;" not in domain_lib
assert "pub mod fan_scout;" not in infra_lib

print("FAN_SCOUT_CANONICAL_PROSPECT_SPINE=PASS")
