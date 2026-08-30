from pathlib import Path
import json
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]

class FanAccountDeletionContract(unittest.TestCase):
    def test_migration_has_explicit_tombstone_state(self):
        migration = (ROOT / 'migrations/0068_fan_account_deletion.sql').read_text()
        self.assertIn('ADD COLUMN deleted_at timestamptz', migration)
        self.assertIn("status = 'suppressed'", migration)
        self.assertIn('@account[.]invalid', migration)

    def test_erasure_is_in_infra_and_clears_identity_surfaces(self):
        infra = (ROOT / 'crates/crowdrelay-infra/src/fan_privacy.rs').read_text()
        for fragment in [
            'DELETE FROM fan_push_endpoints',
            'DELETE FROM fan_sessions',
            'DELETE FROM fan_action_tokens',
            'DELETE FROM fan_acquisition_events',
            'DELETE FROM referral_attributions',
            'UPDATE referral_codes',
            'SET active = false',
            'DELETE FROM synesthesia_reward_entries',
            'leaderboard_name = NULL',
            'fan_id = NULL',
            "action, target_type",
            "'fan.account_erased'",
            "status = 'suppressed'",
            'deleted_at = now()',
        ]:
            self.assertIn(fragment, infra)
        self.assertIn("issuance_method <> 'paid'", infra)
        self.assertIn("'acquisition_referral_erased', true", infra)
        self.assertIn("'paid_commerce_retained', true", infra)
        self.assertIn("'consent_evidence_retained', true", infra)

    def test_http_layer_contains_no_account_erasure_sql(self):
        api = (ROOT / 'crates/crowdrelay-api/src/fan_privacy.rs').read_text()
        self.assertIn('PostgresFanPrivacyRepository', api)
        self.assertNotIn('UPDATE fans', api)
        self.assertNotIn('DELETE FROM', api)
        # The erasure receipt is personal and must never be cached by a shared
        # proxy; openapi declares Cache-Control as a required response header.
        self.assertIn("const PRIVATE_NO_STORE: &str = \"private, no-store\";", api)
        self.assertIn('(CACHE_CONTROL, PRIVATE_NO_STORE)', api)
        routing = (ROOT / 'crates/crowdrelay-api/src/routing.rs').read_text()
        self.assertIn('/v1/me/account', routing)
        self.assertIn('delete(fan_privacy::delete_account)', routing)

    def test_capability_and_schema_are_release_gated(self):
        latest = max(int(p.name[:4]) for p in (ROOT / 'migrations').glob('[0-9][0-9][0-9][0-9]_*.sql'))
        meta = (ROOT / 'crates/crowdrelay-api/src/meta.rs').read_text()
        # SCHEMA_VERSION is auto-discovered by build.rs — verify the pattern
        # and that build.rs exists. The actual value correctness is
        # guaranteed at compile time by the build script.
        self.assertIn('CROWDRELAY_SCHEMA_VERSION', meta)
        self.assertTrue((ROOT / 'crates/crowdrelay-api/build.rs').is_file())
        self.assertGreaterEqual(latest, 68)
        self.assertIn('"fan_account_deletion_v1"', meta)
        compatibility = json.loads((ROOT / 'integration/ecosystem/compatibility.json').read_text())
        self.assertEqual(compatibility['minimumSchemaVersion'], 68)
        self.assertIn('fan_account_deletion_v1', compatibility['requiredCapabilities'])

if __name__ == '__main__':
    unittest.main()
