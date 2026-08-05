#!/usr/bin/env python3
from __future__ import annotations

import os
import pathlib
import subprocess
import tempfile
import textwrap
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "ops/commerce/activate-inventory.sh"


def fake_docker(directory: pathlib.Path, body: str) -> None:
    executable = directory / "docker"
    executable.write_text("#!/usr/bin/env bash\nset -euo pipefail\n" + textwrap.dedent(body))
    executable.chmod(0o755)


def run_script(fake_bin: pathlib.Path) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env["PATH"] = f"{fake_bin}:{env['PATH']}"
    return subprocess.run(
        [str(SCRIPT)],
        cwd=ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )


class InventoryActivationCliTests(unittest.TestCase):
    def test_ready_preflight_activates_and_verifies_public_catalog(self):
        with tempfile.TemporaryDirectory() as raw:
            fake_bin = pathlib.Path(raw)
            fake_docker(
                fake_bin,
                r'''
                if [[ "${1:-}" == "inspect" ]]; then exit 0; fi
                method=""; path=""
                for arg in "$@"; do
                  case "$arg" in
                    REQUEST_METHOD=*) method="${arg#REQUEST_METHOD=}" ;;
                    REQUEST_PATH=*) path="${arg#REQUEST_PATH=}" ;;
                  esac
                done
                case "$method $path" in
                  "GET /v1/staff/merch/inventory/activation")
                    printf '%s\n' '{"status":"preparing","ready":false,"fully_enabled":false,"can_mark_ready":true,"counted_active_variants":22,"total_active_variants":22,"blockers":[],"missing_skus":[]}' ;;
                  "POST /v1/staff/merch/inventory/ready")
                    printf '%s\n' '{"status":"ready","ready":true,"fully_enabled":true,"can_mark_ready":true,"counted_active_variants":22,"total_active_variants":22,"blockers":[],"missing_skus":[]}' ;;
                  "GET /v1/public/merch/catalog")
                    printf '%s\n' '{"products":[{"variants":[{},{}]}]}' ;;
                  *) exit 99 ;;
                esac
                ''',
            )
            result = run_script(fake_bin)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("public merch catalog active", result.stdout)

    def test_incomplete_stocktake_fails_closed_before_ready(self):
        with tempfile.TemporaryDirectory() as raw:
            fake_bin = pathlib.Path(raw)
            fake_docker(
                fake_bin,
                r'''
                if [[ "${1:-}" == "inspect" ]]; then exit 0; fi
                printf '%s\n' '{"status":"preparing","ready":false,"fully_enabled":false,"can_mark_ready":false,"counted_active_variants":21,"total_active_variants":22,"blockers":["uncounted_variants"],"missing_skus":["VIRYA-CD-ECHOES"]}'
                ''',
            )
            result = run_script(fake_bin)
            self.assertEqual(result.returncode, 2)
            self.assertIn("missing_skus=VIRYA-CD-ECHOES", result.stdout)
            self.assertIn("NOT ACTIVATED", result.stderr)


if __name__ == "__main__":
    unittest.main()
