from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
CTL = (ROOT / "crowdrelayctl").read_text(encoding="utf-8")


class DeployRuntimePermissionsContract(unittest.TestCase):
    def test_deploy_normalizes_bind_mount_permissions_before_docker(self):
        deploy = CTL[CTL.index("deploy() {"):CTL.index("\npackage_deploy()")]
        normalize = deploy.index("prepare_runtime_file_permissions")
        doctor = deploy.index("doctor")
        setup = deploy.index("compose run --rm setup")
        self.assertLess(normalize, doctor)
        self.assertLess(doctor, setup)

    def test_runtime_json_is_private_but_readable_by_container_gid(self):
        start = CTL.index("prepare_runtime_file_permissions() {")
        end = CTL.index("\ninit_files()", start)
        function = CTL[start:end]
        self.assertIn('runtime_gid="10001"', function)
        self.assertIn('"$CROWDRELAY_BOOTSTRAP_FILE"', function)
        self.assertIn('"$CROWDRELAY_WEBHOOK_SECRETS_FILE"', function)
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


if __name__ == "__main__":
    unittest.main()
