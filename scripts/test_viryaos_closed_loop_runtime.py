from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

class ViryaOsClosedLoopRuntime(unittest.TestCase):
    def test_runtime_migration_and_ports_exist(self):
        migration = (ROOT / 'migrations/0040_viryaos_closed_loop_runtime.sql').read_text()
        app = (ROOT / 'crates/crowdrelay-application/src/autopilot/control.rs').read_text()
        for token in ('viryaos_executor_instances','viryaos_executor_circuit_breakers','viryaos_autopilot_execution_reports','viryaos_contact_governor','viryaos_release_components','viryaos_rum_samples','approval_expires_at','guarded_until'):
            self.assertIn(token, migration)
        self.assertIn('trait AutopilotRuntimeRepository', app)
        self.assertIn('ReleaseLedgerOverview', app)
        self.assertIn('RumMetricSummary', app)

    def test_executor_gate_bootstraps_then_fails_closed(self):
        infra = (ROOT / 'crates/crowdrelay-infra/src/autopilot/execution.rs').read_text()
        runtime = (ROOT / 'crates/crowdrelay-infra/src/autopilot/runtime.rs').read_text()
        self.assertIn('ensure_executor_capability', infra)
        self.assertIn('viryaos_executor_instances', infra)
        self.assertIn('RepositoryError::Unavailable', infra)
        self.assertIn('heartbeat_write.rows_affected() != 1', runtime)
        self.assertIn("'n8n','production'", runtime)
        self.assertIn('viryaos_executor_circuit_breakers', infra)
        self.assertIn("INTERVAL '15 minutes'", runtime)
        self.assertIn('last_failure_at <= EXCLUDED.last_failure_at', runtime)
        self.assertIn('last_failure_at <= $3', runtime)
        self.assertIn('guarded_executor_count', runtime)

    def test_feedback_loop_and_contact_governor(self):
        measurement = (ROOT / 'crates/crowdrelay-infra/src/autopilot/measurement.rs').read_text()
        actions = (ROOT / 'crates/crowdrelay-infra/src/autopilot/actions.rs').read_text()
        execution = (ROOT / 'crates/crowdrelay-infra/src/autopilot/execution.rs').read_text()
        self.assertIn('two_consecutive_worsened_effects', measurement)
        self.assertIn("INTERVAL '7 days'", measurement)
        self.assertIn('approval_expired', actions)
        self.assertIn('reserve_contact_window', execution)
        self.assertIn('last_action_id uuid,', (ROOT / 'migrations/0040_viryaos_closed_loop_runtime.sql').read_text())
        chief = (ROOT / 'crates/crowdrelay-infra/src/autopilot/operations/chief.rs').read_text()
        ingress = (ROOT / 'crates/crowdrelay-infra/src/autopilot/operations/ingress.rs').read_text()
        self.assertIn("SELECT $1, lower(btrim(contact_email)), 'booking', NULL", chief)
        self.assertIn("SELECT $1, lower(btrim(contact_email)), 'outreach', NULL", ingress)

    def test_release_ledger_reporting_is_fail_open_after_verified_deploy(self):
        ctl = (ROOT / 'crowdrelayctl').read_text()
        helper = (ROOT / 'scripts/report-release-component.sh').read_text()
        self.assertIn('report_release_ledger', ctl)
        self.assertIn('Release ledger reporter failed for $component; production deploy remains valid.', ctl)
        self.assertIn('for component in crowdrelay-api crowdrelay-worker', ctl)
        self.assertIn('/v1/internal/autopilot/release-components', helper)

    def test_server_smoke_preserves_bounded_alerting(self):
        smoke = (ROOT / 'scripts/production-smoke.sh').read_text()
        service = (ROOT / 'deploy/systemd/virya-production-smoke.service').read_text()
        self.assertIn('ALERT_COOLDOWN_SECONDS', smoke)
        self.assertIn('production smoke recovered', smoke)
        self.assertIn('StateDirectory=virya-production-smoke', service)

    def test_public_rum_has_no_identity_fields(self):
        migration = (ROOT / 'migrations/0040_viryaos_closed_loop_runtime.sql').read_text()
        rum = migration[migration.index('CREATE TABLE viryaos_rum_samples'):]
        for token in ('user_id','email','session_id','ip_address','fingerprint'):
            self.assertNotIn(token, rum)
        runtime = (ROOT / 'crates/crowdrelay-infra/src/autopilot/runtime.rs').read_text()
        api = (ROOT / 'crates/crowdrelay-api/src/autopilot/runtime.rs').read_text()
        self.assertIn('percentile_cont(0.75)', runtime)
        self.assertIn('encoded.len() <= 2_048', api)

if __name__ == '__main__':
    unittest.main()
