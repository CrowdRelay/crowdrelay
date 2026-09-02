#!/usr/bin/env python3
"""A deploy must actually redeploy the agent-service.

The blue-green script gated its agent-service block on the tag shape:

    agent_tag="$(sed -n 's/^AGENT_SERVICE_IMAGE_TAG=//p' .env | tail -n1)"
    if [[ "$agent_tag" =~ ^sha-[0-9a-f]{40}$ ]]; then
      ...
    else
      printf 'AGENT_SERVICE=SKIP reason=no-tag-configured\\n'

Production has always carried `AGENT_SERVICE_IMAGE_TAG=latest`. That is a
configured tag, so the message was wrong, and every deploy took the `else`
branch: the control plane rolled forward while the agent-service stayed on
whatever image it happened to boot with. A Reddit navigation-timeout fix and a
generation of model-name changes were published to `latest` and never ran.

The rollback had the matching defect. It restored the previous *tag* into
`.env`, which under a moving tag re-selects the image that just failed health.

This checks the shape of the fix, not the wording: any configured tag reaches
the rollout, a stale image is reported rather than passed over in silence, and
rollback pins an image ID.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT.parent / "crowdrelay-control-plane/scripts/deploy-bluegreen.sh"


class AgentServiceRollout(unittest.TestCase):
    def setUp(self) -> None:
        if not SCRIPT.exists():
            self.skipTest("control-plane checkout not present")
        self.source = SCRIPT.read_text()

    def test_a_moving_tag_is_deployable(self) -> None:
        """`latest` is the tag production actually uses."""
        self.assertNotRegex(
            self.source,
            r'if \[\[ "\$agent_tag" =~ \^sha-',
            "the agent-service rollout is gated on a `sha-<40 hex>` tag again; "
            "production runs AGENT_SERVICE_IMAGE_TAG=latest, so this skips the "
            "agent on every single deploy",
        )
        self.assertRegex(
            self.source,
            r'if \[\[ -n "\$agent_tag" \]\]',
            "the rollout should run for any configured tag",
        )

    def test_the_new_image_is_pulled_not_assumed_present(self) -> None:
        """Without a pull, a moving tag resolves to the host's stale copy."""
        self.assertIn(
            "ghcr.io/crowdrelay/crowdrelay-agents:${agent_tag}",
            self.source,
            "nothing pulls the agent image by tag; on a moving tag the deploy "
            "would recreate the container from the image already on the host",
        )

    def test_a_skipped_agent_is_reported_on_stderr(self) -> None:
        """Silence here is what let the agent drift for months."""
        for marker in ("AGENT_IMAGE=STALE", "AGENT_SERVICE=SKIP"):
            self.assertIn(marker, self.source, f"{marker} is no longer reported")
        skips = re.findall(r"printf 'AGENT_(?:IMAGE|SERVICE)=(?:STALE|SKIP)[^\n]*", self.source)
        self.assertTrue(skips, "no skip/stale reporting found at all")
        for line in skips:
            if "no-tag-configured" in line:
                continue  # genuinely nothing to deploy
            self.assertIn(
                ">&2",
                line,
                f"a skipped agent rollout goes to stdout and gets lost in the "
                f"deploy log: {line}",
            )

    def test_rollback_pins_an_image_not_a_tag(self) -> None:
        self.assertNotRegex(
            self.source,
            r"AGENT_SERVICE_IMAGE_TAG=\$\{prev_agent_tag\}",
            "rollback restores a tag name; under a moving tag that re-selects "
            "the image that just failed its health check",
        )
        self.assertRegex(
            self.source,
            r'docker tag "\$prev_agent_image"',
            "rollback should retag the previously running image ID",
        )

    def test_the_health_gate_survives(self) -> None:
        self.assertIn("AGENT_SERVICE=FAILED", self.source)
        self.assertIn(
            'fail "agent-service failed health check after deploy"',
            self.source,
            "an unhealthy agent must still fail the deploy",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        state = "SKIP" if not SCRIPT.exists() else "PASS"
        print(f"AGENT_SERVICE_ROLLOUT={state}")
    else:
        print("AGENT_SERVICE_ROLLOUT=FAIL")
        sys.exit(1)
