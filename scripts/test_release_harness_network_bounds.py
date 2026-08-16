#!/usr/bin/env python3
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class ReleaseHarnessNetworkBounds(unittest.TestCase):
    def text(self, path: str) -> str:
        return (ROOT / path).read_text(encoding="utf-8")

    def test_production_readiness_binds_admin_auth_to_https_and_caps_ledger(self):
        source = self.text("scripts/verify-production-readiness.py")
        self.assertIn('parsed.scheme != "https"', source)
        self.assertIn("MAX_LEDGER_RESPONSE_BYTES + 1", source)
        self.assertIn("release ledger response exceeds size limit", source)

    def test_operator_and_rekor_emergency_clients_fail_closed_on_plaintext(self):
        operator = self.text("scripts/latarnik_operator.py")
        rekor = self.text("scripts/rekor-disable.py")
        self.assertIn('parsed.scheme != "https"', operator)
        self.assertIn("MAX_RESPONSE_BYTES + 1", operator)
        self.assertIn('parsed_base.scheme != "https"', rekor)
        self.assertIn("MAX_RESPONSE_BYTES + 1", rekor)

    def test_benchmark_does_not_materialize_unbounded_responses(self):
        source = self.text("scripts/benchmark-http.py")
        self.assertIn("def drain_bounded(response)", source)
        self.assertIn("MAX_RESPONSE_BYTES + 1", source)
        self.assertNotIn("response.read()", source)
        self.assertNotIn("error.read()", source)


if __name__ == "__main__":
    unittest.main()
