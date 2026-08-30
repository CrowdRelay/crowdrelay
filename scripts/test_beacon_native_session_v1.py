#!/usr/bin/env python3
"""Native/web Latarnik session attribution and invite-preview contract."""
from __future__ import annotations

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = (ROOT / "migrations/0064_beacon_native_session_attribution.sql").read_text()
BEACON = (ROOT / "crates/crowdrelay-api/src/beacon_signal.rs").read_text()
LIFECYCLE = (ROOT / "crates/crowdrelay-api/src/beacon_signal/lifecycle.rs").read_text()
LIFECYCLE_ADMIN = (ROOT / "crates/crowdrelay-api/src/beacon_signal/lifecycle/admin.rs").read_text()
NETWORK = (ROOT / "crates/crowdrelay-api/src/beacon_signal/network.rs").read_text()
NETWORK_ADMIN = (ROOT / "crates/crowdrelay-api/src/beacon_signal/network/admin.rs").read_text()
NETWORK_INTERNAL = (ROOT / "crates/crowdrelay-api/src/beacon_signal/network/internal.rs").read_text()
COPY = (ROOT / "crates/crowdrelay-api/src/beacon_signal/invite_copy.rs").read_text()
OPENAPI = (ROOT / "openapi/openapi.yaml").read_text()
OPERATOR = (ROOT / "scripts/latarnik_operator.py").read_text()
# SQL writes were extracted from the API layer behind repository ports (S3.2-S3.6).
INFRA_SIGNAL = (ROOT / "crates/crowdrelay-infra/src/beacon_signal/signal.rs").read_text()


class BeaconNativeSessionV1Contract(unittest.TestCase):
    def test_migration_attributes_jobs_without_persisting_invite_capabilities(self) -> None:
        self.assertIn("pending_invite_job_id uuid", MIGRATION)
        self.assertIn("client_kind text NOT NULL DEFAULT 'unknown'", MIGRATION)
        self.assertIn("source_invite_job_id uuid", MIGRATION)
        self.assertIn("'unknown','web','android','ios'", MIGRATION)
        lowered = MIGRATION.lower()
        self.assertNotIn("invite_url", lowered)
        self.assertNotIn("bearer_token", lowered)
        self.assertNotIn("raw_token", lowered)

    def test_exchange_has_bounded_client_kind_and_one_time_job_handoff(self) -> None:
        exchange = BEACON.split("pub async fn exchange_invite", 1)[1]
        self.assertIn("client_kind: Option<String>", BEACON)
        self.assertIn('unwrap_or("web")', exchange)
        self.assertIn('matches!(client_kind, "web" | "android" | "ios")', exchange)
        self.assertIn("client_kind: client_kind.to_owned()", exchange)
        infra_exchange = INFRA_SIGNAL.split("async fn exchange_invite", 1)[1]
        self.assertIn("profile.pending_invite_job_id", infra_exchange)
        self.assertIn("pending_invite_job_id=NULL", infra_exchange)
        self.assertIn("source_invite_job_id", infra_exchange)
        self.assertLess(infra_exchange.index("source_invite_job_id"), infra_exchange.index("pending_invite_job_id=NULL"))

    def test_direct_and_non_campaign_invites_do_not_inherit_old_job_attribution(self) -> None:
        infra_create = INFRA_SIGNAL.split("async fn create_invite", 1)[1].split("async fn exchange_invite", 1)[0]
        self.assertIn("pending_invite_job_id = NULL", infra_create)
        self.assertIn("None,", LIFECYCLE_ADMIN.split("mint_invite_batch_tx", 1)[1])
        claim = NETWORK_INTERNAL.split("mint_invite_batch_tx", 1)[1]
        self.assertIn("Some(job_id)", claim)
        self.assertIn("pending_invite_job_id=EXCLUDED.pending_invite_job_id", LIFECYCLE)

    def test_preview_reuses_eligibility_without_minting_or_requiring_mutation_identity(self) -> None:
        action = NETWORK_ADMIN.split("pub async fn admin_beacon_network_action", 1)[1].split("async fn request_discovery", 1)[0]
        self.assertIn('if payload.action == "preview_invites"', action)
        self.assertLess(action.index('payload.action == "preview_invites"'), action.index("idempotency_key(&headers)"))
        preview = NETWORK_ADMIN.split("async fn preview_invites", 1)[1].split("async fn queue_invites", 1)[0]
        self.assertIn('"tokensMinted": false', preview)
        self.assertIn("beacon.active AND beacon.verified AND beacon.accepts_outreach", preview)
        self.assertIn("NOT beacon.do_not_contact", preview)
        self.assertIn("marketing_email_consent_confirmed", preview)
        self.assertNotIn("mint_invite_batch_tx", preview)
        self.assertNotIn("random_token", preview)
        self.assertNotIn("invite_token_hash", preview)

    def test_campaign_metrics_are_derived_from_session_attribution(self) -> None:
        for field in (
            "exchanged_count", "web_count", "android_count", "ios_count", "active_count",
            "push_enabled_count", "helping_count", "coverage_count",
        ):
            self.assertIn(field, NETWORK)
            self.assertIn(field, NETWORK_ADMIN)
        self.assertIn("session.source_invite_job_id", NETWORK_ADMIN)
        self.assertIn("endpoint.principal_hash=session.token_hash", NETWORK_ADMIN)
        self.assertIn("engagement.updated_at >= session.created_at", NETWORK_ADMIN)
        self.assertIn("coverage.created_at >= session.created_at", NETWORK_ADMIN)

    def test_openapi_and_cli_expose_preview_and_native_attribution(self) -> None:
        self.assertIn("clientKind:", OPENAPI)
        self.assertIn("enum: [web, android, ios, null]", OPENAPI)
        self.assertIn("preview_invites", OPENAPI)
        for field in (
            "exchangedCount", "webCount", "androidCount", "iosCount", "activeCount",
            "pushEnabledCount", "helpingCount", "coverageCount",
        ):
            self.assertIn(field, OPENAPI)
        self.assertIn('add_parser("network-preview"', OPERATOR)

    def test_invitation_copy_sets_professional_non_transactional_expectation(self) -> None:
        lowered = COPY.lower()
        self.assertIn("nie jest newsletter", lowered)
        self.assertIn("programem ambasadorskim", lowered)
        self.assertIn("nie ma obowiązku publikowania", lowered)
        self.assertIn("virya signal", lowered)
        self.assertIn("przeglądarce", lowered)
        self.assertNotIn("każdy latarnik dostaje każdą", lowered)
        self.assertNotIn("coverage za", lowered)


    def test_campaign_outcomes_stop_at_session_revocation(self) -> None:
        admin = NETWORK_ADMIN
        self.assertIn("engagement.updated_at < session.expires_at", admin)
        self.assertIn("engagement.updated_at < session.revoked_at", admin)
        self.assertIn("coverage.created_at < session.expires_at", admin)
        self.assertIn("coverage.created_at < session.revoked_at", admin)

if __name__ == "__main__":
    unittest.main()
