from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
DEPLOY_PATH = ROOT / "scripts/deploy.sh"
DEPLOY = DEPLOY_PATH.read_text()


class MakeDeployConvergenceRecoveryContract(unittest.TestCase):
    def test_shell_syntax(self) -> None:
        subprocess.run(["bash", "-n", str(DEPLOY_PATH)], check=True)

    def test_recovery_is_bounded_to_known_exact_sha_drift(self) -> None:
        for token in (
            "recover_exact_runtime_convergence",
            "runtime-already-exact-failure-is-not-convergence",
            "pin-mismatch",
            "effective-compose-not-exact",
            "RUNTIME_CONVERGENCE_RECOVERY=PASS",
            "Retrying canonical deploy once",
        ):
            self.assertIn(token, DEPLOY)
        self.assertEqual(DEPLOY.count("Retrying canonical deploy once"), 1)

    def test_recovery_force_recreates_only_api_and_worker(self) -> None:
        self.assertIn(
            'compose up -d --no-deps --force-recreate --wait --wait-timeout "${CROWDRELAY_DEPLOY_WAIT_TIMEOUT_SECONDS:-180}" api worker',
            DEPLOY,
        )
        self.assertNotIn("--force-recreate area-management-proxy", DEPLOY)
        self.assertIn("proxy=untouched", DEPLOY)

    def test_tunnel_is_fingerprinted_before_after_and_after_recovery(self) -> None:
        for token in (
            "CONTROL_PLANE_TUNNEL_BASELINE=PASS",
            "CONTROL_PLANE_TUNNEL_PRESERVATION=PASS",
            "CONTROL_PLANE_TUNNEL_RECOVERY_PRESERVATION=PASS",
            "CONTROL_PLANE_TUNNEL_FINAL=PASS",
        ):
            self.assertIn(token, DEPLOY)

    def test_recovery_revalidates_runtime_identity(self) -> None:
        self.assertIn("post-recovery-tag-mismatch", DEPLOY)
        self.assertIn("post-recovery-revision-mismatch", DEPLOY)
        self.assertIn("post-recovery-meta-mismatch", DEPLOY)
        self.assertIn("exact-runtime=true", DEPLOY)


if __name__ == "__main__":
    unittest.main()
