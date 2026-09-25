#!/usr/bin/env python3
"""A deploy must not delete the only copy of a container's logs.

The compose files use Docker's `local` log driver, which removes a container's
logs together with the container. Blue-green removes the outgoing colour on
every deploy, and a rollback removes the colour that failed, which is the one
whose logs explain the failure. On 2026-09-24 every autopilot cycle degraded
for sixteen hours; by the time anyone looked, a deploy had removed the worker
and the cause with it.

`deploy-bluegreen.sh` now gzips each container's log before removing it. This
checks two things:

- every place the script removes an api or worker container archives first;
- the archive function itself writes, prunes to its keep count, and never
  returns non-zero. It runs after cutover and inside rollback, under
  `set -Eeuo pipefail` with an ERR trap, so a failure there would roll back
  a deploy that succeeded or abort a rollback halfway.

The function is run for real against a stub `docker`, not matched as text.
"""
from __future__ import annotations

import os
import re
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts/deploy-bluegreen.sh"

STUB_DOCKER = """#!/usr/bin/env bash
case "$1" in
  inspect) [[ "$2" != missing-* ]] ;;
  logs)
    [[ "${STUB_LOGS_FAIL:-}" == 1 ]] && exit 1
    printf '2026-09-25T00:00:00Z log line from %s\\n' "${@: -1}"
    ;;
  *) exit 0 ;;
esac
"""


def archive_function(source: str) -> str:
    match = re.search(
        r"^archive_container_logs\(\) \{\n.*?^\}\n", source, re.MULTILINE | re.DOTALL
    )
    if match is None:
        raise AssertionError("archive_container_logs() not found in deploy-bluegreen.sh")
    return match.group(0)


class DeployLogArchive(unittest.TestCase):
    def setUp(self) -> None:
        self.source = SCRIPT.read_text()
        self.function = archive_function(self.source)
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.bin = root / "bin"
        self.bin.mkdir()
        docker = self.bin / "docker"
        docker.write_text(STUB_DOCKER)
        docker.chmod(docker.stat().st_mode | stat.S_IXUSR)
        self.archive_dir = root / "archive"

    def run_archive(self, *names: str, keep: int = 8, env: dict[str, str] | None = None,
                    archive_dir: Path | None = None) -> subprocess.CompletedProcess[str]:
        # The same shell options and ERR trap the deploy runs under.
        program = "\n".join([
            "set -Eeuo pipefail",
            "trap 'echo ERR_TRAP_FIRED; exit 99' ERR",
            f"CONTAINER_LOG_ARCHIVE_DIR={archive_dir or self.archive_dir}",
            f"CONTAINER_LOG_ARCHIVE_KEEP={keep}",
            self.function,
            'archive_container_logs "$@"',
            "echo DONE",
        ])
        return subprocess.run(
            ["bash", "-c", program, "archive", *names],
            capture_output=True,
            text=True,
            env={**os.environ, "PATH": f"{self.bin}:{os.environ['PATH']}", **(env or {})},
            check=False,
        )

    def test_every_container_removal_archives_first(self) -> None:
        lines = self.source.splitlines()
        compose_rm = [
            number for number, line in enumerate(lines, 1)
            if re.match(r"\s*rm -f (api|api-green) ", line)
        ]
        direct_rm = [
            number for number, line in enumerate(lines, 1)
            if re.match(r"\s*docker rm ", line)
        ]
        # Assert the patterns matched before judging them: two removals in
        # rollback (one per colour) and one after cutover.
        self.assertEqual(len(compose_rm), 2, f"expected two rollback removals, found {compose_rm}")
        self.assertEqual(len(direct_rm), 1, f"expected one cutover removal, found {direct_rm}")
        for number in compose_rm + direct_rm:
            preceding = "\n".join(lines[max(0, number - 4):number - 1])
            self.assertIn(
                "archive_container_logs",
                preceding,
                f"deploy-bluegreen.sh:{number} removes a container without archiving its logs",
            )

    def test_writes_a_gzip_per_container(self) -> None:
        result = self.run_archive("crowdrelay-api-1", "crowdrelay-worker-1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("DONE", result.stdout)
        archives = sorted(path.name for path in self.archive_dir.iterdir())
        self.assertEqual(len(archives), 2, archives)
        worker = next(self.archive_dir.glob("crowdrelay-worker-1.*.log.gz"))
        text = subprocess.run(["gzip", "-dc", str(worker)], capture_output=True, text=True,
                              check=True).stdout
        self.assertIn("log line from crowdrelay-worker-1", text)

    def test_prunes_to_the_keep_count(self) -> None:
        self.archive_dir.mkdir()
        for day in range(1, 6):
            old = self.archive_dir / f"crowdrelay-worker-1.2026090{day}T000000Z.log.gz"
            old.write_bytes(b"")
            os.utime(old, (day * 1000, day * 1000))
        other = self.archive_dir / "crowdrelay-api-1.20260901T000000Z.log.gz"
        other.write_bytes(b"")
        result = self.run_archive("crowdrelay-worker-1", keep=3)
        self.assertEqual(result.returncode, 0, result.stderr)
        kept = sorted(path.name for path in self.archive_dir.glob("crowdrelay-worker-1.*"))
        self.assertEqual(len(kept), 3, kept)
        # The newest two old ones survive beside the fresh archive.
        self.assertIn("crowdrelay-worker-1.20260905T000000Z.log.gz", kept)
        self.assertIn("crowdrelay-worker-1.20260904T000000Z.log.gz", kept)
        self.assertTrue(other.exists(), "pruning one container touched another's archives")

    def test_missing_and_empty_names_are_skipped(self) -> None:
        result = self.run_archive("", "missing-worker")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("DONE", result.stdout)
        self.assertEqual(list(self.archive_dir.iterdir()), [])

    def test_a_failing_docker_logs_is_reported_not_fatal(self) -> None:
        result = self.run_archive("crowdrelay-worker-1", env={"STUB_LOGS_FAIL": "1"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("ERR_TRAP_FIRED", result.stdout)
        self.assertIn("DONE", result.stdout)
        self.assertIn("LOG_ARCHIVE=FAILED container=crowdrelay-worker-1", result.stderr)
        self.assertEqual(list(self.archive_dir.iterdir()), [])

    def test_an_unwritable_directory_is_reported_not_fatal(self) -> None:
        blocker = Path(self.tmp.name) / "file"
        blocker.write_text("")
        result = self.run_archive("crowdrelay-worker-1", archive_dir=blocker / "sub")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("DONE", result.stdout)
        self.assertIn("LOG_ARCHIVE=SKIPPED", result.stderr)


if __name__ == "__main__":
    unittest.main()
