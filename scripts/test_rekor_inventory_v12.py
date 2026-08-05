#!/usr/bin/env python3
from __future__ import annotations
import pathlib
import unittest
import yaml

ROOT = pathlib.Path(__file__).resolve().parents[1]


class RekorInventoryV12Tests(unittest.TestCase):
    def test_rekor_readiness_starts_closed_and_checks_dependencies(self):
        text = (ROOT / "proofs/rekor-anchor/relayer/index.mjs").read_text()
        self.assertIn('let ready = false', text)
        self.assertIn('/v1/health/ready', text)
        self.assertIn('/api/v1/log/publicKey', text)
        self.assertIn('dependencies,', text)
        self.assertNotIn('let healthy = true', text)

    def test_oracle_compose_has_isolated_relayer(self):
        data = yaml.safe_load((ROOT / "compose.oracle.yaml").read_text())
        service = data["services"]["rekor-proof-anchor"]
        self.assertEqual(service["depends_on"]["api"]["condition"], "service_healthy")
        self.assertTrue(service["read_only"])
        self.assertEqual(service["cap_drop"], ["ALL"])
        self.assertIn("rekor-anchor-state:/data", service["volumes"])
        self.assertNotIn("ports", service)

    def test_ghcr_publishes_relayer(self):
        workflow = (ROOT / ".github/workflows/publish-images.yml").read_text()
        self.assertIn("crowdrelay-rekor-proof-anchor:sha-${{ env.IMAGE_SHA }}", workflow)
        self.assertIn("context: proofs/rekor-anchor/relayer", workflow)

    def test_canary_rolls_back_flag(self):
        text = (ROOT / "scripts/rekor-canary.py").read_text()
        self.assertIn('set_flag(client, False', text)
        self.assertIn('verify_rekor_entry(anchor_url, entry_id)', text)
        self.assertIn('external_proof_anchoring_enabled', text)

    def test_inventory_ready_is_atomic(self):
        text = (ROOT / "crates/crowdrelay-api/src/commerce.rs").read_text()
        update_position = text.index("SET status = 'ready'")
        flags_position = text.index("inventory activated from staff panel")
        commit_position = text.index("transaction.commit()", flags_position)
        self.assertLess(update_position, flags_position)
        self.assertLess(flags_position, commit_position)
        self.assertIn('blocker == "feature_flags_inconsistent"', text)

    def test_catalog_seed_has_22_skus_and_no_stock(self):
        migration = (ROOT / "migrations/0028_inventory_onboarding.sql").read_text()
        self.assertEqual(migration.count("('echoes', 'VIRYA-CD-ECHOES'"), 1)
        self.assertEqual(migration.count("'VIRYA-TEE-"), 20)
        self.assertEqual(migration.count("'VIRYA-BAG-CREST'"), 1)
        seed_start = migration.index("WITH seed(slug, name")
        self.assertNotIn("INSERT INTO inventory_ledger", migration[seed_start:])

    def test_n8n_not_referenced_by_rollout_scripts(self):
        for relative in [
            "ops/rekor/install-anchor.sh",
            "ops/rekor/rollback-anchor.sh",
            "scripts/rekor-canary.py",
        ]:
            text = (ROOT / relative).read_text().lower()
            self.assertNotIn("n8n", text)


if __name__ == "__main__":
    unittest.main()
