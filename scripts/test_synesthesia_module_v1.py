#!/usr/bin/env python3
"""Synesthesia optional-module contract.

Virya's interactive album becomes a per-tenant module. The gate must be:

- FAIL-CLOSED for workspaces without the flag row (new tenants start dark);
- BACKFILLED ON for every workspace that existed before gating (migration
  0112), so rollout is a no-op for the first tenant;
- ABSENT FROM FAN PRIVACY PATHS — data-rights actions stay reachable even
  when the module is dark;
- DEFAULT-OFF in FLAG_KEYS so a fresh workspace does not expose the album.
"""
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

GATE = ROOT / "crates/crowdrelay-api/src/synesthesia_gate.rs"
SYNESTHESIA = ROOT / "crates/crowdrelay-api/src/synesthesia.rs"
ECOSYSTEM = ROOT / "crates/crowdrelay-api/src/ecosystem.rs"
MIGRATION = ROOT / "migrations/0112_synesthesia_optional_module.sql"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"


class SynesthesiaModuleContract(unittest.TestCase):
    def test_gate_is_fail_closed(self):
        # The middleware answers 404 when disabled...
        self.assertIn("problem_not_found", GATE.read_text())
        # ...and the flag lookup itself fails closed on a missing row.
        eco = ECOSYSTEM.read_text()
        self.assertIn("Ok(None) => false", eco)
        self.assertIn("failing closed", eco)

    def test_backfill_enables_only_preexisting_workspaces(self):
        migration = MIGRATION.read_text()
        self.assertIn("FROM workspaces", migration)
        self.assertIn("'synesthesia_module', true", migration)
        self.assertIn("ON CONFLICT DO NOTHING", migration)

    def test_flag_default_off_for_new_workspaces(self):
        eco = ECOSYSTEM.read_text()
        self.assertIn('("synesthesia_module", false)', eco)
        self.assertIn("FLAG_KEYS: [(&str, bool); 17]", eco)

    def test_public_surface_is_gated_and_privacy_is_not(self):
        gated = SYNESTHESIA.read_text()
        self.assertIn("gated_public_router", gated)
        self.assertIn(
            "require_synesthesia_module", gated,
            "the fan-facing router must sit behind the module gate",
        )
        routing = ROUTING.read_text()
        # Privacy action stays ungated and mounted directly.
        self.assertIn("/v1/me/synesthesia/leaderboard", routing)
        self.assertIn("unpublish_synesthesia_leaderboard", routing)
        # Nothing else may mount the gated router outside the application tree.
        self.assertIn(".merge(synesthesia::gated_public_router(&state))", routing)

    def test_admin_surface_stays_outside_the_gate(self):
        routing = ROUTING.read_text()
        self.assertNotIn(
            "/v1/admin/synesthesia",
            routing.split("gated_public_router")[0],
        )


if __name__ == "__main__":
    unittest.main()
