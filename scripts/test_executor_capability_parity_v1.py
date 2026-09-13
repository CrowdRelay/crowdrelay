#!/usr/bin/env python3
"""The executor contract's capability list must match what CrowdRelay routes by.

`executor_capability_for_event` maps every outbound event type to the capability
an executor must advertise to receive it. `n8n/viryaos-executor-contract.md` is
what an executor operator reads to build that heartbeat. When the two disagree,
the failure is silent and total: an operator cannot advertise a capability the
contract never mentions, so CrowdRelay emits events that reach a consumer which
has never been told about them.

Measured 2026-09-13. `agent.content`, `agent.content.press_pitch` and
`community.engage` were all routed by the code and named nowhere in the
capability list. `crowdrelay.agent.content_requested` and
`crowdrelay.community.engagement_requested` were delivered to n8n and answered
`HTTP 422` — seven of them by that afternoon, growing — and 422 is
`http_permanent_status`, so the outbox correctly stops retrying and every press
pitch the brain had ever drafted was `cancelled` and gone.

Both directions matter, and they fail for different reasons:

- A capability in the code and not the list is unadvertisable, so its events are
  emitted to a consumer that refuses them.
- A capability in the list and not the code is worse in a quieter way: an operator
  builds and attests a handler for work CrowdRelay will never route, and the
  heartbeat's `team.email` attestation rules mean that attestation is not free.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CAPABILITIES = ROOT / "crates/crowdrelay-infra/src/autopilot/execution_capabilities.rs"
CONTRACT = ROOT / "n8n/viryaos-executor-contract.md"

# Capabilities CrowdRelay satisfies in-process and no external executor should
# register. Empty today, and listed as a named concept so that adding one is a
# deliberate edit rather than a silent exemption.
INTERNAL_ONLY: set[str] = set()


def code_capabilities() -> set[str]:
    source = CAPABILITIES.read_text()
    found = set(re.findall(r'=>\s*"([a-z][a-z0-9_.]*)"', source))
    # Capabilities referenced through a constant rather than a match arm.
    found |= set(re.findall(r'CAPABILITY: &str = "([a-z][a-z0-9_.]*)"', source))
    found.discard("unknown")
    return found - INTERNAL_ONLY


def contract_capabilities() -> set[str]:
    for line in CONTRACT.read_text().splitlines():
        if line.startswith("Capabilities:"):
            return set(re.findall(r"`([a-z][a-z0-9_.]*)`", line))
    raise AssertionError("the contract has no `Capabilities:` line")


class ExecutorCapabilityParity(unittest.TestCase):
    def test_every_routed_capability_is_in_the_contract(self):
        missing = code_capabilities() - contract_capabilities()
        self.assertEqual(
            missing,
            set(),
            f"CrowdRelay routes by {sorted(missing)} and the executor contract's "
            f"capability list does not mention them. An operator cannot advertise "
            f"a capability they have never been told exists, so these events go to "
            f"a consumer that refuses them — which is exactly how every press "
            f"pitch was lost to HTTP 422.",
        )

    def test_the_contract_promises_no_capability_that_is_never_routed(self):
        extra = contract_capabilities() - code_capabilities()
        self.assertEqual(
            extra,
            set(),
            f"the contract lists {sorted(extra)} and no event routes to them. An "
            f"operator would build and attest a handler for work CrowdRelay never "
            f"sends.",
        )

    def test_neither_side_is_silently_empty(self):
        # If either regex stops matching, both assertions above pass vacuously.
        self.assertGreaterEqual(len(code_capabilities()), 25)
        self.assertGreaterEqual(len(contract_capabilities()), 25)


if __name__ == "__main__":
    unittest.main()
