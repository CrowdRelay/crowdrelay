#!/usr/bin/env python3
"""Blue-green must recover from disagreement, not wedge on it.

The deploy used to read `# CROWDRELAY_ACTIVE=` from the edge Caddyfile, treat
that comment as the authority for which colour was live, and use container
health only as a veto:

    active_color="$(sed -n 's/^# CROWDRELAY_ACTIVE=//p' "$EDGE_CADDYFILE")"
    if [[ "$active_color" == "blue" ]]; then
      [[ "$blue_health" == "healthy" ]] || fail "edge declares blue active ..."

Two failure modes followed, and both were observed in production.

**Stuck.** Any drift between the marker and reality failed every subsequent
deploy, with no path forward but hand-editing production config. Drift is easy
to produce: a run interrupted between the marker flip and the container coming
up, a container removed by hand, a `git checkout` of the Caddyfile.

**Unrecoverable.** With neither colour healthy the run failed outright — the
one situation where a deploy is most needed was the one it refused.

The rewrite had the same defect from the other side: its `sed` matched the
*old* marker value, so a stale marker left the candidate file unchanged and the
run died on its own "was not updated" guard.

This checks both scripts still derive the live colour from container health,
still handle the cold start, and still write the marker without depending on
what it previously said.
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
    """Only check what exists; the sibling checkout is not always present."""
    return [path for path in SCRIPTS if path.exists()]


class BlueGreenRecovery(unittest.TestCase):
    def test_at_least_one_script_is_checked(self) -> None:
        self.assertTrue(present(), "no blue-green script found to check")

    def test_health_decides_not_only_vetoes(self) -> None:
        """The marker may break a tie; it may not be the sole authority."""
        for path in present():
            source = path.read_text()
            self.assertRegex(
                source,
                r"(blue_ok|green_ok)\s*=",
                f"{path.name} no longer derives the live colour from container "
                f"health; a comment in the Caddyfile must not be the authority",
            )
            self.assertNotRegex(
                source,
                r'if \[\[ "\$active_color" == "blue" \]\]; then\s*\n\s*\[\[ "\$blue_health"',
                f"{path.name} reverted to marker-as-authority with health as a "
                f"veto, which wedges every deploy once the two disagree",
            )

    def test_a_cold_start_is_deployable(self) -> None:
        """Neither colour healthy must still deploy, not fail."""
        for path in present():
            source = path.read_text()
            self.assertIn(
                "COLD_START",
                source,
                f"{path.name} has no cold-start path; with nothing healthy the "
                f"deploy refuses, which is exactly when it is needed most",
            )

    def test_drift_is_reported_rather_than_hidden(self) -> None:
        for path in present():
            self.assertIn(
                "EDGE_MARKER=RECONCILED",
                path.read_text(),
                f"{path.name} silently corrects marker drift; an operator "
                f"should see that reality and the file disagreed",
            )

    def test_the_marker_rewrite_ignores_its_previous_value(self) -> None:
        """`s/ACTIVE=blue/ACTIVE=green/` fails whenever the file says neither."""
        for path in present():
            source = path.read_text()
            stale = re.findall(
                r"s[/|]# (?:CROWDRELAY|CONTROL_PLANE)_ACTIVE=(?:blue|green)[/|]", source
            )
            self.assertEqual(
                stale,
                [],
                f"{path.name} rewrites the marker by matching its old value "
                f"({stale}); a stale marker then leaves the candidate unchanged "
                f"and the run dies on its own 'was not updated' guard",
            )
            self.assertRegex(
                source,
                r"_ACTIVE=\.\*\|",
                f"{path.name} should match the marker with `.*` so the rewrite "
                f"works from any prior state",
            )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"BLUEGREEN_RECOVERY=PASS scripts={len(present())}")
    else:
        print("BLUEGREEN_RECOVERY=FAIL")
        sys.exit(1)
