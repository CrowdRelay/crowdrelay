from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
DEPLOY = (ROOT / "scripts/deploy-production-exact.sh").read_text()
INSTALLER = (ROOT / "scripts/install-deploy-crowdrelay-wrapper.sh").read_text()
CTL = (ROOT / "crowdrelayctl").read_text()
PUBLISH = (ROOT / ".github/workflows/publish-images.yml").read_text()


class BoringProductionDeployContract(unittest.TestCase):
    def test_one_tracked_orchestrator_owns_the_release_path(self) -> None:
        for token in (
            "Production image gate",
            "Exact source sync",
            "Canonical crowdrelayctl deploy",
            "Public exact-SHA verification",
            "./crowdrelayctl pin",
            "./crowdrelayctl deploy",
            "git bundle create",
            "git fetch --no-tags",
            "git merge --ff-only",
            "OCI revision mismatch",
            "architecture=amd64",
            "github-auth-on-server=none",
        ):
            self.assertIn(token, DEPLOY)

        for forbidden in (
            ".local/libexec",
            "guardian",
            "pair-converge",
            "git pull",
            "git reset",
            "git stash",
        ):
            self.assertNotIn(forbidden, DEPLOY.lower())

    def test_server_never_needs_github_credentials_for_source_sync(self) -> None:
        self.assertIn('scp -q "$BUNDLE"', DEPLOY)
        self.assertIn('git fetch --no-tags "$bundle" HEAD', DEPLOY)
        self.assertNotIn("github.com/", DEPLOY)
        self.assertNotIn("git@github.com", DEPLOY)

    def test_image_gate_is_bounded_and_fails_fast_on_platform_mismatch(self) -> None:
        self.assertIn("timeout 90s docker pull", DEPLOY)
        self.assertLess(
            DEPLOY.index('*"no matching manifest"*'),
            DEPLOY.index('*"manifest unknown"*'),
        )
        self.assertIn('[[ "$architecture" == "amd64" ]]', DEPLOY)

    def test_canonical_ctl_owns_env_interpolation_and_final_runtime_gates(self) -> None:
        self.assertIn('--env-file "$env_file"', CTL)
        self.assertIn("verify_exact_release_identity", CTL)
        self.assertIn("compose pull", CTL)
        self.assertIn("compose run --rm setup", CTL)
        self.assertIn("compose up --detach --wait", CTL)
        self.assertIn("verify\n", CTL)

    def test_publication_matches_the_real_production_architecture(self) -> None:
        self.assertIn("*.platform=linux/amd64", PUBLISH)
        self.assertIn("platforms: linux/amd64", PUBLISH)
        self.assertNotIn("linux/arm64", PUBLISH)

    def test_public_receipt_checks_the_requested_sha(self) -> None:
        self.assertIn('json.loads(sys.stdin.read())', DEPLOY)
        self.assertIn('actual = data.get("gitSha")', DEPLOY)
        self.assertIn('"$TARGET" <<<"$META"', DEPLOY)

    def test_fish_wrapper_is_only_a_thin_launcher(self) -> None:
        self.assertIn("deploy-production-exact.sh", INSTALLER)
        self.assertIn("LEGACY_HELPERS=UNREFERENCED", INSTALLER)
        marker = 'cat > "$DEST" <<EOF\n'
        self.assertIn(marker, INSTALLER)
        wrapper_template = INSTALLER.split(marker, 1)[1].split("\nEOF\n", 1)[0]

        self.assertIn("deploy-production-exact.sh", wrapper_template)
        self.assertNotIn(".local/libexec", wrapper_template)
        for legacy in (
            "crowdrelay-image-set-gate",
            "crowdrelay-deploy-guardian",
            "crowdrelay-pair-converge",
            "crowdrelay-rekor-deploy",
            "crowdrelay-deploy-verify",
        ):
            self.assertNotIn(legacy, wrapper_template)


if __name__ == "__main__":
    unittest.main()
