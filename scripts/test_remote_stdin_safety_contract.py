"""Remote deploy bodies must never lose commands to a stdin-consuming child.

`ssh host bash -s <<'MARKER'` feeds the remote script to bash on *stdin*.
Any command in that script that attaches stdin -- `docker compose run` and
`docker compose exec` both do, and `-T` only disables TTY allocation, not the
stdin attachment -- reads the remainder of the script as its own input. bash
then hits EOF and exits 0, silently skipping every remaining line.

That is not hypothetical: it silently skipped `./crowdrelayctl deploy` and the
runtime-revision gate in the 4/5 step of deploy-production-exact.sh, so the
deploy reported success while production still ran the previous image.

Two independent defences are required, and this contract enforces both:

  1. every compose invocation that could attach stdin redirects `</dev/null`
  2. every ssh-heredoc body is one `{ ... } </dev/null` group, so bash parses
     the whole block before running it and no child can reach the script text
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

SHELL_SOURCES = [
    ROOT / "crowdrelayctl",
    ROOT / "scripts" / "deploy.sh",
    ROOT / "scripts" / "deploy-production-safe.sh",
    ROOT / "scripts" / "deploy-production-exact.sh",
]

# `compose run`/`compose exec` (and their `docker compose` spellings) attach
# stdin unless it is explicitly redirected away.
STDIN_ATTACHING = re.compile(r"(?:docker\s+)?compose\s+(?:run|exec)\b")

# An ssh heredoc: `ssh ... bash -s ... <<'MARKER'` through its closing MARKER.
SSH_HEREDOC = re.compile(
    # the ssh invocation may wrap across backslash continuations before `<<`
    r"ssh\b(?:[^\n]|\\\n)*?<<'(?P<marker>[A-Z_][A-Z0-9_]*)'\n(?P<body>.*?)\n(?P=marker)\n",
    re.S,
)


def _logical_commands(text: str) -> list[str]:
    """Join backslash continuations so a wrapped command reads as one line."""
    return text.replace("\\\n", " ").splitlines()


class RemoteStdinSafetyContract(unittest.TestCase):
    def test_stdin_attaching_compose_calls_redirect_from_devnull(self) -> None:
        for source in SHELL_SOURCES:
            text = source.read_text(encoding="utf-8")
            for line in _logical_commands(text):
                if not STDIN_ATTACHING.search(line):
                    continue
                # Skip prose: comments and the assertions inside heredoc docs.
                if line.lstrip().startswith("#"):
                    continue
                self.assertIn(
                    "</dev/null",
                    line,
                    f"{source.name}: compose run/exec must redirect stdin "
                    f"from /dev/null or it will eat an ssh heredoc: {line.strip()}",
                )

    def test_ssh_heredoc_bodies_are_stdin_detached_brace_groups(self) -> None:
        found = 0
        for source in SHELL_SOURCES:
            text = source.read_text(encoding="utf-8")
            for match in SSH_HEREDOC.finditer(text):
                found += 1
                body = match.group("body")
                marker = match.group("marker")
                stripped = [
                    line for line in body.splitlines()
                    if line.strip() and not line.lstrip().startswith("#")
                ]
                self.assertTrue(
                    stripped and stripped[0].strip() == "{",
                    f"{source.name}:{marker} body must open with a bare '{{' so "
                    "bash parses the whole group before executing it",
                )
                self.assertTrue(
                    stripped[-1].strip() == "} </dev/null",
                    f"{source.name}:{marker} body must close with '}} </dev/null' "
                    "so no remote command can consume the script text",
                )
        self.assertGreaterEqual(
            found, 10, "expected every ssh heredoc to be covered by this contract"
        )


if __name__ == "__main__":
    unittest.main()
