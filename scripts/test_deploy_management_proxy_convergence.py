#!/usr/bin/env python3
"""Regression guard for transient Control Plane 5xx during proxy convergence."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = (ROOT / "scripts/deploy-production-safe.sh").read_text(encoding="utf-8")


class DeployManagementProxyConvergenceContract(unittest.TestCase):
    def test_cross_system_gate_retries_only_transient_gateway_failures(self) -> None:
        self.assertIn("for attempt in $(seq 1 30)", SCRIPT)
        self.assertIn("^(502|503|504)$", SCRIPT)
        self.assertIn("CONTROL_PLANE_CROSS_GATE_RETRY", SCRIPT)
        self.assertIn("CONTROL_PLANE_CROSS_GATE_READINESS=PASS", SCRIPT)
        self.assertIn("did not converge after transient management-proxy failures", SCRIPT)

    def test_non_transient_http_failures_still_fail_fast(self) -> None:
        self.assertIn('if [[ ! "$code" =~ ^(502|503|504)$ ]]', SCRIPT)
        self.assertIn('fail "Control Plane cross-system gate failed status=$code detail=$detail"', SCRIPT)


if __name__ == "__main__":
    unittest.main()
