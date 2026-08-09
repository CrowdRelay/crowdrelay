#!/usr/bin/env python3
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

class SynesthesiaEcosystemContract(unittest.TestCase):
    def test_public_ledger_is_additive_and_no_shipping_pii(self):
        migration = (ROOT / 'migrations/0030_synesthesia_ecosystem.sql').read_text()
        api = (ROOT / 'crates/crowdrelay-api/src/synesthesia.rs').read_text()
        router = (ROOT / 'crates/crowdrelay-api/src/lib.rs').read_text()
        for path in (
            '/v1/public/synesthesia/runs',
            '/v1/public/synesthesia/runs/{run_id}/rooms/{room_id}',
            '/v1/public/synesthesia/runs/{run_id}/complete',
            '/v1/public/synesthesia/reward-claims',
        ):
            self.assertIn(path, router)
        self.assertIn('synesthesia_runs', migration)
        self.assertIn('synesthesia_reward_entries', migration)
        self.assertNotIn('shipping_address', migration.lower())
        self.assertNotIn('postal_code', migration.lower())
        self.assertNotIn('outbox_events', api)
        self.assertNotIn('fan_consents', api)
        self.assertNotIn('marketing_consent', api)
        self.assertNotIn('city_slug', api)
        self.assertIn("status = 'scheduled'", api)
        self.assertIn('opens_at <= now()', api)
        self.assertIn('closes_at > now()', api)
        self.assertIn('reward_draws_synesthesia_live_ref_uidx', migration)

    def test_draw_is_fixed_to_five_equal_entries(self):
        validation = (ROOT / 'crates/crowdrelay-api/src/commerce/validation.rs').read_text()
        worker = (ROOT / 'crates/crowdrelay-worker/src/draws.rs').read_text()
        for text in (validation, worker):
            self.assertIn('synesthesia_completion', text)
        self.assertIn('payload.winner_count != 5', validation)
        self.assertIn('payload.units_per_winner != 1', validation)
        self.assertIn('payload.base_entries != 1', validation)
        self.assertIn('payload.entries_per_referral != 0', validation)
        self.assertIn('payload.entries_per_checkin != 0', validation)
        self.assertIn('payload.max_entries != 1', validation)
        self.assertIn('synesthesia_reward_entries', worker)

    def test_v4_handoff_is_idempotent_and_identity_safe(self):
        api = (ROOT / 'crates/crowdrelay-api/src/synesthesia.rs').read_text()
        migration = (ROOT / 'migrations/0032_fan_context_synesthesia_handoff.sql').read_text()
        router = (ROOT / 'crates/crowdrelay-api/src/lib.rs').read_text()
        self.assertIn('/v1/me/synesthesia/link', router)
        self.assertIn('handoff_token_hash', migration)
        self.assertIn("AND (fan_id IS NULL OR fan_id = $3)", api)
        # A successful link keeps the short-lived hash until expiry so a lost
        # 200 response can be retried by the same fan without re-binding it.
        link_block = api.split('pub async fn link_completed_run_to_fan', 1)[1].split('pub async fn enter_reward_draw', 1)[0]
        self.assertNotIn('handoff_token_hash = NULL', link_block)
        self.assertIn("fan.status = 'active'", link_block)
        # Reward entry must never diverge from an already-bound run identity.
        reward_block = api.split('async fn enter_reward_draw_inner', 1)[1]
        self.assertIn('if linked_run.rows_affected() != 1', reward_block)
        self.assertIn('return Err(SynesthesiaError::Conflict);', reward_block)

    def test_openapi_documents_the_game_contract(self):
        spec = (ROOT / 'openapi/openapi.yaml').read_text()
        self.assertIn('/public/synesthesia/runs:', spec)
        self.assertIn('/public/synesthesia/reward-claims:', spec)
        self.assertIn('SynesthesiaRewardEntryRequest:', spec)
        self.assertIn('synesthesia_completion', spec)
        self.assertIn('eligibility_ref:', spec)

if __name__ == '__main__':
    unittest.main()
