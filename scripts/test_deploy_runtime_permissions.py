from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
CTL = (ROOT / "crowdrelayctl").read_text(encoding="utf-8")


class DeployRuntimePermissionsContract(unittest.TestCase):
    def test_deploy_normalizes_bind_mount_permissions_before_docker(self):
        deploy = CTL[CTL.index("deploy() {"):CTL.index("\npackage_deploy()")]
        normalize = deploy.index("prepare_runtime_file_permissions")
        doctor = deploy.index("doctor")
        setup = deploy.index("compose run --rm -T setup")
        self.assertLess(normalize, doctor)
        self.assertLess(doctor, setup)

    def test_runtime_json_is_private_but_readable_by_container_gid(self):
        start = CTL.index("prepare_runtime_file_permissions() {")
        end = CTL.index("\ninit_files()", start)
        function = CTL[start:end]
        self.assertIn('runtime_gid="10001"', function)
        self.assertIn('"$CROWDRELAY_BOOTSTRAP_FILE"', function)
        self.assertIn('"$CROWDRELAY_WEBHOOK_SECRETS_FILE"', function)
        self.assertIn('"$CROWDRELAY_FCM_SERVICE_ACCOUNT_FILE"', function)
        self.assertIn('run_privileged chown "${owner_uid}:${runtime_gid}" "$file"', function)
        self.assertIn('run_privileged chmod 0640 "$file"', function)
        self.assertIn('${SUDO_UID:-}', CTL)
        self.assertNotIn('chmod 0644 "$file"', function)

    def test_ship_repairs_legacy_0400_before_preserve_and_after_extract(self):
        ship = CTL[CTL.index("ship() {"):]
        first_bootstrap = ship.index('normalize_runtime_json "$install_dir/deploy/bootstrap.production.json"')
        first_secret = ship.index('normalize_runtime_json "$install_dir/deploy/webhook-secrets.production.json"')
        preserve = ship.index('preserve_dir="$(mktemp -d)"')
        extract = ship.index('tar -xzf "$archive" -C "$install_dir"')
        deploy = ship.index('./crowdrelayctl deploy')
        self.assertLess(first_bootstrap, preserve)
        self.assertLess(first_secret, preserve)
        second_bootstrap = ship.index(
            'normalize_runtime_json "$install_dir/deploy/bootstrap.production.json"',
            extract,
        )
        second_secret = ship.index(
            'normalize_runtime_json "$install_dir/deploy/webhook-secrets.production.json"',
            extract,
        )
        self.assertLess(extract, second_bootstrap)
        self.assertLess(extract, second_secret)
        self.assertLess(second_bootstrap, deploy)
        self.assertLess(second_secret, deploy)
        self.assertNotIn('chown 10001:10001 deploy/webhook-secrets.production.json', ship)
        self.assertNotIn('chmod 0400 deploy/webhook-secrets.production.json', ship)


    def test_fcm_mount_is_install_dir_agnostic_and_preflighted(self):
        compose = (ROOT / "compose.production.yaml").read_text(encoding="utf-8")
        self.assertIn("${CROWDRELAY_FCM_SERVICE_ACCOUNT_HOST_FILE:-./deploy/secrets/firebase-service-account.json}", compose)
        self.assertNotIn("/opt/crowdrelay/deploy/secrets/firebase-service-account.json", compose)
        compose_fn = CTL[CTL.index("compose() {"):CTL.index("\nruntime_owner_uid()")]
        self.assertIn("CROWDRELAY_FCM_SERVICE_ACCOUNT_HOST_FILE", compose_fn)
        deploy = CTL[CTL.index("deploy() {"):CTL.index("\npackage_deploy()")]
        self.assertLess(deploy.index("prepare_runtime_file_permissions"), deploy.index("doctor"))

    def test_deploy_proves_exact_sha_before_generic_health_verification(self):
        start = CTL.index("verify_exact_release_identity() {")
        end = CTL.index("\nverify() {", start)
        exact = CTL[start:end]
        self.assertIn("org.opencontainers.image.revision", exact)
        self.assertIn("/v1/meta", exact)
        self.assertIn('data.get("gitSha")', exact)
        self.assertIn("EXACT_SHA_GATE=PASS", exact)
        deploy = CTL[CTL.index("deploy() {"):CTL.index("\npackage_deploy()")]
        up = deploy.index("compose up --detach --wait")
        exact_call = deploy.index("verify_exact_release_identity")
        verify_call = deploy.index("\n  verify\n")
        self.assertLess(up, exact_call)
        self.assertLess(exact_call, verify_call)


if __name__ == "__main__":
    unittest.main()
