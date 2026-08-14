from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

class FanPushDeliveryV1Contract(unittest.TestCase):
    def text(self, rel):
        return (ROOT / rel).read_text()

    def test_schema_and_public_capability_are_published(self):
        meta = self.text('crates/crowdrelay-api/src/meta.rs')
        self.assertIn('SCHEMA_VERSION: u32 = 51', meta)
        self.assertIn('"fan_push_delivery_v1"', meta)
        migration = self.text('migrations/0051_fan_push_delivery.sql')
        self.assertIn("'push_delivery_enabled'", migration)
        self.assertIn("'provider_accepted'", migration)
        self.assertIn("'ambiguous'", migration)

    def test_provider_acceptance_is_not_delivery(self):
        repo = self.text('crates/crowdrelay-worker/src/push_delivery/repository.rs')
        api = self.text('crates/crowdrelay-api/src/push.rs')
        self.assertIn('provider_accepted', repo)
        self.assertIn('ack_token_hash', repo)
        self.assertIn('ack_token', api)
        self.assertIn("status = 'delivered'", api)
        self.assertIn('digest($3', api)

    def test_crash_retry_semantics_are_fail_closed(self):
        repo = self.text('crates/crowdrelay-worker/src/push_delivery/repository.rs')
        self.assertIn("status = 'claimed'", repo)
        self.assertIn('provider_started_at IS NULL', repo)
        self.assertIn("status = 'provider_started'", repo)
        self.assertIn("'ambiguous'", repo)

    def test_runtime_and_db_kill_switch_are_both_required(self):
        api = self.text('crates/crowdrelay-api/src/push.rs')
        worker_main = self.text('crates/crowdrelay-worker/src/main.rs')
        repository = self.text('crates/crowdrelay-worker/src/push_delivery/repository.rs')
        self.assertIn('runtime_enabled', api)
        self.assertIn('push_delivery_enabled', api)
        self.assertIn('config.push_delivery.runtime_enabled', worker_main)
        self.assertIn('feature_enabled', repository)

    def test_worker_is_supervised(self):
        main = self.text('crates/crowdrelay-worker/src/main.rs')
        self.assertIn('push delivery', main.lower())
        self.assertIn('PushDeliveryWorker', main)

    def test_web_push_has_endpoint_allowlist_and_no_redirects(self):
        api = self.text('crates/crowdrelay-api/src/push.rs')
        providers = self.text('crates/crowdrelay-worker/src/push_delivery/providers.rs')
        self.assertIn('valid_web_push_endpoint', api)
        self.assertIn('.redirect(Policy::none())', providers)

if __name__ == '__main__':
    unittest.main()
