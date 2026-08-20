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

    def test_safe_mac_orchestrator_reconciles_proxy_without_normal_double_restart(self) -> None:
        self.assertIn('CANONICAL="$ROOT_DIR/scripts/deploy-production-exact.sh"', SAFE_DEPLOY)
        self.assertIn('"$CANONICAL" "$TARGET"', SAFE_DEPLOY)
        self.assertIn('proxy_status="$(docker inspect crowdrelay-area-management-proxy-1', SAFE_DEPLOY)
        self.assertIn('if [[ "$proxy_status" != "running" || "$runtime_sha" != "$source_sha" ]]', SAFE_DEPLOY)
        self.assertIn('compose run --rm --no-deps --entrypoint caddy area-management-proxy', SAFE_DEPLOY)
        self.assertIn('ORACLE_MANAGEMENT_RECONCILE=REPAIR', SAFE_DEPLOY)
        self.assertIn('ORACLE_MANAGEMENT_RECONCILE=NOOP', SAFE_DEPLOY)
        self.assertIn('--force-recreate area-management-proxy', SAFE_DEPLOY)
        self.assertLess(
            SAFE_DEPLOY.index('if [[ "$proxy_status" != "running" || "$runtime_sha" != "$source_sha" ]]'),
            SAFE_DEPLOY.index('--force-recreate area-management-proxy'),
        )
        for route in (
            '/v1/control-plane/area',
            '/v1/control-plane/ops/summary',
            '/v1/control-plane/ecosystem/flags',
            '/v1/control-plane/autopilot/overview',
        ):
            self.assertIn(route, SAFE_DEPLOY)
        self.assertIn('expected=401', SAFE_DEPLOY)
        self.assertIn('CROWDRELAY_CONTROL_PLANE_HOST:-virya-home', SAFE_DEPLOY)
        self.assertIn('CONTROL_PLANE_AREA_MANAGEMENT_MASTER_KEY', SAFE_DEPLOY)
        self.assertIn('CONTROL_PLANE_MANAGEMENT_MASTER_KEY', SAFE_DEPLOY)
        self.assertIn('CONTROL_PLANE_VIRYA_MANAGEMENT_URL', SAFE_DEPLOY)
        self.assertIn('http://127.0.0.1:18080', SAFE_DEPLOY)
        self.assertIn('/api/v1/tenants/virya/operations/summary', SAFE_DEPLOY)
        self.assertIn('CONTROL_PLANE_CROSS_GATE=PASS', SAFE_DEPLOY)
        self.assertIn('CROWDRELAY_SAFE_DEPLOY=PASS', SAFE_DEPLOY)
        self.assertNotIn('docker exec "$tunnel" grep', SAFE_DEPLOY)
        self.assertNotIn('docker exec crowdrelay-area-management-proxy-1 grep', SAFE_DEPLOY)

    def test_publication_matches_the_real_production_architecture(self) -> None:
        self.assertIn("*.platform=linux/amd64", PUBLISH)
        self.assertIn("platforms: linux/amd64", PUBLISH)
        self.assertNotIn("linux/arm64", PUBLISH)

    def test_public_receipt_checks_the_requested_sha(self) -> None:
        self.assertIn('json.loads(sys.stdin.read())', DEPLOY)
        self.assertIn('actual = data.get("gitSha")', DEPLOY)
        self.assertIn('"$TARGET" <<<"$META"', DEPLOY)

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
