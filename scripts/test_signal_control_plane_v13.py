from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
LIB = ((ROOT / "crates/crowdrelay-api/src/lib.rs").read_text(encoding="utf-8") + (ROOT / "crates/crowdrelay-api/src/routing.rs").read_text(encoding="utf-8"))
OPS = (ROOT / "crates/crowdrelay-api/src/ops.rs").read_text(encoding="utf-8")
SPEC = (ROOT / "openapi/openapi.yaml").read_text(encoding="utf-8")


class SignalControlPlaneV13(unittest.TestCase):
    def test_owner_only_route_and_private_response_exist(self):
        self.assertIn('/v1/admin/signal/overview', LIB)
        self.assertIn('get(ops::signal_overview)', LIB)
        self.assertIn('private_json(', OPS)
        self.assertIn('SignalOverview', OPS)

    def test_snapshot_is_aggregate_only_and_bounded(self):
        start = OPS.index('pub struct SignalOverview')
        end = OPS.index('struct SignalSummaryRow', start)
        contract = OPS[start:end]
        self.assertNotIn('email', contract)
        self.assertNotIn('display_name', contract)
        self.assertNotIn('fan_id', contract)
        self.assertIn('LIMIT 10', OPS)
        self.assertIn('latest_marketing', OPS)
        self.assertIn('unavailable_sources.push("top_cities")', OPS)

    def test_openapi_contract_is_versioned_with_private_cache(self):
        self.assertIn('/admin/signal/overview:', SPEC)
        self.assertIn("$ref: '#/components/schemas/SignalOverview'", SPEC)
        self.assertIn("$ref: '#/components/headers/PrivateNoStore'", SPEC)
        self.assertIn('maxItems: 10', SPEC)


if __name__ == "__main__":
    unittest.main()
