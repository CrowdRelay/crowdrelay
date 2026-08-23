#!/usr/bin/env python3
"""Latarnik public-source acquisition and reviewed invite-delivery contract."""
from __future__ import annotations

import json
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = (ROOT / "migrations/0059_beacon_network_acquisition.sql").read_text()
NETWORK = (ROOT / "crates/crowdrelay-api/src/beacon_signal/network.rs").read_text()
ADMIN = (ROOT / "crates/crowdrelay-api/src/beacon_signal/network/admin.rs").read_text()
INTERNAL = (ROOT / "crates/crowdrelay-api/src/beacon_signal/network/internal.rs").read_text()
LIFECYCLE = (ROOT / "crates/crowdrelay-api/src/beacon_signal/lifecycle.rs").read_text()
ROUTING = (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text()
META = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text()
OPENAPI = (ROOT / "openapi/openapi.yaml").read_text()
README = (ROOT / "n8n/README.md").read_text()
PRODUCTION_MANIFEST_PATH = ROOT / "n8n/viryaos-production-workflow-manifest.tsv"
EXECUTOR_MANIFEST_PATH = ROOT / "n8n/viryaos-executor-manifest.tsv"
GITIGNORE = (ROOT / ".gitignore").read_text()
EXECUTOR = (ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs").read_text()
OPERATOR = (ROOT / "scripts/latarnik_operator.py").read_text()


class BeaconNetworkAcquisitionV1Contract(unittest.TestCase):
    def test_schema_tracks_runs_and_one_shot_jobs_without_second_crm(self) -> None:
        self.assertIn("CREATE TABLE viryaos_beacon_network_discovery_runs", MIGRATION)
        self.assertIn("CREATE TABLE viryaos_beacon_invite_delivery_jobs", MIGRATION)
        self.assertNotIn("CREATE TABLE viryaos_beacon_network_contacts", MIGRATION)
        self.assertIn("beacon_ids uuid[]", MIGRATION)
        self.assertIn("claim_token_hash bytea", MIGRATION)
        self.assertIn("INSERT INTO viryaos_manager_config (workspace_id, config_key, value)", MIGRATION)
        self.assertNotIn("INSERT INTO viryaos_manager_config (workspace_id, config_key, config)", MIGRATION)
        self.assertIn("'queued','claimed','completed','failed','ambiguous','cancelled'", MIGRATION)
        self.assertIn("rawInviteCapabilitiesInOutbox', false", MIGRATION)

    def test_discovery_ingest_cannot_grant_outreach_permission(self) -> None:
        self.assertIn("true,false,false,false", INTERNAL)
        self.assertIn('"human_review_required": true', INTERNAL)
        self.assertIn('"marketing_email_consent_confirmed": false', INTERNAL)
        self.assertIn("metadata ? 'network_discovery_run_id'", ADMIN)
        self.assertNotIn("accepts_outreach=true", INTERNAL)
        self.assertNotIn("verified=true", INTERNAL)

    def test_human_approval_requires_source_and_consent_evidence(self) -> None:
        self.assertIn("payload.source_verified != Some(true)", ADMIN)
        self.assertIn("payload.marketing_email_consent_confirmed != Some(true)", ADMIN)
        self.assertIn("!valid_https_url(&evidence_url)", ADMIN)
        self.assertIn('"consent_evidence_url": evidence_url', ADMIN)
        self.assertIn("SET verified=true,accepts_outreach=true", ADMIN)
        self.assertIn("approve_beacon_network_candidate", ADMIN)

    def test_invite_outbox_contains_job_id_not_plaintext_capability(self) -> None:
        queue = ADMIN.rsplit("async fn queue_invites", 1)[1]
        self.assertIn("viryaos.beacon.invite_delivery_requested", queue)
        self.assertIn('json!({"job_id": job_id})', queue)
        self.assertNotIn("invite_url", queue)
        self.assertNotIn("invite_token", queue)
        self.assertIn("token_hash(&invite_token)", LIFECYCLE)

    def test_claim_is_one_shot_and_ambiguous_is_terminal(self) -> None:
        self.assertIn('if job.4 != "queued"', INTERNAL)
        self.assertIn("mint_invite_batch_tx", INTERNAL)
        self.assertIn("claim_token: String", NETWORK)
        self.assertIn("SET status='claimed',claim_token_hash=$3", INTERNAL)
        self.assertIn('"completed" | "failed" | "ambiguous"', INTERNAL)
        self.assertIn('if current.0 != "claimed"', INTERNAL)
        self.assertNotIn("status='queued'", INTERNAL.split("internal_report_invite_delivery_job", 1)[1])

    def test_executor_capabilities_fail_closed_until_private_workflows_are_attested(self) -> None:
        pairs = (
            ("viryaos.beacon.release_delivery_confirmation_requested", "beacon.release.mail"),
            ("viryaos.beacon.network_discovery_requested", "beacon.network.discovery"),
            ("viryaos.beacon.invite_delivery_requested", "beacon.network.invite"),
        )
        for event, capability in pairs:
            self.assertIn(f'"{event}" => "{capability}"', EXECUTOR)

        # The concrete n8n executor/production manifests are intentionally private
        # and gitignored. Fresh CI checkouts must not require or recreate them. If
        # an operator has the private files locally, still assert that the new
        # capabilities remain UNAVAILABLE/enabled=0 until workflow attestation.
        self.assertIn("n8n/*", GITIGNORE)
        self.assertIn("!n8n/README.md", GITIGNORE)
        self.assertIn("!n8n/examples/", GITIGNORE)
        if PRODUCTION_MANIFEST_PATH.exists() and EXECUTOR_MANIFEST_PATH.exists():
            production_manifest = PRODUCTION_MANIFEST_PATH.read_text()
            executor_manifest = EXECUTOR_MANIFEST_PATH.read_text()
            for event, capability in pairs:
                self.assertIn(f'{event}\tUNAVAILABLE\t{capability}\t0', production_manifest)
                self.assertIn(event, executor_manifest)

        self.assertIn('"beacon.release.mail"', (ROOT / "crates/crowdrelay-api/src/beacon_signal/releases/admin.rs").read_text())
        self.assertIn('"beacon.network.discovery"', ADMIN)
        self.assertIn('"beacon.network.invite"', ADMIN)

    def test_public_workflow_examples_preserve_review_and_execution_boundaries(self) -> None:
        discovery = json.loads((ROOT / "n8n/examples/beacon-network-discovery.example.json").read_text())
        invite = json.loads((ROOT / "n8n/examples/beacon-invite-delivery.example.json").read_text())
        release = json.loads((ROOT / "n8n/examples/beacon-release-mail.example.json").read_text())
        for workflow in (discovery, invite, release):
            self.assertFalse(workflow["active"])
            self.assertEqual("none", workflow["settings"]["saveDataSuccessExecution"])
            self.assertEqual("none", workflow["settings"]["saveDataErrorExecution"])
        discovery_text = json.dumps(discovery, ensure_ascii=False)
        self.assertIn("n8n-nodes-base.convertToFile", discovery_text)
        self.assertIn('"operation": "xlsx"', discovery_text)
        self.assertIn("MarketingConsent", discovery_text)
        self.assertIn("NOT CONFIRMED", discovery_text)
        # The public README no longer enumerates the review step by name. What
        # it still has to say is the boundary that makes the step necessary:
        # n8n executes, and chooses nothing.
        self.assertIn(
            "does not own business state, recipient selection, policy, authority",
            README.lower(),
        )
        invite_text = json.dumps(invite)
        self.assertIn("claim", invite_text)
        self.assertIn('"retryOnFail": false', invite_text)

    def test_operator_cli_has_reviewed_network_and_release_parity_with_retry_identity(self) -> None:
        for command in (
            "release-list", "release-create", "release-launch", "release-close",
            "release-recipients", "release-state", "network", "network-discover",
            "network-approve", "network-invite",
        ):
            self.assertIn(f'add_parser("{command}"', OPERATOR)
        self.assertIn("--idempotency-key", OPERATOR)
        self.assertIn("reuse this exact key if the result is ambiguous", OPERATOR)
        self.assertIn("--marketing-email-consent-confirmed", OPERATOR)
        self.assertIn("--consent-evidence-url", OPERATOR)
        self.assertIn("must be an HTTPS URL without embedded credentials", OPERATOR)

    def test_routes_openapi_and_meta_are_complete(self) -> None:
        for route in (
            "/v1/admin/autopilot/beacon-network",
            "/v1/internal/beacon/network-discovery/{run_id}/candidates",
            "/v1/internal/beacon/network-discovery/{run_id}/report",
            "/v1/internal/beacon/invite-delivery-jobs/{job_id}/claim",
            "/v1/internal/beacon/invite-delivery-jobs/{job_id}/report",
        ):
            self.assertIn(route, ROUTING)
            self.assertIn(route.removeprefix("/v1"), OPENAPI)
        self.assertIn("", META)
        self.assertIn('(\"beacon_network_acquisition_v1\", true)', META)


if __name__ == "__main__":
    unittest.main()
