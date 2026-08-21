#!/usr/bin/env python3
"""Compose interpolation must not silently eat shell variables.

Compose substitutes `$NAME` in a service definition before the string ever
reaches a shell. A healthcheck written as `$(readlink "$p")` therefore becomes
`$(readlink "")` -- Compose warns once about an unset variable, the probe still
parses, and it simply never passes. That failure mode is invisible: the service
just stays unhealthy, and a provisioner reports a readiness timeout rather than
a config bug. Shell variables must be written `$$NAME`.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
COMPOSE_FILES = sorted(
    {path for path in ROOT.glob("compose*.y*ml")} | {ROOT / "docker-compose.yml"}
)
# A `$` that is neither an escaped `$$` nor a deliberate `${VAR}` substitution.
BARE_DOLLAR = re.compile(r"(?<!\$)\$(?![$\{])")


class ComposeInterpolationContract(unittest.TestCase):
    def test_compose_files_exist(self):
        self.assertTrue(COMPOSE_FILES, "no compose files found to check")
        for path in COMPOSE_FILES:
            self.assertTrue(path.is_file(), f"missing compose file: {path.name}")

    def test_no_shell_variable_is_left_to_compose_interpolation(self):
        for path in COMPOSE_FILES:
            for number, line in enumerate(
                path.read_text(encoding="utf-8").splitlines(), 1
            ):
                if line.lstrip().startswith("#"):
                    continue
                self.assertIsNone(
                    BARE_DOLLAR.search(line),
                    f"{path.name}:{number} passes a bare $ through Compose "
                    f"interpolation; write $$ for a shell variable: {line.strip()}",
                )

    def test_worker_healthcheck_matches_the_installed_binary(self):
        """A probe that checks the wrong path fails exactly like a dead worker."""
        dockerfile = (ROOT / "Dockerfile").read_text(encoding="utf-8")
        worker_path = re.search(
            r"COPY --from=builder /out/crowdrelay-worker (\S+)", dockerfile
        )
        self.assertIsNotNone(worker_path, "worker install path not found in Dockerfile")
        installed = worker_path.group(1)
        for path in COMPOSE_FILES:
            body = path.read_text(encoding="utf-8")
            if "crowdrelay-worker" not in body or "readlink" not in body:
                continue
            self.assertIn(
                installed,
                body,
                f"{path.name} probes a worker path the image does not install",
            )


if __name__ == "__main__":
    unittest.main()
