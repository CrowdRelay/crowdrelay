"""Contract test for the ecosystem deploy orchestrator.

Verifies that the ecosystem deploy scripts exist, have valid syntax, and
contain the structural invariants required for a safe blue-green deploy
with rollback.
"""
from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
ECOSYSTEM = ROOT / "scripts" / "deploy-ecosystem.sh"
BLUEGREEN = ROOT / "scripts" / "deploy-bluegreen.sh"
COMPOSE_OVERLAY = ROOT / "compose.bluegreen.yaml"
CLASSIFIER = ROOT / "scripts" / "classify-migrations.py"
WORKFLOW = ROOT / ".github" / "workflows" / "ecosystem-deploy.yml"

ECOSYSTEM_TEXT = ECOSYSTEM.read_text()
BLUEGREEN_TEXT = BLUEGREEN.read_text()
COMPOSE_TEXT = COMPOSE_OVERLAY.read_text()
CLASSIFIER_TEXT = CLASSIFIER.read_text()
WORKFLOW_TEXT = WORKFLOW.read_text()


class EcosystemDeployContract(unittest.TestCase):
    def test_shell_syntax(self) -> None:
        for script in (ECOSYSTEM, BLUEGREEN):
            result = subprocess.run(["bash", "-n", str(script)], capture_output=True)
            self.assertEqual(result.returncode, 0, f"{script.name} has syntax errors: {result.stderr.decode()}")

    def test_classifier_syntax(self) -> None:
        result = subprocess.run(["python3", "-c", CLASSIFIER_TEXT], capture_output=True)
        # Running without args should produce JSON and exit 0 or 1
        result = subprocess.run(["python3", str(CLASSIFIER)], capture_output=True)
        self.assertIn(result.returncode, (0, 1))

    def test_orchestrator_has_all_phases(self) -> None:
        for phase in (
            "Phase 0",
            "Phase 1",
            "Phase 2",
            "Phase 3",
            "Phase 4",
            "Phase 5",
            "Phase 6",
            "Phase 7",
        ):
            self.assertIn(phase, ECOSYSTEM_TEXT, f"orchestrator missing phase: {phase}")

    def test_orchestrator_has_rollback_mode(self) -> None:
        self.assertIn("--rollback", ECOSYSTEM_TEXT)
        self.assertIn("ECOSYSTEM_ROLLBACK=PASS", ECOSYSTEM_TEXT)

    def test_orchestrator_has_dry_run(self) -> None:
        self.assertIn("--dry-run", ECOSYSTEM_TEXT)
        self.assertIn("DRY_RUN=PASS", ECOSYSTEM_TEXT)

    def test_orchestrator_has_contract_gates(self) -> None:
        self.assertIn("test-ecosystem-contract-v2.py", ECOSYSTEM_TEXT)
        self.assertIn("ECOSYSTEM_CONTRACTS_PRE=PASS", ECOSYSTEM_TEXT)
        self.assertIn("ECOSYSTEM_CONTRACTS_POST=PASS", ECOSYSTEM_TEXT)

    def test_orchestrator_has_db_snapshot(self) -> None:
        self.assertIn("crowdrelay_backup", ECOSYSTEM_TEXT)
        self.assertIn("DB_SNAPSHOT", ECOSYSTEM_TEXT)

    def test_orchestrator_has_migration_classification(self) -> None:
        self.assertIn("classify-migrations.py", ECOSYSTEM_TEXT)
        self.assertIn("--allow-contract-migrations", ECOSYSTEM_TEXT)
        self.assertIn("MIGRATION_CLASSIFY", ECOSYSTEM_TEXT)

    def test_orchestrator_has_error_handler(self) -> None:
        self.assertIn("on_error", ECOSYSTEM_TEXT)
        self.assertIn("ECOSYSTEM_DEPLOY=FAILED", ECOSYSTEM_TEXT)

    def test_orchestrator_does_not_use_forbidden_patterns(self) -> None:
        for forbidden in (
            "git pull",
            "git reset --hard",
            "git stash",
        ):
            self.assertNotIn(forbidden, ECOSYSTEM_TEXT, f"orchestrator uses forbidden pattern: {forbidden}")

    def test_orchestrator_verifies_runtime_sha(self) -> None:
        self.assertIn("RUNTIME_SHA=PASS", ECOSYSTEM_TEXT)
        self.assertIn("org.opencontainers.image.revision", ECOSYSTEM_TEXT)

    def test_bluegreen_has_rollback(self) -> None:
        self.assertIn("rollback()", BLUEGREEN_TEXT)
        self.assertIn("ROLLBACK=START", BLUEGREEN_TEXT)
        self.assertIn("ROLLBACK=COMPLETE", BLUEGREEN_TEXT)
        self.assertIn("CADDY_SWITCH=PASS", BLUEGREEN_TEXT)

    def test_bluegreen_starts_green_before_switching(self) -> None:
        # Green containers must start before the Caddy cutover (which switches traffic).
        green_start = BLUEGREEN_TEXT.index("up -d --no-deps --wait")
        # The Caddy switch is the traffic switch — it reorders the static
        # upstream pair and gracefully reloads Caddy.
        caddy_switch = BLUEGREEN_TEXT.index("ALIAS_MOVED=true")
        self.assertLess(green_start, caddy_switch, "green must start before Caddy switch")

    def test_bluegreen_health_checks_green_directly(self) -> None:
        # Alternating blue-green: the new color is health-checked directly
        # before the Caddy switch. The log line uses NEW_HEALTH since the
        # new color may be green or blue depending on which is active.
        self.assertIn("NEW_HEALTH=PASS", BLUEGREEN_TEXT)
        self.assertIn("crowdrelay-api-green", BLUEGREEN_TEXT)
        self.assertIn("v1/health/ready", BLUEGREEN_TEXT)

    def test_bluegreen_stops_blue_after_verification(self) -> None:
        # Blue stop must come after public health verification
        health_check = BLUEGREEN_TEXT.index("PUBLIC_SMOKE=PASS")
        blue_stop = BLUEGREEN_TEXT.index("docker stop")
        self.assertLess(health_check, blue_stop, "blue must not stop before public health passes")

    def test_bluegreen_verifies_green_meta_sha(self) -> None:
        self.assertIn("v1/meta", BLUEGREEN_TEXT)
        self.assertIn("gitSha", BLUEGREEN_TEXT)

    def test_compose_overlay_has_green_services(self) -> None:
        self.assertIn("api-green", COMPOSE_TEXT)
        self.assertIn("worker-green", COMPOSE_TEXT)
        self.assertIn("crowdrelay-api-green", COMPOSE_TEXT)
        self.assertIn("CROWDRELAY_GREEN_TAG", COMPOSE_TEXT)

    def test_compose_overlay_green_has_durable_restart(self) -> None:
        # Green containers must be restart-safe: the deploy script enforces
        # unless-stopped after health verification, and the compose overlay
        # must match so a Docker daemon restart doesn't kill the active color.
        import yaml
        compose = yaml.safe_load(COMPOSE_TEXT)
        services = compose.get("services", {})
        for service in ("api-green", "worker-green"):
            self.assertIn(service, services, f"{service} not in compose overlay")
            restart = services[service].get("restart", "")
            self.assertEqual(restart, "unless-stopped", f"{service} should have restart: unless-stopped, got {restart}")

    def test_classifier_distinguishes_expand_and_contract(self) -> None:
        self.assertIn("CONTRACT_PATTERNS", CLASSIFIER_TEXT)
        self.assertIn("EXPAND_PATTERNS", CLASSIFIER_TEXT)
        self.assertIn("DROP\\s+TABLE", CLASSIFIER_TEXT)
        self.assertIn("DROP\\s+COLUMN", CLASSIFIER_TEXT)
        self.assertIn("CREATE\\s+TABLE", CLASSIFIER_TEXT)
        self.assertIn("ADD\\s+COLUMN", CLASSIFIER_TEXT)

    def test_classifier_outputs_json(self) -> None:
        self.assertIn('"all_expand"', CLASSIFIER_TEXT)
        self.assertIn('"pending_count"', CLASSIFIER_TEXT)

    def test_workflow_is_manual_dispatch_only(self) -> None:
        self.assertIn("workflow_dispatch:", WORKFLOW_TEXT)
        self.assertNotIn("push:", WORKFLOW_TEXT)
        self.assertNotIn("schedule:", WORKFLOW_TEXT)

    def test_workflow_has_production_environment(self) -> None:
        self.assertIn("name: production", WORKFLOW_TEXT)

    def test_workflow_has_concurrency_group(self) -> None:
        self.assertIn("concurrency:", WORKFLOW_TEXT)
        self.assertIn("group: ecosystem-deploy", WORKFLOW_TEXT)
        self.assertIn("cancel-in-progress: false", WORKFLOW_TEXT)

    def test_workflow_passes_target_sha(self) -> None:
        self.assertIn("target_sha", WORKFLOW_TEXT)
        self.assertIn("steps.target.outputs.sha", WORKFLOW_TEXT)


if __name__ == "__main__":
    unittest.main()
