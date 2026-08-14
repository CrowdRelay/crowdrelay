#!/usr/bin/env python3
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "production_readiness", ROOT / "scripts/verify-production-readiness.py"
)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


def code_component(key: str, seed: str = "d", manifest: bool = False):
    value = {
        "component_key": key,
        "source_sha": seed * 40,
        "artifact_digest": "sha256:" + seed * 64,
        "dependency_lock_sha256": seed * 64,
        "artifact_manifest_sha256": seed * 64 if manifest else None,
        "observed_at": "2026-08-14T09:00:00Z",
        "stale": False,
    }
    return value


def ledger(**overrides):
    value = {
        "executor_manifest_drift": False,
        "backend_sha_drift": False,
        "n8n_attestation_ready": True,
        "active_team_email_executor_count": 1,
        "team_email_live": True,
        "missing_components": [],
        "components": [
            code_component("crowdrelay-api", "1"),
            code_component("crowdrelay-worker", "1"),
            code_component("virya-www", "2", True),
            code_component("synesthesia", "3", True),
            code_component("virya-signal", "4", True),
            {
                "component_key": "n8n",
                "source_sha": "b" * 64,
                "manifest_sha": "b" * 64,
                "workflow_attestation_sha": "c" * 64,
                "workflow_attested_at": "2026-08-14T09:00:00Z",
                "observed_at": "2026-08-14T09:00:00Z",
                "stale": False,
            },
        ],
    }
    value.update(overrides)
    return value


class ProductionReadinessTest(unittest.TestCase):
    def test_workflow_skips_until_the_production_endpoint_is_configured(self):
        workflow = (ROOT / ".github/workflows/production-readiness.yml").read_text()
        self.assertIn("if: ${{ vars.CROWDRELAY_PRODUCTION_BASE_URL != '' }}", workflow)

    def test_live_team_email_requires_attest_manifest_executor_and_provenance(self):
        failures, receipt = MODULE.evaluate(ledger())
        self.assertEqual(failures, [])
        self.assertEqual(receipt["status"], "pass")
        self.assertTrue(receipt["teamEmail"]["live"])
        self.assertEqual(receipt["components"]["virya-www"]["dependencyLockSha256"], "2" * 64)

    def test_desired_state_without_live_executor_fails_closed(self):
        failures, receipt = MODULE.evaluate(
            ledger(active_team_email_executor_count=0, team_email_live=False)
        )
        self.assertIn("team-email-executor-not-live", failures)
        self.assertIn("team-email-not-live", failures)
        self.assertEqual(receipt["status"], "fail")

    def test_manifest_drift_cannot_be_reported_ready(self):
        failures, _ = MODULE.evaluate(
            ledger(executor_manifest_drift=True, team_email_live=False)
        )
        self.assertIn("executor-manifest-drift", failures)

    def test_backend_drift_missing_or_stale_component_fails(self):
        data = ledger(backend_sha_drift=True, missing_components=["synesthesia"])
        data["components"] = [item for item in data["components"] if item["component_key"] != "synesthesia"]
        data["components"][0]["stale"] = True
        failures, receipt = MODULE.evaluate(data)
        self.assertIn("backend-sha-drift", failures)
        self.assertIn("release-components-missing", failures)
        self.assertIn("crowdrelay-api-stale", failures)
        self.assertIn("synesthesia", receipt["missingComponents"])

    def test_missing_content_root_provenance_fails_closed(self):
        data = ledger()
        signal = next(item for item in data["components"] if item["component_key"] == "virya-signal")
        signal["artifact_digest"] = None
        signal["dependency_lock_sha256"] = None
        signal["artifact_manifest_sha256"] = None
        failures, _ = MODULE.evaluate(data)
        self.assertIn("virya-signal-artifact-digest-missing", failures)
        self.assertIn("virya-signal-dependency-lock-missing", failures)
        self.assertIn("virya-signal-artifact-manifest-missing", failures)

    def test_unavailable_check_still_writes_both_secretless_failure_receipts(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "receipt.json"
            release_output = Path(directory) / "release.json"
            environment = os.environ.copy()
            environment.pop("CROWDRELAY_PRODUCTION_ADMIN_API_KEY", None)
            result = subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "scripts/verify-production-readiness.py"),
                    "--base-url",
                    "",
                    "--output",
                    str(output),
                    "--release-output",
                    str(release_output),
                ],
                check=False,
                capture_output=True,
                text=True,
                env=environment,
            )
            self.assertEqual(result.returncode, 1)
            self.assertEqual(json.loads(output.read_text()), json.loads(release_output.read_text()))
            receipt = json.loads(output.read_text())
            self.assertEqual(receipt["status"], "fail")
            self.assertEqual(receipt["failures"], ["readiness-check-unavailable"])
            self.assertEqual(receipt["errorClass"], "ValueError")
            serialized = json.dumps(receipt)
            self.assertNotIn("base-url", serialized)
            self.assertNotIn("admin", serialized.lower())

    def test_attestation_hashes_must_be_lower_hex_sha256(self):
        data = ledger()
        n8n = next(item for item in data["components"] if item["component_key"] == "n8n")
        n8n["manifest_sha"] = "Z" * 64
        n8n["workflow_attestation_sha"] = "not-a-sha".ljust(64, "x")
        failures, _ = MODULE.evaluate(data)
        self.assertIn("n8n-attestation-sha-missing", failures)
        self.assertIn("n8n-manifest-sha-missing", failures)
        self.assertIn("n8n-source-manifest-mismatch", failures)


if __name__ == "__main__":
    unittest.main()
