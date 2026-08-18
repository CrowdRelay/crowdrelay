#!/usr/bin/env python3
"""Source-level regression contract for tenant AREA Designer.

This intentionally checks architecture/privacy invariants that are easy to regress
without requiring a live PostgreSQL instance. It complements, not replaces, Rust
compile/tests.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = (ROOT / "migrations/0067_area_designer.sql").read_text()
STORAGE = (ROOT / "crates/crowdrelay-api/src/area/storage.rs").read_text()
CHALLENGE = (ROOT / "crates/crowdrelay-api/src/area/challenge.rs").read_text()
CLAIMS = (ROOT / "crates/crowdrelay-api/src/area/claims.rs").read_text()
ENDPOINTS = (ROOT / "crates/crowdrelay-api/src/area/endpoints.rs").read_text()
API = (ROOT / "crates/crowdrelay-api/src/area_admin.rs").read_text()
API_LIB = (ROOT / "crates/crowdrelay-api/src/lib.rs").read_text()
APP = (ROOT / "crates/crowdrelay-application/src/area_admin.rs").read_text()
DOMAIN = (ROOT / "crates/crowdrelay-domain/src/area.rs").read_text()
INFRA_ROOT = ROOT / "crates/crowdrelay-infra/src"
INFRA = (INFRA_ROOT / "area_admin.rs").read_text() + "\n" + "\n".join(
    path.read_text() for path in sorted((INFRA_ROOT / "area_admin").glob("*.rs"))
)


class AreaDesignerContract(unittest.TestCase):
    def test_schema_is_additive_draft_publish_and_history_safe(self):
        for needle in [
            "ADD COLUMN IF NOT EXISTS revision",
            "ADD COLUMN IF NOT EXISTS published_at",
            "ADD COLUMN IF NOT EXISTS archived_at",
            "CREATE TABLE IF NOT EXISTS area_drop_drafts",
            "CREATE TABLE IF NOT EXISTS area_workspace_settings",
            "WHERE archived_at IS NULL",
        ]:
            self.assertIn(needle, MIGRATION)
        self.assertIn("area_drops_workspace_current_city_uidx", MIGRATION)
        self.assertIn("area_drops_workspace_current_number_uidx", MIGRATION)
        self.assertIn("UPDATE area_drops\nSET published_at = COALESCE", MIGRATION)
        self.assertIn("OR EXISTS (", MIGRATION)
        self.assertIn("FROM area_drops AS existing_drop", MIGRATION)
        self.assertIn("CHECK (base_revision >= 0)", MIGRATION)
        self.assertNotIn("FOREIGN KEY (workspace_id, drop_id)", MIGRATION)
        # New drafts must not create runtime placeholders: this preserves old-binary
        # rollback behavior until the first explicit publish.
        create_draft = INFRA.split("async fn create_draft(", 1)[1].split("async fn save_draft(", 1)[0]
        self.assertNotIn("INSERT INTO area_drops", create_draft)
        self.assertIn("INSERT INTO area_drop_drafts", create_draft)

    def test_public_runtime_only_reads_published_enabled_non_archived_drops(self):
        for needle in [
            "area_drop.published_at IS NOT NULL",
            "area_drop.archived_at IS NULL",
            "FROM area_workspace_settings AS area_settings",
            "area_settings.enabled",
        ]:
            self.assertIn(needle, STORAGE)
        # Entitlement must be folded into existing reads, not become an extra
        # per-request round trip on the existing VIRYA AREA hot path.
        runtime_source = STORAGE + CHALLENGE + CLAIMS + ENDPOINTS
        self.assertNotIn("fn area_is_enabled", runtime_source)
        self.assertNotIn("async fn area_is_enabled", runtime_source)

    def test_challenge_and_claim_enforce_entitlement_but_legacy_import_can_repair_history(self):
        self.assertIn("published_at IS NOT NULL", CHALLENGE)
        self.assertIn("archived_at IS NULL", CHALLENGE)
        self.assertIn("area_settings.enabled", CHALLENGE)
        self.assertRegex(CLAIMS, r"lock_drop\([\s\S]*?require_enabled: bool")
        self.assertIn("player_id, true).await", CLAIMS)
        self.assertIn("player_id, false).await", ENDPOINTS)

    def test_management_namespace_uses_dedicated_role(self):
        self.assertIn('path == "/v1/control-plane/area" || path.starts_with("/v1/control-plane/area/")', API_LIB)
        self.assertIn("PrivilegedAuthorization::AreaManagement", API_LIB)
        self.assertIn("area_management_api_key_sha256", API_LIB)
        self.assertIn('"/v1/control-plane/area/drops/{drop_id}/publish"', API)
        self.assertIn("DefaultBodyLimit::max(crate::MAX_PUBLIC_BODY_BYTES)", API)
        self.assertIn("pub(crate) fn router(state: AppState) -> Router", API)
        self.assertIn(".with_state(state)", API)
        self.assertIn("area_admin::router(state.clone())", API_LIB)

    def test_list_contract_never_contains_exact_coordinates(self):
        summary = re.search(
            r"pub struct AreaDropSummary \{(?P<body>.*?)\n\}", APP, re.DOTALL
        )
        self.assertIsNotNone(summary)
        body = summary.group("body")
        self.assertNotIn("exact_lat", body)
        self.assertNotIn("exact_lng", body)
        self.assertIn("has_exact_location", body)
        # Exact geometry is allowed only in the private detail/draft model.
        self.assertIn("pub struct AreaDropDetail", APP)
        self.assertIn("pub published: AreaDropDraft", APP)

    def test_domain_debug_and_audit_redact_exact_geometry(self):
        self.assertIn('map(|_| "[REDACTED]")', DOMAIN)
        self.assertIn('changed.push("exactLocation")', DOMAIN)
        self.assertNotIn('json!({"exactLat"', INFRA)
        self.assertNotIn('json!({"exactLng"', INFRA)
        publish_audit = re.search(
            r'"area\.drop\.published"[\s\S]{0,500}?json!\((?P<body>.*?)\)\s*,',
            INFRA,
        )
        self.assertIsNotNone(publish_audit)
        self.assertIn('"changed"', publish_audit.group("body"))
        self.assertNotIn("exact_lat", publish_audit.group("body"))
        self.assertNotIn("exact_lng", publish_audit.group("body"))

    def test_publish_revalidates_claim_count_inside_locked_transaction(self):
        publish = INFRA.split("async fn publish(", 1)[1].split("async fn set_active(", 1)[0]
        self.assertIn("FOR UPDATE", publish)
        self.assertIn("validation_issues_tx", publish)
        validation = INFRA.split("async fn validation_issues_tx(", 1)[1].split("#[async_trait]", 1)[0]
        self.assertIn("FROM area_claims", validation)
        self.assertIn("fetch_one(&mut **tx)", validation)

    def test_custom_city_is_create_only_for_shared_catalogue(self):
        create_city = INFRA.split("async fn create_city(", 1)[1].split("async fn list_drops(", 1)[0]
        self.assertIn("ON CONFLICT (country_code, slug) DO NOTHING", create_city)
        self.assertNotIn("DO UPDATE", create_city)
        self.assertNotRegex(create_city, r"UPDATE\s+cities")

    def test_lifecycle_protects_history_and_unpublished_rows(self):
        archive = INFRA.split("async fn archive(", 1)[1].split("async fn duplicate(", 1)[0]
        self.assertIn('Conflict("DROP_NOT_PUBLISHED")', archive)
        delete = INFRA.split("async fn delete_unpublished(", 1)[1]
        self.assertIn("SELECT EXISTS(SELECT 1 FROM area_drops", delete)
        self.assertIn('Conflict("DROP_HAS_HISTORY")', delete)
        self.assertIn("DELETE FROM area_drop_drafts", delete)
        self.assertNotIn("DELETE FROM area_drops", delete)


    def test_saved_drafts_get_the_same_structural_storage_guard_as_created_drafts(self):
        save_draft = INFRA.split("async fn save_draft(", 1)[1].split("async fn discard_draft(", 1)[0]
        self.assertIn("draft_storage_safe(&draft)", save_draft)
        self.assertIn('Conflict("INVALID_DRAFT")', save_draft)

    def test_first_publish_is_the_only_path_that_materializes_a_new_runtime_drop(self):
        publish = INFRA.split("async fn publish(", 1)[1].split("async fn set_active(", 1)[0]
        self.assertIn("INSERT INTO area_drops", publish)
        self.assertIn("published_at", publish)
        self.assertIn("let current_revision = published_state.as_ref().map_or(0", publish)
        self.assertIn("let next_revision = current_revision + 1", publish)


if __name__ == "__main__":
    unittest.main()
