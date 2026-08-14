#!/usr/bin/env python3
from __future__ import annotations
import pathlib

from rust_source_tree import read_rust_module
import unittest

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
        compose_path = ROOT / "compose.oracle.yaml"
        if not compose_path.exists():
            self.skipTest(
                "compose.oracle.yaml is a private operator file and is not tracked in git"
            )
        compose = compose_path.read_text()
        service = compose.split("  rekor-proof-anchor:", 1)[1].split("\nsecrets:", 1)[0]
        self.assertIn("condition: service_healthy", service)
        self.assertIn("read_only: true", service)
        self.assertIn("cap_drop: [ALL]", service)
        self.assertIn("rekor-anchor-state:/data", service)
        self.assertNotIn("\n    ports:", service)

    def test_ghcr_publishes_relayer(self):
        workflow = (ROOT / ".github/workflows/publish-images.yml").read_text()
        self.assertIn("crowdrelay-rekor-proof-anchor:sha-${{ env.IMAGE_SHA }}", workflow)
        self.assertIn("context: proofs/rekor-anchor/relayer", workflow)

    def test_relayer_image_contains_batch_runtime_and_writable_journal_directory(self):
        dockerfile = (ROOT / "proofs/rekor-anchor/relayer/Dockerfile").read_text()
        runtime = (ROOT / "proofs/rekor-anchor/relayer/index.mjs").read_text()
        self.assertIn("batch-runner.mjs", dockerfile)
        self.assertIn("chown -R node:node /app /data", dockerfile)
        self.assertIn("chmod 700 /data", dockerfile)
        self.assertIn("await verifyPendingStorage()", runtime)
        self.assertIn("pending storage is not writable", runtime)

    def test_canary_restores_previous_flag_and_pins_exact_api_build(self):
        text = (ROOT / "scripts/rekor-canary.py").read_text()
        self.assertIn('previous_flag_state = current_flag_state(client)', text)
        self.assertIn('set_flag(\n                    client,\n                    previous_flag_state', text)
        self.assertIn('require_exact_api_build(client, expected_git_sha)', text)
        self.assertIn('require_no_processing_batches(client, "preflight")', text)
        self.assertIn('require_no_processing_batches(client, "post-confirm")', text)
        self.assertIn('verify_rekor_entry(anchor_url, entry_id)', text)
        self.assertIn('external_proof_anchoring_enabled', text)

    def test_installer_checks_private_relayer_before_and_after_canary(self):
        installer = (ROOT / "ops/rekor/install-anchor.sh").read_text()
        self.assertIn('CROWDRELAY_EXPECTED_GIT_SHA="$expected_git_sha"', installer)
        self.assertGreaterEqual(installer.count('/health/ready'), 2)
        self.assertGreaterEqual(installer.count('/data/pending-confirmation.json'), 2)
        self.assertIn('pending confirmation journal remains after confirmed canary', installer)

    def test_installer_requires_immutable_image_and_private_api_path(self):
        installer = (ROOT / "ops/rekor/install-anchor.sh").read_text()
        env_example = (ROOT / "deploy/rekor-anchor.env.example").read_text()
        self.assertIn("^sha-[0-9a-f]{40,64}$", installer)
        self.assertIn("CROWDRELAY_INTERNAL_URL=http://crowdrelay-api:8080", env_example)
        self.assertIn("private Docker API endpoint", installer)

    def test_inventory_ready_is_atomic(self):
        text = read_rust_module(ROOT, "crates/crowdrelay-api/src/commerce.rs")
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
