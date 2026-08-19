#!/usr/bin/env python3
"""Keep canonical OpenAPI complete for every literal Axum /v1 route."""
from __future__ import annotations

import re
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[1]
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


class OpenApiRouterCoverageContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.routing = ROUTING.read_text()
        cls.spec = yaml.safe_load(OPENAPI.read_text())

    def test_every_literal_v1_router_path_is_documented(self) -> None:
        router_paths = set(re.findall(r'\.route\(\s*"(/v1/[^"?]+)"', self.routing))
        openapi_paths = {
            path if path.startswith("/v1/") else f"/v1{path}"
            for path in self.spec.get("paths", {})
            if path.startswith("/")
        }
        missing = sorted(router_paths - openapi_paths)
        self.assertEqual([], missing, "literal router paths missing from OpenAPI")

    def test_tenant_regional_contract_matches_runtime_shape(self) -> None:
        operation = self.spec["paths"]["/public/tenant/config"]["get"]
        self.assertEqual(
            "#/components/schemas/TenantProfile",
            operation["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        )
        schemas = self.spec["components"]["schemas"]
        timezone = schemas["FanPushPreferences"]["properties"]["quiet_timezone"]
        self.assertNotIn("enum", timezone)
        self.assertEqual(64, timezone.get("maxLength"))
        regional_timezone = schemas["TenantRegionalProfile"]["properties"]["timezone"]
        self.assertNotIn("enum", regional_timezone)
        self.assertEqual(64, regional_timezone.get("maxLength"))

    def test_operation_ids_are_unique(self) -> None:
        operation_ids: list[str] = []
        for path_item in self.spec.get("paths", {}).values():
            for operation in path_item.values():
                if isinstance(operation, dict) and operation.get("operationId"):
                    operation_ids.append(operation["operationId"])
        self.assertEqual(len(operation_ids), len(set(operation_ids)))


if __name__ == "__main__":
    unittest.main()
