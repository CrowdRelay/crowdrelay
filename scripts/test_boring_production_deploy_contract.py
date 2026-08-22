from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
DEPLOY_PATH = ROOT / "scripts/deploy-production-exact.sh"
SAFE_DEPLOY_PATH = ROOT / "scripts/deploy-production-safe.sh"
DEPLOY = DEPLOY_PATH.read_text()
SAFE_DEPLOY = SAFE_DEPLOY_PATH.read_text()
INSTALLER = (ROOT / "scripts/install-deploy-crowdrelay-wrapper.sh").read_text()
CTL = (ROOT / "crowdrelayctl").read_text()
PUBLISH = (ROOT / ".github/workflows/publish-images.yml").read_text()


class BoringProductionDeployContract(unittest.TestCase):
    def test_shell_syntax(self) -> None:
        subprocess.run(["bash", "-n", str(DEPLOY_PATH)], check=True)
        subprocess.run(["bash", "-n", str(SAFE_DEPLOY_PATH)], check=True)

    def test_one_tracked_orchestrator_owns_the_release_path(self) -> None:
        for token in (
            "Production image gate",
            "Exact source sync",
            "Canonical crowdrelayctl deploy",
            "Production git/runtime receipt + public health",
            "./crowdrelayctl pin",
            "./crowdrelayctl deploy",
            "git bundle create",
            "git fetch --no-tags",
            "git merge --ff-only",
            "OCI revision mismatch",
            "production image architecture mismatch",
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
        # The gate binds the image to the host that runs it rather than to a
        # fixed architecture, so one deploy path serves amd64 and arm64 hosts.
        self.assertIn('[[ "$architecture" == "$host_architecture" ]]', DEPLOY)
        self.assertIn("{{.Server.Arch}}", DEPLOY)

    def test_canonical_ctl_owns_env_interpolation_and_final_runtime_gates(self) -> None:
        self.assertIn('--env-file "$env_file"', CTL)
        self.assertIn("verify_exact_release_identity", CTL)
        self.assertIn("compose pull", CTL)
        self.assertIn("compose run --rm -T setup </dev/null", CTL)
        self.assertIn("compose up --detach --wait", CTL)
        self.assertIn("verify\n", CTL)

    def test_safe_mac_orchestrator_only_verifies_after_canonical_deploy(self) -> None:
        self.assertIn("deploy-production-exact.sh", SAFE_DEPLOY)
        self.assertIn("$CANONICAL", SAFE_DEPLOY)
        self.assertIn("ORACLE_MANAGEMENT_RECONCILE=VERIFY_ONLY", SAFE_DEPLOY)
        self.assertNotIn("ORACLE_MANAGEMENT_RECONCILE=REPAIR", SAFE_DEPLOY)
        self.assertNotIn("--force-recreate area-management-proxy", SAFE_DEPLOY)
        self.assertIn("verify_management_proxy", CTL)
        self.assertIn("MANAGEMENT_PROXY=PASS", CTL)
        self.assertIn("CROWDRELAY_AREA_MANAGEMENT_CONFIG_SHA256", CTL)
        for route in (
            "/v1/control-plane/area",
            "/v1/control-plane/ops/summary",
            "/v1/control-plane/ecosystem/flags",
            "/v1/control-plane/autopilot/overview",
        ):
            self.assertIn(route, SAFE_DEPLOY)
        self.assertIn("expected=401", SAFE_DEPLOY)
        self.assertIn("CROWDRELAY_CONTROL_PLANE_HOST:-virya-home", SAFE_DEPLOY)
        self.assertIn("CONTROL_PLANE_AREA_MANAGEMENT_MASTER_KEY", SAFE_DEPLOY)
        self.assertIn("CONTROL_PLANE_MANAGEMENT_MASTER_KEY", SAFE_DEPLOY)
        self.assertIn("CONTROL_PLANE_VIRYA_MANAGEMENT_URL", SAFE_DEPLOY)
        self.assertIn("http://127.0.0.1:18080", SAFE_DEPLOY)
        self.assertIn("CONTROL_PLANE_CROSS_GATE=PASS", SAFE_DEPLOY)
        self.assertIn("CROWDRELAY_SAFE_DEPLOY=PASS", SAFE_DEPLOY)

    def test_publication_matches_the_real_production_architecture(self) -> None:
        # Production spans both hosts, so a release must carry both platforms
        # and merge them into one manifest list the deploy tag resolves to.
        for platform in ("linux/amd64", "linux/arm64"):
            self.assertIn(f"platform: {platform}", PUBLISH)
        self.assertIn("*.platform=${{ matrix.platform }}", PUBLISH)
        self.assertIn("platforms: ${{ matrix.platform }}", PUBLISH)
        self.assertIn("imagetools create", PUBLISH)
        # Native runners only: emulating the Rust release build costs hours.
        self.assertNotIn("setup-qemu-action", PUBLISH)

    def test_final_release_identity_is_git_and_runtime_not_public_metadata(self) -> None:
        receipt = DEPLOY.split("Production git/runtime receipt + public health", 1)[1]
        self.assertIn('head="$(git rev-parse HEAD)"', receipt)
        self.assertIn("PRODUCTION_EXACT_SHA=PASS source=git+oci", receipt)
        self.assertIn("runtime OCI revision mismatch", receipt)
        self.assertIn("PUBLIC_HEALTH=PASS", receipt)
        self.assertIn("PUBLIC_META=STALE", receipt)
        self.assertIn("blocking=false", receipt)
        self.assertNotIn("PUBLIC_EXACT_SHA=FAIL", receipt)

    def test_fish_wrapper_is_only_a_thin_launcher(self) -> None:
        self.assertIn("deploy-production-safe.sh", INSTALLER)
        self.assertIn("LEGACY_HELPERS=UNREFERENCED", INSTALLER)
        marker = 'cat > "$DEST" <<EOF\n'
        self.assertIn(marker, INSTALLER)
        wrapper_template = INSTALLER.split(marker, 1)[1].split("\nEOF\n", 1)[0]

        self.assertIn("deploy-production-safe.sh", wrapper_template)
        self.assertNotIn("deploy-production-exact.sh", wrapper_template)
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
