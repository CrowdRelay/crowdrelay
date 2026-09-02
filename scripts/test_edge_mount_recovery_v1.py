#!/usr/bin/env python3
"""A detached edge bind mount must heal, not silently skip the cutover.

The edge Caddyfile is a **single-file** bind mount:

    - ./Caddyfile:/etc/caddy/Caddyfile:ro

Docker resolves a single-file bind to an inode once, at container start, and
the container follows that inode for its whole life. Anything that *replaces*
the file rather than writing into it produces a new inode — `git checkout`,
`git pull`, `mv`, `cp`-then-rename, most editors — and from that moment the
host and the container are reading two different files.

`/opt/crowdrelay` is a git checkout and two repos' deploy scripts write that
file, so this is a routine occurrence rather than an edge case. On 2026-09-02
a checkout at 10:59 swapped the inode under a container that had been running
since 08:00. The next blue-green deploy wrote the host copy, `caddy reload`
read the container's orphaned copy, and the cutover did nothing at all —
traffic stayed on blue while every health check passed and the marker on disk
said green.

Both scripts now detect the divergence and restart the edge, which is the only
remedy that works. `docker cp` fails with "device or resource busy" — it
removes-and-replaces, which is impossible on a mount point — and the mount is
read-only, so writing from inside the container is refused too. A restart makes
Docker resolve the bind afresh against the current file.

This checks the recovery is still there, still precedes the reload, and that
the post-reload verification survives — the recovery makes that check pass in
the normal case, which is exactly when someone is tempted to delete it.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPTS = [
    ROOT / "scripts/deploy-bluegreen.sh",
    ROOT.parent / "crowdrelay-control-plane/scripts/deploy-bluegreen.sh",
]


def present() -> list[Path]:
    """Only check what exists; the sibling checkout is not always there."""
    return [path for path in SCRIPTS if path.exists()]


class EdgeMountRecovery(unittest.TestCase):
    def test_at_least_one_script_is_checked(self) -> None:
        self.assertTrue(present(), "no blue-green script found to check")

    def test_a_detached_mount_is_re_attached(self) -> None:
        for path in present():
            source = path.read_text()
            self.assertRegex(
                source,
                r'docker restart "\$EDGE_CONTAINER"',
                f"{path.name} does not restart the edge when host and "
                f"container disagree. Nothing else works: `docker cp` fails "
                f"with \"device or resource busy\" on a mount point, and the "
                f"mount is read-only. Without it the reload reads the stale "
                f"file and the cutover silently does nothing",
            )
            self.assertNotIn(
                'docker cp "$EDGE_CADDYFILE"',
                source,
                f"{path.name} tries to `docker cp` over the bind mount; that "
                f"fails with \"device or resource busy\" every time",
            )

    def test_the_re_attach_is_verified(self) -> None:
        """A restart that did not help must fail, not proceed."""
        for path in present():
            self.assertIn(
                "edge config still differs after restart",
                path.read_text(),
                f"{path.name} restarts the edge without re-checking; if the "
                f"restart did not help, the deploy would carry on and cut over "
                f"to a config the edge is not running",
            )

    def test_a_detached_mount_is_reported(self) -> None:
        """Healing silently would hide a real and recurring condition."""
        for path in present():
            self.assertIn(
                "EDGE_MOUNT=DETACHED",
                path.read_text(),
                f"{path.name} re-syncs the edge config without saying so; an "
                f"operator should see that the mount had drifted",
            )

    def test_the_deploy_does_not_refuse_on_a_stale_mount(self) -> None:
        """It used to demand a manual fix with no documented steps."""
        for path in present():
            self.assertNotRegex(
                path.read_text(),
                r"fail 'edge Caddy bind mount is stale; apply edge config separately",
                f"{path.name} refuses to deploy on a stale mount again. The "
                f"condition is routine and self-healable; telling the operator "
                f"to 'apply edge config separately' names no steps",
            )

    def test_the_post_reload_verification_survives(self) -> None:
        """The copy makes this pass, which is when it looks removable."""
        for path in present():
            source = path.read_text()
            self.assertIn(
                "edge runtime config differs after reload",
                source,
                f"{path.name} no longer verifies the running config after "
                f"reload. That check is what caught this bug; with the copy in "
                f"place a failure now means something is rewriting the config "
                f"underneath the deploy, which is worth failing on",
            )

    def test_the_reload_still_targets_the_container_path(self) -> None:
        for path in present():
            self.assertRegex(
                path.read_text(),
                r"caddy reload --config /etc/caddy/Caddyfile",
                f"{path.name} reloads from an unexpected path; the copy above "
                f"targets /etc/caddy/Caddyfile and the two must agree",
            )

    def test_the_re_attach_precedes_the_reload(self) -> None:
        """Restarting after the reload would apply one deploy too late."""
        for path in present():
            source = path.read_text()
            restart_at = source.rfind('docker restart "$EDGE_CONTAINER"')
            reload_at = source.rfind("caddy reload --config /etc/caddy/Caddyfile")
            self.assertNotEqual(restart_at, -1, f"{path.name}: no restart found")
            self.assertNotEqual(reload_at, -1, f"{path.name}: no reload found")
            self.assertLess(
                restart_at,
                reload_at,
                f"{path.name} re-attaches the mount after reloading, so the "
                f"reload still reads the stale file and the new config only "
                f"takes effect on the following deploy",
            )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"EDGE_MOUNT_RECOVERY=PASS scripts={len(present())}")
    else:
        print("EDGE_MOUNT_RECOVERY=FAIL")
        sys.exit(1)
