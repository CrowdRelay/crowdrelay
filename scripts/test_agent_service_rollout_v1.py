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
CP = ROOT.parent / "crowdrelay-control-plane/scripts"
SCRIPT = CP / "deploy-bluegreen.sh"
DEPLOY = CP / "deploy.sh"


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

    def test_the_agent_version_is_resolved_not_named_by_hand(self) -> None:
        """`latest` was never published; only `sha-<40 hex>` tags exist.

        `crowdrelay-agents:latest` lived on the production host as a tag
        someone built by hand once. No registry counterpart ever existed to
        move it forward, so every pull of it 404s. Pinning a sha into `.env`
        instead fixes one release and rots on the next.
        """
        if not DEPLOY.exists():
            self.skipTest("control-plane checkout not present")
        source = DEPLOY.read_text()
        self.assertIn(
            "resolve_agent_image",
            source,
            "deploy.sh no longer resolves the agent image from the agents "
            "repo's newest published release; it is back to trusting whatever "
            "tag production's .env happens to name",
        )
        self.assertIn(
            "AGENT_SERVICE_IMAGE_DIGEST",
            source,
            "the resolved agent image should be pinned by digest",
        )
        self.assertRegex(
            source,
            r'"\$\{AGENT_SERVICE_IMAGE_TAG:-\}" "\$\{AGENT_SERVICE_IMAGE_DIGEST:-\}"',
            "the resolved tag and digest must be passed to the remote script as "
            "arguments; exported env does not survive `ssh ... sudo bash`",
        )

    def test_compose_is_pointed_at_the_resolved_tag(self) -> None:
        """compose reads .env, not the argument the script was handed.

        `compose.agents.yml` declares
        `image: crowdrelay-agents:${AGENT_SERVICE_IMAGE_TAG:-latest}` and
        resolves it from `.env` on the production host. Pulling the right
        image and tagging it locally changes nothing if `.env` still says
        `latest` — compose starts `latest`, which is a tag the publish
        workflow has never pushed and which had sat unchanged on the host for
        days.
        """
        if not SCRIPT.exists():
            self.skipTest("control-plane checkout not present")
        self.assertRegex(
            SCRIPT.read_text(),
            r'sed -i "s\|\^AGENT_SERVICE_IMAGE_TAG=\.\*\|AGENT_SERVICE_IMAGE_TAG=\$\{agent_tag\}\|" \.env',
            "the rollout never points .env at the tag it resolved, so compose "
            "recreates the container on whatever .env already named",
        )

    def test_the_running_image_is_verified_not_assumed(self) -> None:
        """Healthy is not the same as running the right thing.

        The old check inspected the image it had just pulled and tagged, then
        reported PASS on a container that had been recreated from a different
        one. Every agent release went out green while the service never moved.
        """
        if not SCRIPT.exists():
            self.skipTest("control-plane checkout not present")
        source = SCRIPT.read_text()
        self.assertIn(
            "AGENT_SERVICE=FAILED reason=wrong-image",
            source,
            "the rollout does not compare the container's actual image against "
            "the one it pulled, so a recreate that picked up the wrong image "
            "still reports PASS",
        )
        self.assertIn(
            'running_agent_image="$(docker inspect "$agent_container" --format',
            source,
            "the running image should be read from the container itself, not "
            "from the tag the script just created",
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
