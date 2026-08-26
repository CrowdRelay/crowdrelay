#!/usr/bin/env python3
from pathlib import Path
import unittest

from rust_source_tree import read_rust_module

ROOT = Path(__file__).resolve().parents[1]

class SynesthesiaEcosystemContract(unittest.TestCase):
    def test_public_ledger_is_additive_and_no_shipping_pii(self):
        migration = (ROOT / 'migrations/0030_synesthesia_ecosystem.sql').read_text()
        api = read_rust_module(ROOT, 'crates/crowdrelay-api/src/synesthesia.rs')
        # The link route moved behind the optional-module gate inside the
        # synesthesia module, so the search spans all three mount points.
        router = (
            (ROOT / 'crates/crowdrelay-api/src/lib.rs').read_text()
            + (ROOT / 'crates/crowdrelay-api/src/routing.rs').read_text()
            + (ROOT / 'crates/crowdrelay-api/src/synesthesia.rs').read_text()
        )
        for path in (
            '/v1/public/synesthesia/runs',
            '/v1/public/synesthesia/runs/{run_id}/rooms/{room_id}',
            '/v1/public/synesthesia/runs/{run_id}/complete',
            '/v1/public/synesthesia/runs/{run_id}/recover',
            '/v1/public/synesthesia/runs/{run_id}/context',
            '/v1/public/synesthesia/runs/{run_id}/handoff',
            '/v1/public/synesthesia/reward-claims',
            '/v1/public/synesthesia/leaderboard',
            '/v1/public/synesthesia/runs/{run_id}/leaderboard',
        ):
            self.assertIn(path, router)
        self.assertIn('synesthesia_runs', migration)
        self.assertIn('synesthesia_reward_entries', migration)
        recovery = (ROOT / 'migrations/0048_synesthesia_noncompetitive_recovery.sql').read_text()
        self.assertIn('recovery_completed_at', recovery)
        self.assertIn('does not carry a fabricated elapsed time', recovery.lower())
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
        leaderboard_migration = (ROOT / 'migrations/0044_synesthesia_leaderboard.sql').read_text()
        self.assertIn('attempt_id', leaderboard_migration)
        self.assertIn('leaderboard_name', leaderboard_migration)
        self.assertNotIn('normalized_email', leaderboard_migration)

    def test_draw_is_fixed_to_five_equal_entries(self):
        validation = (ROOT / 'crates/crowdrelay-api/src/commerce/validation.rs').read_text()
        worker = read_rust_module(ROOT, 'crates/crowdrelay-worker/src/draws.rs')
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
        api = read_rust_module(ROOT, 'crates/crowdrelay-api/src/synesthesia.rs')
        migration = (ROOT / 'migrations/0032_fan_context_synesthesia_handoff.sql').read_text()
        # The link route moved behind the optional-module gate inside the
        # synesthesia module, so the search spans all three mount points.
        router = (
            (ROOT / 'crates/crowdrelay-api/src/lib.rs').read_text()
            + (ROOT / 'crates/crowdrelay-api/src/routing.rs').read_text()
            + (ROOT / 'crates/crowdrelay-api/src/synesthesia.rs').read_text()
        )
        self.assertIn('/v1/me/synesthesia/link', router)
        self.assertIn('handoff_token_hash', migration)
        self.assertIn("AND (fan_id IS NULL OR fan_id = $3)", api)
        # A successful link keeps the short-lived hash until expiry so a lost
        # 200 response can be retried by the same fan without re-binding it.
        link_block = api.split('pub async fn link_completed_run_to_fan', 1)[1].split('pub async fn enter_reward_draw', 1)[0]
        self.assertNotIn('handoff_token_hash = NULL', link_block)
        self.assertIn("fan.status = 'active'", link_block)
        # Linking one completed attempt also claims earlier anonymous completed
        # attempts from the same install. This keeps Signal account-best aligned
        # with the local Synesthesia PB without ever rebinding another fan's run.
        self.assertIn('RETURNING id, next_room_index, COALESCE(client_total_elapsed_ms, 0),', link_block)
        self.assertIn('install_hash, campaign_slug', link_block)
        self.assertIn('AND install_hash = $4', link_block)
        self.assertNotIn('$5', link_block.split('Consuming a handoff proves control', 1)[1].split('transaction.commit', 1)[0])
        self.assertIn('AND fan_id IS NULL', link_block)
        self.assertIn('campaign_slug != CAMPAIGN_SLUG', link_block)
        # Reward entry must never diverge from an already-bound run identity.
        reward_block = api.split('async fn enter_reward_draw_inner', 1)[1]
        self.assertIn('if linked_run.rows_affected() != 1', reward_block)
        self.assertIn('return Err(SynesthesiaError::Conflict);', reward_block)

    def test_legacy_recovery_cannot_enter_the_competitive_leaderboard(self):
        api = read_rust_module(ROOT, 'crates/crowdrelay-api/src/synesthesia.rs')
        recovery = api.split('pub async fn recover_run', 1)[1].split('async fn completion_response', 1)[0]
        leaderboard = (ROOT / 'crates/crowdrelay-api/src/synesthesia/leaderboard.rs').read_text()
        self.assertIn('recovery_completed_at', recovery)
        self.assertNotIn('client_total_elapsed_ms', recovery)
        self.assertNotIn('recovery_completed_at', leaderboard)
        self.assertGreaterEqual(leaderboard.count('completed_at IS NOT NULL'), 4)

    def test_openapi_documents_the_game_contract(self):
        spec = (ROOT / 'openapi/openapi.yaml').read_text()
        self.assertIn('/public/synesthesia/runs:', spec)
        self.assertIn('/public/synesthesia/reward-claims:', spec)
        self.assertIn('/public/synesthesia/leaderboard:', spec)
        self.assertIn('/public/synesthesia/runs/{run_id}/recover:', spec)
        self.assertIn('/public/synesthesia/runs/{run_id}/context:', spec)
        self.assertIn('/public/synesthesia/runs/{run_id}/handoff:', spec)
        self.assertIn('SynesthesiaRunRecoveryRequest:', spec)
        self.assertIn('SynesthesiaLeaderboardResponse:', spec)
        self.assertIn('SynesthesiaRewardEntryRequest:', spec)
        self.assertIn('synesthesia_completion', spec)
        self.assertIn('eligibility_ref:', spec)

if __name__ == '__main__':
    unittest.main()
