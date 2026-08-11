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

    def test_external_evidence_waits_for_provider_receipt(self):
        actions = (ROOT / 'crates/crowdrelay-infra/src/autopilot/actions.rs').read_text()
        execution = (ROOT / 'crates/crowdrelay-infra/src/autopilot/execution.rs').read_text()
        runtime = (ROOT / 'crates/crowdrelay-infra/src/autopilot/runtime.rs').read_text()
        snapshots = (ROOT / 'crates/crowdrelay-infra/src/autopilot/operations/snapshots.rs').read_text()
        self.assertIn('payload_requires_executor', execution)
        self.assertIn('if !payload_requires_executor(&action.payload)', actions)
        succeeded = runtime[runtime.index('ExecutorReportStatus::Succeeded =>'):runtime.index('ExecutorReportStatus::Accepted | ExecutorReportStatus::Executing')]
        self.assertIn('schedule_effect_measurement', succeeded)
        self.assertIn('record_execution_outcome', succeeded)
        self.assertIn("SET status='submitted', version=version+1", succeeded)
        self.assertIn("SET package_status='ready', status='prepared', version=version+1", succeeded)
        self.assertIn("report.status = 'succeeded'", snapshots)
        self.assertIn("report.status IN ('succeeded','failed')", snapshots)

    def test_worker_phases_fail_independently(self):
        worker = (ROOT / 'crates/crowdrelay-worker/src/autopilot.rs').read_text()
        self.assertIn('let mut phase_failed = false', worker)
        self.assertIn('ViryaOS Autopilot evaluation failed', worker)
        self.assertIn('ViryaOS Autopilot action claim failed', worker)
        self.assertIn('ViryaOS Autopilot measurement claim failed', worker)
        evaluation_failure = worker[worker.index('ViryaOS Autopilot evaluation failed'):worker.index('claim_due_actions')]
        self.assertNotIn('return Err', evaluation_failure)

    def test_optional_deadline_calendar_never_blocks_primary_provider_action(self):
        execution = (ROOT / 'crates/crowdrelay-infra/src/autopilot/operations/execution.rs').read_text()
        deadline = execution[execution.index('async fn seed_deadline_calendar'):execution.index('pub(in crate::autopilot) async fn execute_live_opportunity')]
        release = execution[execution.index('async fn seed_release_calendar'):execution.index('async fn execute_release_campaign')]
        self.assertIn('Err(RepositoryError::Unavailable) => return Ok(())', deadline)
        self.assertNotIn('Err(RepositoryError::Unavailable) => return Ok(())', release)


    def test_provider_success_dominates_delayed_failure_receipts(self):
        runtime = (ROOT / 'crates/crowdrelay-infra/src/autopilot/runtime.rs').read_text()
        control = (ROOT / 'crates/crowdrelay-infra/src/autopilot/control.rs').read_text()
        chief = (ROOT / 'crates/crowdrelay-infra/src/autopilot/operations/chief.rs').read_text()
        failed_arm = runtime[runtime.index('ExecutorReportStatus::Failed =>'):runtime.index('ExecutorReportStatus::Succeeded =>')]
        self.assertIn('provider_already_succeeded', failed_arm)
        self.assertIn("report.status='succeeded'", failed_arm)
        self.assertIn('if provider_already_succeeded', failed_arm)
        self.assertIn('NOT EXISTS (', control)
        self.assertIn("success.status=\'succeeded\'", control)
        self.assertIn("success.status=\'succeeded\'", chief)

    def test_execution_plane_hardening_index_and_delayed_receipt_window(self):
        migration = (ROOT / 'migrations/0041_viryaos_execution_plane_hardening.sql').read_text()
        api = (ROOT / 'crates/crowdrelay-api/src/autopilot/runtime.rs').read_text()
        self.assertIn('viryaos_autopilot_action_emissions_action_idx', migration)
        self.assertIn('(workspace_id, action_id, emitted_at DESC)', migration)
        self.assertIn('viryaos_autopilot_actions_status_finished_idx', migration)
        self.assertIn("WHERE status IN ('succeeded', 'failed')", migration)
        self.assertIn('MAX_EXECUTION_REPORT_AGE: Duration = Duration::days(7)', api)
        report_validation = api[api.index('pub async fn execution_report'):api.index('pub async fn executor_heartbeat')]
        self.assertIn('MAX_EXECUTION_REPORT_AGE', report_validation)

if __name__ == '__main__':
    unittest.main()
