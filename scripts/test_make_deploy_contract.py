from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
JUSTFILE = (ROOT / "justfile").read_text()
WAITER = ROOT / "scripts/deploy.sh"
SAFE = ROOT / "scripts/deploy-production-safe.sh"
WAITER_TEXT = WAITER.read_text()
SAFE_TEXT = SAFE.read_text()


class MakeDeployContract(unittest.TestCase):
    def test_shell_syntax(self) -> None:
        subprocess.run(["bash", "-n", str(WAITER)], check=True)
        subprocess.run(["bash", "-n", str(SAFE)], check=True)

    def test_just_deploy_is_the_guarded_entrypoint(self) -> None:
        # The task runner migrated from make to just; the guarded deploy
        # chain must survive the rename.
        self.assertIn("deploy:\n    bash scripts/deploy.sh", JUSTFILE)
        self.assertIn('wait_for_workflow "CI" "CI"', WAITER_TEXT)
        self.assertIn("wait_for_image_release", WAITER_TEXT)
        self.assertNotIn('wait_for_workflow "Publish container images" "IMAGES"', WAITER_TEXT)
        self.assertIn('artifact_name="crowdrelay-image-digests-${TARGET}"', WAITER_TEXT)
        self.assertIn('actions/artifacts?name=${artifact_name}', WAITER_TEXT)
        self.assertIn("select(.expired == false)", WAITER_TEXT)
        self.assertIn("IMAGES_ARTIFACT=%s", WAITER_TEXT)
        self.assertIn('scripts/deploy-production-safe.sh', WAITER_TEXT)
        self.assertIn('origin/main mismatch', WAITER_TEXT)
        self.assertIn('still waiting for %s run', WAITER_TEXT)

    def test_deploy_chain_does_not_depend_on_executable_bits(self) -> None:
        self.assertNotIn('[[ -x "$CANONICAL" ]]', WAITER_TEXT)
        self.assertNotIn('[[ -x "$CANONICAL" ]]', SAFE_TEXT)
        self.assertIn('[[ -f "$CANONICAL" && ! -L "$CANONICAL" ]]', WAITER_TEXT)
        self.assertIn('[[ -f "$CANONICAL" && ! -L "$CANONICAL" ]]', SAFE_TEXT)
        self.assertIn('bash "$CANONICAL" "$TARGET"', WAITER_TEXT)
        self.assertIn('bash "$CANONICAL" "$TARGET"', SAFE_TEXT)

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
