#!/usr/bin/env python3
import csv
import hashlib
import json
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

import generate_n8n_workflow_attestation as attest


class N8nWorkflowAttestationContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.manifest = self.root / "manifest.tsv"
        with self.manifest.open("w", newline="") as handle:
            writer = csv.DictWriter(
                handle,
                fieldnames=["event_type", "workflow_id", "capability", "enabled"],
                delimiter="\t",
            )
            writer.writeheader()
            writer.writerow(
                {
                    "event_type": "crowdrelay.team.assignment_email_requested",
                    "workflow_id": "VOSTEAMEMAIL001",
                    "capability": "team.email",
                    "enabled": "1",
                }
            )
        self.workflow = {
            "id": "VOSTEAMEMAIL001",
            "name": "secret production workflow name",
            "active": True,
            "nodes": [
                {
                    "id": "node-secret-id",
                    "name": "Gmail secret operator label",
                    "type": "n8n-nodes-base.gmail",
                    "credentials": {"gmailOAuth2": {"id": "credential-secret-id", "name": "private gmail"}},
                    "parameters": {"to": "private@example.test", "token": "do-not-publish"},
                }
            ],
            "settings": {
                "saveDataErrorExecution": "none",
                "saveDataSuccessExecution": "none",
                "saveManualExecutions": False,
                "saveExecutionProgress": False,
            },
        }
        self.workflow_dir = self.root / "workflows"
        self.workflow_dir.mkdir()
        (self.workflow_dir / "team.json").write_text(json.dumps(self.workflow))

    def tearDown(self):
        self.temp.cleanup()

    def smoke(self):
        return {
            "VOSTEAMEMAIL001": {
                "workflowSha256": attest.canonical_sha(self.workflow),
                "testedAt": datetime.now(timezone.utc).replace(microsecond=0).isoformat(),
                "eventValidation": True,
                "executionClaim": True,
                "providerReceipt": True,
                "receiptBeforeRetry": True,
                "credentialCheck": True,
                "claimContractVersion": attest.CLAIM_CONTRACT,
                "receiptContractVersion": attest.RECEIPT_CONTRACT,
            }
        }

    def test_public_attestation_binds_smoke_to_exact_workflow_without_leaking_private_fields(self):
        rows = attest.read_manifest(self.manifest)
        workflows = attest.load_workflows(self.workflow_dir)
        result = attest.build_attestation(
            self.manifest,
            rows,
            workflows,
            self.smoke(),
            datetime.now(timezone.utc),
            14,
        )
        encoded = json.dumps(result)
        self.assertIn("n8n-nodes-base.gmail", encoded)
        self.assertIn("VOSTEAMEMAIL001", encoded)
        self.assertNotIn("private@example.test", encoded)
        self.assertNotIn("credential-secret-id", encoded)
        self.assertNotIn("secret production workflow name", encoded)
        self.assertNotIn("do-not-publish", encoded)
        self.assertEqual(
            result["routeManifestSha256"], hashlib.sha256(self.manifest.read_bytes()).hexdigest()
        )

    def test_enabled_workflow_fails_if_persistence_or_bound_smoke_is_wrong(self):
        rows = attest.read_manifest(self.manifest)
        workflows = attest.load_workflows(self.workflow_dir)
        smoke = self.smoke()
        smoke["VOSTEAMEMAIL001"]["workflowSha256"] = "0" * 64
        self.workflow["settings"]["saveDataSuccessExecution"] = "all"
        with self.assertRaisesRegex(ValueError, "unsafe execution-data persistence"):
            attest.build_attestation(
                self.manifest,
                rows,
                {"VOSTEAMEMAIL001": self.workflow},
                smoke,
                datetime.now(timezone.utc),
                14,
            )

    def test_smoke_template_exposes_only_hash_and_contract_checks(self):
        template = attest.smoke_template(
            attest.read_manifest(self.manifest),
            attest.load_workflows(self.workflow_dir),
        )
        encoded = json.dumps(template)
        self.assertIn("workflowSha256", encoded)
        self.assertTrue(template["VOSTEAMEMAIL001"]["candidateEnabled"])
        self.assertNotIn("private@example.test", encoded)
        self.assertNotIn("credential-secret-id", encoded)

    def test_disabled_mapped_workflow_can_be_smoked_before_activation(self):
        rows = attest.read_manifest(self.manifest)
        rows[0]["enabled"] = "0"
        template = attest.smoke_template(rows, attest.load_workflows(self.workflow_dir))
        self.assertIn("VOSTEAMEMAIL001", template)
        self.assertFalse(template["VOSTEAMEMAIL001"]["candidateEnabled"])
        self.assertEqual(template["VOSTEAMEMAIL001"]["workflowSha256"], attest.canonical_sha(self.workflow))


if __name__ == "__main__":
    unittest.main()
