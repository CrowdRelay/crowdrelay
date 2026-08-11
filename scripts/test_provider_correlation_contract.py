#!/usr/bin/env python3
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class ProviderCorrelationContract(unittest.TestCase):
    def test_provider_correlation_is_backed_by_execution_ledger(self):
        runtime = (ROOT / "crates/crowdrelay-infra/src/autopilot/runtime.rs").read_text()
        self.assertIn("async fn find_provider_action", runtime)
        self.assertIn("viryaos_autopilot_execution_reports report", runtime)
        self.assertIn("report.provider_reference=$3", runtime)
        self.assertIn("report.status='succeeded'", runtime)

    def test_lookup_has_a_targeted_partial_index(self):
        migration = (ROOT / "migrations/0042_viryaos_provider_correlation.sql").read_text()
        self.assertIn("viryaos_autopilot_execution_reports_provider_ref_idx", migration)
        self.assertIn("executor_id", migration)
        self.assertIn("provider_reference", migration)
        self.assertIn("status = 'succeeded'", migration)

    def test_openapi_exposes_only_internal_lookup(self):
        spec = (ROOT / "openapi/openapi.yaml").read_text()
        self.assertIn("/internal/autopilot/provider-actions/{provider_reference}", spec)
        self.assertIn("X-Virya-Executor-Id", spec)
        self.assertIn("ProviderActionCorrelation", spec)


if __name__ == "__main__":
    unittest.main()
