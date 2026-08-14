#!/usr/bin/env python3
import csv
import hashlib
import importlib.util
import json
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

SCRIPT = Path(__file__).with_name("build_n8n_executor_heartbeat.py")
SPEC = importlib.util.spec_from_file_location("n8n_heartbeat", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC and SPEC.loader
SPEC.loader.exec_module(MODULE)


class N8nHeartbeatBuilderTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.manifest = self.root / "manifest.tsv"
        with self.manifest.open("w", newline="") as handle:
            writer = csv.DictWriter(handle, fieldnames=["event_type", "workflow_id", "capability", "enabled"], delimiter="\t")
            writer.writeheader()
            writer.writerow({"event_type": "viryaos.team.assignment_email_requested", "workflow_id": "live-workflow", "capability": "team.email", "enabled": "1"})
        now = datetime.now(timezone.utc).replace(microsecond=0)
        self.attestation = self.root / "attestation.json"
        data = {
            "schemaVersion": 1,
            "generatedAt": now.isoformat().replace("+00:00", "Z"),
            "routeManifestSha256": hashlib.sha256(self.manifest.read_bytes()).hexdigest(),
            "workflows": [{
                "workflowId": "live-workflow", "enabled": True, "active": True,
                "smoke": {"eventValidation": True, "executionClaim": True, "providerReceipt": True, "receiptBeforeRetry": True, "credentialCheck": True},
            }],
        }
        self.attestation.write_text(json.dumps(data) + "\n")
        self.now = now

    def tearDown(self):
        self.temp.cleanup()

    def test_heartbeat_is_derived_from_exact_manifest_and_attestation(self):
        payload = MODULE.build_heartbeat(self.manifest, self.attestation, "n8n-blue", "2026.08.14", 90, self.now)
        manifest_sha = hashlib.sha256(self.manifest.read_bytes()).hexdigest()
        self.assertEqual(payload["manifest_sha"], manifest_sha)
        self.assertEqual(payload["capabilities"], [{"capability": "team.email", "version": "1"}])
        self.assertEqual(payload["metadata"]["workflow_attestation_manifest_sha"], manifest_sha)
        self.assertEqual(payload["metadata"]["workflow_attestation_sha"], hashlib.sha256(self.attestation.read_bytes()).hexdigest())

    def test_manifest_drift_fails_closed(self):
        self.manifest.write_text(self.manifest.read_text() + "viryaos.other\tother\tother.cap\t0\n")
        with self.assertRaisesRegex(ValueError, "route-manifest SHA"):
            MODULE.build_heartbeat(self.manifest, self.attestation, "n8n-blue", "v1", 90, self.now)


if __name__ == "__main__":
    unittest.main()
