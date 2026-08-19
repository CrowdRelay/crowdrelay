from pathlib import Path
from rust_source_tree import read_rust_module
import unittest

ROOT = Path(__file__).resolve().parents[1]


class HttpMetricsSourceContract(unittest.TestCase):
    def test_client_and_server_errors_are_separate_operational_signals(self):
        metrics = (ROOT / 'crates/crowdrelay-api/src/http_metrics.rs').read_text()
        api = (ROOT / 'crates/crowdrelay-api/src/lib.rs').read_text()
        ops = read_rust_module(ROOT, 'crates/crowdrelay-api/src/ops.rs')
        openapi = (ROOT / 'openapi/openapi.yaml').read_text()

        self.assertIn('errors_4xx: AtomicU64', metrics)
        self.assertIn('if (400..500).contains(&status)', metrics)
        self.assertIn('crowdrelay_http_requests_4xx_total', api)
        self.assertIn('errors_4xx: snapshot.errors_4xx', ops)
        self.assertIn('required: [requests, errors_4xx, errors_5xx, average_ms, p50_ms, p95_ms]', openapi)
        self.assertNotIn('metrics_snapshot().await.unwrap_or_default()', api)
        self.assertIn('crowdrelay_ops_metrics_snapshot_available 0', api)
        self.assertIn('crowdrelay_ops_metrics_snapshot_available 1', api)
        self.assertIn(
            'tracing::warn!(error = ?error, "operational metrics snapshot unavailable")',
            api,
        )
        self.assertNotIn(
            'tracing::warn!(error = %error, "operational metrics snapshot unavailable")',
            api,
        )


if __name__ == '__main__':
    unittest.main()
