#!/usr/bin/env python3
"""Tenant settings HTTP contract: branding without psql.

Completes the tenant_settings story end to end: an operator edits brand URLs
from the admin surface (and the Control Plane forwards them), while every
write lands in the infra repository and invalidates its cache.

Pins:
- only EDITABLE_KEYS are accepted — no arbitrary key smuggling;
- GET returns EFFECTIVE values plus which keys are overridden;
- both /v1/admin and /v1/control-plane mounts reuse the same handlers, so the
  platform plane grows no authority path of its own;
- zero write SQL in this module.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

API_MODULE = ROOT / "crates/crowdrelay-api/src/tenant_settings_http.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/tenant_settings.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
CONTROL_PLANE = ROOT / "crates/crowdrelay-api/src/control_plane.rs"

# Case-sensitive on purpose: Rust sources keep SQL keywords uppercase,
# while lowercase prose ("could not update...") must not false-positive.
WRITE_SQL = re.compile(r"\b(INSERT\s+INTO|UPDATE\s+[a-z_]+|DELETE\s+FROM)\b")


class TenantSettingsHttpContract(unittest.TestCase):
    def test_key_allowlist_is_enforced_before_any_write(self):
        source = API_MODULE.read_text()
        self.assertIn("EDITABLE_KEYS.contains(&key)", source)
        self.assertRegex(source, r"fn validate_value\(key: &str")

    def test_get_returns_effective_plus_override_markers(self):
        source = API_MODULE.read_text()
        self.assertIn("brand_settings(workspace_id)", source)
        self.assertIn("list_overrides(workspace_id)", source)
        self.assertIn("overridden", source)

    def test_writes_live_only_in_infra_and_invalidate_cache(self):
        api_source = API_MODULE.read_text()
        writes = [m.group(0) for m in WRITE_SQL.finditer(api_source)]
        self.assertEqual(writes, [])
        infra = INFRA.read_text()
        self.assertIn("cache.remove(&workspace_id)", infra)

    def test_both_mounts_reuse_the_same_handlers(self):
        control_plane = CONTROL_PLANE.read_text()
        self.assertIn(
            "/v1/control-plane/tenant-settings",
            control_plane,
        )
        self.assertIn("tenant_settings_http::get_brand_settings", control_plane)
        routing = ROUTING.read_text()
        self.assertIn("/v1/admin/tenant-settings", routing)


if __name__ == "__main__":
    unittest.main()
