from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
MAKEFILE = (ROOT / "Makefile").read_text()
WAITER = ROOT / "scripts/deploy.sh"
WAITER_TEXT = WAITER.read_text()


class MakeDeployContract(unittest.TestCase):
    def test_shell_syntax(self) -> None:
        subprocess.run(["bash", "-n", str(WAITER)], check=True)

    def test_make_deploy_is_the_guarded_entrypoint(self) -> None:
        self.assertIn("deploy:\n\tbash scripts/deploy.sh", MAKEFILE)
        self.assertIn('wait_for_workflow "CI" "CI"', WAITER_TEXT)
        self.assertIn('wait_for_workflow "Publish container images" "IMAGES"', WAITER_TEXT)
        self.assertIn('scripts/deploy-production-safe.sh', WAITER_TEXT)
        self.assertIn('origin/main mismatch', WAITER_TEXT)

    def test_crowdrelay_deploy_cannot_touch_control_plane_tunnel(self) -> None:
        for token in (
            "CONTROL_PLANE_TUNNEL_BASELINE=PASS",
            "CONTROL_PLANE_TUNNEL_PRESERVATION=PASS",
            "{{.Id}}|{{.State.StartedAt}}|{{.RestartCount}}|{{.State.Status}}",
            "CrowdRelay deploy touched Control Plane tunnel",
            "crowdrelay-control-plane-virya-area-tunnel-1",
        ):
            self.assertIn(token, WAITER_TEXT)


if __name__ == "__main__":
    unittest.main()
