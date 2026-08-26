"""Contract tests for admin acquisition endpoints — campaign preparation.

The plan's first prerequisite for the acquisition campaign is "make every
launch channel a tracked link". The third is "give the nineteen a referral
code each". Both need admin endpoints that did not exist before:

- POST /v1/admin/smart-links — creates a tracked link with channel attribution
- GET  /v1/admin/smart-links — lists all links with channel attribution
- POST /v1/admin/audience/fans/{fan_id}/referral-code — backfills a code

Pinned here:
- The routes exist and are admin-scoped.
- Smart link creation accepts channel_source/channel_community/channel_creative
  (migration 0079), is idempotent on slug, and requires https:// destination.
- Referral code creation is idempotent (returns existing code if one exists).
- All SQL writes go through AcquisitionRepository — the API layer holds no
  sqlx write call sites (the api-sql-ratchet enforces this).
- The UpsertSmartLinkCommand is a parameter struct, not 8 loose arguments.
"""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
ACQUISITION = ROOT / "crates/crowdrelay-api/src/acquisition.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/ports.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/acquisition.rs"
RATCHET = ROOT / "scripts/api-sql-ratchet.json"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class AdminAcquisitionContract(unittest.TestCase):
    def setUp(self) -> None:
        self.routing = read(ROUTING)
        self.acquisition = read(ACQUISITION)
        self.ports = read(PORTS)
        self.infra = read(INFRA)

    # --- routes exist and are admin-scoped -------------------------------

    def test_smart_links_route_is_admin_scoped(self) -> None:
        self.assertIn("/v1/admin/smart-links", self.routing)
        self.assertIn("acquisition::admin_create_smart_link", self.routing)
        self.assertIn("acquisition::admin_list_smart_links", self.routing)

    def test_fan_referral_code_route_is_admin_scoped(self) -> None:
        self.assertIn(
            "/v1/admin/audience/fans/{fan_id}/referral-code",
            self.routing,
        )
        self.assertIn("acquisition::admin_create_fan_referral_code", self.routing)

    # --- smart link creation ---------------------------------------------

    def test_create_accepts_channel_attribution(self) -> None:
        handler = self.acquisition.split("admin_create_smart_link", 1)[1]
        self.assertIn("channel_source", handler)
        self.assertIn("channel_community", handler)
        self.assertIn("channel_creative", handler)
        self.assertIn("campaign_id", handler)

    def test_create_requires_https_destination(self) -> None:
        handler = self.acquisition.split("admin_create_smart_link", 1)[1]
        self.assertIn("starts_with(\"https://\")", handler)

    def test_create_validates_slug(self) -> None:
        handler = self.acquisition.split("admin_create_smart_link", 1)[1]
        self.assertIn("SmartLinkSlug::parse", handler)

    def test_create_is_idempotent_on_slug(self) -> None:
        impl = self.infra.split("upsert_smart_link", 1)[1]
        self.assertIn("ON CONFLICT (workspace_id, slug) DO UPDATE", impl)

    def test_create_returns_created_status(self) -> None:
        handler = self.acquisition.split("admin_create_smart_link", 1)[1]
        self.assertIn("CREATED", handler)

    # --- smart link listing ----------------------------------------------

    def test_list_returns_all_links_with_channels(self) -> None:
        handler = self.acquisition.split("admin_list_smart_links", 1)[1]
        self.assertIn("list_smart_links", handler)
        self.assertIn("channel_source", handler)

    def test_list_orders_by_channel_then_slug(self) -> None:
        impl = self.infra.split("list_smart_links", 1)[1]
        self.assertIn("ORDER BY channel_source NULLS LAST, slug", impl)

    # --- referral code backfill ------------------------------------------

    def test_referral_code_is_idempotent(self) -> None:
        impl = self.infra.split("load_or_create_fan_referral_code", 1)[1]
        # Returns existing code before trying to insert.
        self.assertIn("existing", impl.lower())

    def test_referral_code_checks_fan_exists(self) -> None:
        impl = self.infra.split("load_or_create_fan_referral_code", 1)[1]
        self.assertIn("SELECT EXISTS(SELECT 1 FROM fans", impl)

    def test_referral_code_returns_not_found_for_missing_fan(self) -> None:
        handler = self.acquisition.split("admin_create_fan_referral_code", 1)[1]
        self.assertIn("NotFound", handler)
        self.assertIn("not_found", handler)

    def test_referral_code_uses_gen_random_bytes(self) -> None:
        impl = self.infra.split("load_or_create_fan_referral_code", 1)[1]
        self.assertIn("gen_random_bytes(18)", impl)

    def test_referral_code_retries_on_collision(self) -> None:
        impl = self.infra.split("load_or_create_fan_referral_code", 1)[1]
        self.assertIn("ON CONFLICT DO NOTHING", impl)
        # At least 3 retries.
        self.assertIn("0..3", impl)

    # --- repository trait ------------------------------------------------

    def test_acquisition_repository_has_upsert_smart_link(self) -> None:
        self.assertIn("upsert_smart_link", self.ports)
        self.assertIn("UpsertSmartLinkCommand", self.ports)
        self.assertIn("UpsertedSmartLink", self.ports)

    def test_acquisition_repository_has_list_smart_links(self) -> None:
        self.assertIn("list_smart_links", self.ports)

    def test_acquisition_repository_has_load_or_create_fan_referral_code(self) -> None:
        self.assertIn("load_or_create_fan_referral_code", self.ports)

    def test_upsert_command_is_a_parameter_struct(self) -> None:
        self.assertIn("pub struct UpsertSmartLinkCommand", self.ports)

    # --- API layer holds no sqlx writes ----------------------------------

    def test_acquisition_rs_is_not_in_api_sql_ratchet(self) -> None:
        import json

        baseline = json.loads(RATCHET.read_text(encoding="utf-8"))
        key = "crates/crowdrelay-api/src/acquisition.rs"
        self.assertNotIn(
            key,
            baseline["maxWrites"],
            "acquisition.rs must not have SQL writes in the HTTP layer",
        )

    # --- state wiring ----------------------------------------------------

    def test_acquisition_state_holds_repository(self) -> None:
        self.assertIn("acquisition_repository", self.acquisition)
        self.assertIn("Arc<dyn AcquisitionRepository>", self.acquisition)

    def test_acquisition_state_exposes_repository(self) -> None:
        self.assertIn("fn acquisition_repository(&self)", self.acquisition)

    def test_acquisition_state_exposes_workspace_id(self) -> None:
        self.assertIn("fn workspace_id(&self)", self.acquisition)


if __name__ == "__main__":
    unittest.main()
