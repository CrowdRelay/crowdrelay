import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]

def read(path: str) -> str:
    return (ROOT / path).read_text()

class BeaconPrincipalHardening(unittest.TestCase):
    def test_release_transition_policy_allows_closed_sent_delivery_only(self):
        domain = read('crates/crowdrelay-domain/src/beacon_release.rs')
        admin = read('crates/crowdrelay-api/src/beacon_signal/releases/admin.rs')
        self.assertIn('Closed => matches!((self, next), (Sent, Delivered))', domain)
        self.assertIn('(Sent, Delivered)', domain)
        self.assertIn('current_state.can_transition_to(next_state, campaign_state)', admin)
        self.assertIn("campaign.status IN ('open','closed')", admin)

    def test_expired_release_claims_reconcile_inventory(self):
        steps = read('crates/crowdrelay-worker/src/retention/steps.rs')
        retention = read('crates/crowdrelay-worker/src/retention.rs')
        self.assertIn('reconcile_expired_beacon_release_claims', steps)
        self.assertIn("SET status='expired'", steps)
        self.assertIn('inventory_reservation_items', steps)
        self.assertIn('expired_beacon_release_claims_reconciled', retention)

    def test_invite_claims_have_lease_and_stale_claims_fail_closed(self):
        migration = read('migrations/0060_beacon_release_and_network_hardening.sql')
        internal = read('crates/crowdrelay-api/src/beacon_signal/network/internal.rs')
        steps = read('crates/crowdrelay-worker/src/retention/steps.rs')
        self.assertIn('claim_expires_at timestamptz', migration)
        self.assertIn("claim_expires_at=now()+interval '60 minutes'", internal)
        self.assertIn("job.status='claimed' AND job.claim_expires_at <= now()", steps)
        self.assertIn("status='ambiguous'", steps)
        self.assertIn('claim_expires_at,reported_at', read('crates/crowdrelay-api/src/beacon_signal/network/admin.rs'))

    def test_discovery_is_concurrency_safe_and_provenance_is_per_run(self):
        migration = read('migrations/0060_beacon_release_and_network_hardening.sql')
        internal = read('crates/crowdrelay-api/src/beacon_signal/network/internal.rs')
        self.assertIn('viryaos_beacon_network_discovery_observations', migration)
        self.assertIn('pg_advisory_xact_lock(hashtextextended($1,0))', internal)
        self.assertIn('format!("email:{email}")', internal)
        self.assertIn('format!("destination:{url}")', internal)
        self.assertIn('{identity_key}', internal)
        self.assertIn('SELECT count(*)::bigint FROM viryaos_beacon_network_discovery_observations', internal)
        self.assertIn('RETURNING discovered_count', internal)
        self.assertIn('canonical_count=discovered_count', internal)

    def test_valid_invite_is_not_silently_rotated(self):
        lifecycle = read('crates/crowdrelay-api/src/beacon_signal/lifecycle.rs')
        admin = read('crates/crowdrelay-api/src/beacon_signal/network/admin.rs')
        needle = "profile.status='invited' AND profile.invite_expires_at > now()"
        self.assertIn(needle, lifecycle)
        self.assertIn(needle, admin)

    def test_terminal_invite_replay_binds_provider_summary(self):
        internal = read('crates/crowdrelay-api/src/beacon_signal/network/internal.rs')
        self.assertIn('provider_summary', internal)
        self.assertIn('SELECT status,claim_token_hash,provider_summary', internal)
        self.assertIn('current.2 != payload.provider_summary', internal)

    def test_closed_campaign_sent_recipients_remain_visible_to_staff(self):
        source = read('crates/crowdrelay-api/src/beacon_signal/releases/admin.rs')
        self.assertIn("campaign.status='open' OR recipient.status IN ('confirmed','prepared','sent')", source)

    def test_release_launch_bulk_materializes_outbox(self):
        source = read('crates/crowdrelay-api/src/beacon_signal/releases/admin.rs')
        self.assertIn('FROM unnest(', source)
        self.assertIn('AS mail(beacon_id,display_name,contact_email,subject,body_text,request_id)', source)
        self.assertNotIn('for (beacon_id, display_name, contact_email, locale) in &eligible {\n        let delivery = release_delivery_copy(locale, display_name, &campaign.2, campaign.3);\n        if let Err(error) = sqlx::query(', source)

    def test_openapi_has_structural_network_and_release_read_models(self):
        spec = read('openapi/openapi.yaml')
        self.assertIn('BeaconNetworkAcquisition:', spec)
        self.assertIn('BeaconReleaseCampaignsAdmin:', spec)
        self.assertIn('BeaconMemberReleaseCampaigns:', spec)
        self.assertIn("schema: { $ref: '#/components/schemas/BeaconNetworkAcquisition' }", spec)
        self.assertIn("schema: { $ref: '#/components/schemas/BeaconReleaseCampaignsAdmin' }", spec)
        self.assertIn('claimExpiresAt:', spec)

if __name__ == '__main__':
    unittest.main()
