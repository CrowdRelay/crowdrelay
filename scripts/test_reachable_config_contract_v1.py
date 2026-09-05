#!/usr/bin/env python3
"""Configuration that is set but cannot reach a container must be reported.

Two env files sit next to each other on a production host and do different jobs.
`.env` is what Docker Compose reads to substitute `${VAR}` in the compose files.
The file named by `CROWDRELAY_ENV_FILE` (default `deploy/.env.production`) is
what `env_file:` injects into the containers. A variable written to `.env` alone
reaches a container only if some compose file mentions it by name.

Nothing checked that, and it cost a feature. `CROWDRELAY_CITY_GEOCODING_CONTACT`
was set in `.env`, which is where an operator would reasonably put it, and the
city geocoding worker stayed off across a restart and two deploys because the
value never entered the container. `deploy/env.production.example` documents the
variable, so the configuration looked done. The only symptom was one line in the
boot log saying the component was disabled, and requested cities silently
keeping no coordinates -- which is exactly the class of failure the readiness
line was added to surface, arriving through the one path it could not see.

`crowdrelayctl doctor` now fails on it, and doctor runs before every deploy. This
keeps that check wired and keeps its rule honest: a variable named in a compose
file is legitimately allowed to live only in `.env`, because substitution is how
it reaches a container. Image tags are the usual case, and flagging them would
make the check noise and get it deleted.
"""
from __future__ import annotations

import re
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CTL = ROOT / "crowdrelayctl"
CTL_TEXT = CTL.read_text(encoding="utf-8")


class DoctorChecksReachability(unittest.TestCase):
    def test_the_check_exists(self) -> None:
        self.assertIn(
            "check_unreachable_config()",
            CTL_TEXT,
            "crowdrelayctl no longer defines the unreachable-config check",
        )

    def test_doctor_runs_it(self) -> None:
        """Defining it without calling it is how this silently stops working."""
        doctor = re.search(r"^doctor\(\)\s*\{(.*?)^\}", CTL_TEXT, re.S | re.M)
        self.assertIsNotNone(doctor, "crowdrelayctl no longer defines doctor()")
        assert doctor is not None
        self.assertIn(
            "check_unreachable_config",
            doctor.group(1),
            "doctor() no longer calls check_unreachable_config, so nothing runs it "
            "before a deploy",
        )

    def test_the_script_parses(self) -> None:
        result = subprocess.run(["bash", "-n", str(CTL)], capture_output=True)
        self.assertEqual(
            result.returncode, 0, f"crowdrelayctl has syntax errors: {result.stderr.decode()}"
        )


class TheRuleBehavesOnRealFiles(unittest.TestCase):
    """Runs the extracted check against a scratch tree, both ways round."""

    def run_check(self, tmp: Path) -> subprocess.CompletedProcess[bytes]:
        body = re.search(r"^check_unreachable_config\(\)\s*\{.*?^\}", CTL_TEXT, re.S | re.M)
        assert body is not None, "could not extract the check from crowdrelayctl"
        script = "\n".join(
            [
                "set -Eeuo pipefail",
                'require_command(){ command -v "$1" >/dev/null; }',
                f"ROOT_DIR={tmp}",
                'absolute_path(){ printf "%s" "$ROOT_DIR/$1"; }',
                "CROWDRELAY_ENV_FILE=deploy/.env.production",
                body.group(0),
                "check_unreachable_config",
            ]
        )
        return subprocess.run(["bash", "-c", script], capture_output=True)

    def scratch(self, tmp: Path, *, in_env_file: bool) -> None:
        (tmp / "deploy").mkdir(parents=True, exist_ok=True)
        (tmp / ".env").write_text(
            "CROWDRELAY_DATABASE_URL=postgres://x\n"
            "CROWDRELAY_IMAGE_TAG=sha-abc\n"
            "CROWDRELAY_CITY_GEOCODING_CONTACT=ops@example.test\n"
        )
        env_file = "CROWDRELAY_DATABASE_URL=postgres://x\n"
        if in_env_file:
            env_file += "CROWDRELAY_CITY_GEOCODING_CONTACT=ops@example.test\n"
        (tmp / "deploy" / ".env.production").write_text(env_file)
        # Names CROWDRELAY_IMAGE_TAG, so that one reaches a container by
        # substitution and must not be reported.
        (tmp / "compose.production.yaml").write_text(
            "services:\n  worker:\n    image: repo:${CROWDRELAY_IMAGE_TAG}\n"
        )

    def test_a_variable_that_cannot_reach_a_container_fails(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            self.scratch(tmp, in_env_file=False)
            result = self.run_check(tmp)
            self.assertEqual(result.returncode, 1, "the check passed on inert configuration")
            self.assertIn(
                b"CROWDRELAY_CITY_GEOCODING_CONTACT",
                result.stderr,
                "the failure must name the variable an operator has to move",
            )

    def test_moving_it_into_the_env_file_passes(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            self.scratch(tmp, in_env_file=True)
            result = self.run_check(tmp)
            self.assertEqual(
                result.returncode, 0, f"the check still fails after the fix: {result.stderr.decode()}"
            )

    def test_a_substituted_variable_is_not_reported(self) -> None:
        """`CROWDRELAY_IMAGE_TAG` lives only in `.env` on every host.

        Reporting it would make the check noise, and a noisy gate gets deleted.
        """
        import tempfile

        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            self.scratch(tmp, in_env_file=True)
            result = self.run_check(tmp)
            self.assertNotIn(b"CROWDRELAY_IMAGE_TAG", result.stderr)


if __name__ == "__main__":
    unittest.main()
