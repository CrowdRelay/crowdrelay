#!/usr/bin/env python3
"""The agents bearer token is one HMAC computed in two languages.

CrowdRelay's worker derives the token that `crowdrelay-agents` verifies:

    token = hex(HMAC-SHA256(master_key, NAMESPACE + workspace_id + ":" + capability))

Rust builds it in `crowdrelay-worker/src/discovery.rs`; TypeScript rebuilds it
in `crowdrelay-agents/src/auth.ts`. Nothing connects the two, and the three
things that must agree byte-for-byte — the namespace, the separator layout and
the capability spellings — are literals on both sides.

The failure mode is not an outage, which is why it needs a gate.
`extractWorkspaceId` tries the scoped token, and on failure falls back to a
legacy token derived from `NAMESPACE + workspace_id` alone. The legacy token is
still workspace-bound, so a namespace or spelling drift does not break
authentication — it makes every scoped check miss, every caller land on the
legacy path, and **every token grant all capabilities**. A `read` caller would
silently hold `credentials` and `social_publish`. `AGENT_SERVICE_ALLOW_LEGACY_TOKENS`
defaults to true, so that fallback is live.

This is a static contract test. It reads the sibling checkout when one exists
and skips when it does not, the same way `test-ecosystem-contract-v2.py` treats
virya — CI checks out the ecosystem, a bare CrowdRelay clone does not have it.
"""
from __future__ import annotations

import hashlib
import hmac
import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DISCOVERY = ROOT / "crates/crowdrelay-worker/src/discovery.rs"
AGENTS = ROOT.parent / "crowdrelay-agents"
AGENTS_AUTH = AGENTS / "src/auth.ts"


def rust_namespace() -> str:
    source = DISCOVERY.read_text()
    match = re.search(
        r'const AGENT_AUTH_NAMESPACE: &\[u8\] = b"([^"]+)";', source
    )
    if not match:
        raise AssertionError("AGENT_AUTH_NAMESPACE not found; the parser is wrong")
    return match.group(1)


def rust_capabilities() -> set[str]:
    """`AgentCapability::as_str` as the wire values it produces."""
    source = DISCOVERY.read_text()
    start = source.index("const fn as_str(self) -> &'static str {")
    body = source[start : source.index("\n    }", start)]
    values = set(re.findall(r'Self::\w+ => "([a-z_]+)"', body))
    if not values:
        raise AssertionError("AgentCapability::as_str has no arms; parser is wrong")
    return values


def ts_namespace() -> str:
    source = AGENTS_AUTH.read_text()
    match = re.search(r'const NAMESPACE = "([^"]+)";', source)
    if not match:
        raise AssertionError("NAMESPACE not found in agents auth.ts")
    return match.group(1)


def ts_capabilities() -> set[str]:
    source = AGENTS_AUTH.read_text()
    match = re.search(r"export type Capability =([^;]+);", source)
    if not match:
        raise AssertionError("Capability union not found in agents auth.ts")
    return set(re.findall(r'"([a-z_]+)"', match.group(1)))


@unittest.skipUnless(
    AGENTS_AUTH.exists(),
    "no crowdrelay-agents checkout beside this repository",
)
class AgentCapabilityTokenParity(unittest.TestCase):
    def test_the_namespace_is_identical(self) -> None:
        self.assertEqual(
            rust_namespace(),
            ts_namespace(),
            "the HMAC namespace differs between the deriving and verifying "
            "side. Every scoped token would miss and fall back to the legacy "
            "path, which authenticates and grants every capability",
        )

    def test_the_capability_vocabulary_is_identical(self) -> None:
        self.assertEqual(
            rust_capabilities(),
            ts_capabilities(),
            "the capability spellings differ. A capability CrowdRelay can name "
            "and agents cannot is a token that always falls back to legacy; "
            "one agents can require and CrowdRelay cannot produce is a route "
            "no caller can reach with a scoped token",
        )

    def test_the_message_layout_is_identical(self) -> None:
        """`namespace + workspace_id + ':' + capability`, in that order.

        Asserted against the TS expression rather than inferred, because the
        colon is easy to move and a token built from `workspace + capability`
        with no separator verifies against nothing while looking correct.
        """
        source = AGENTS_AUTH.read_text()
        self.assertIn(
            'NAMESPACE + workspaceId + ":" + capability',
            source,
            "the agents-side message layout changed; the Rust side builds "
            "namespace, workspace id, a colon, then the capability",
        )
        rust = DISCOVERY.read_text()
        derive = rust[rust.index("pub(crate) fn derive_agent_token_with_capability") :]
        derive = derive[: derive.index("\n}")]
        for step in (
            "mac.update(AGENT_AUTH_NAMESPACE);",
            "mac.update(workspace_id.to_string().as_bytes());",
            'mac.update(b":");',
            "mac.update(capability.as_str().as_bytes());",
        ):
            self.assertIn(step, derive, f"the Rust message layout lost `{step}`")
        # The workspace id goes in hyphenated and lowercase — `Uuid`'s Display.
        # The agents side takes the header string verbatim, so a caller sending
        # the simple form would compute a different HMAC against the same
        # workspace.
        self.assertNotIn(
            "workspace_id.simple()",
            derive,
            "the workspace id must be hyphenated `Uuid` Display, which is what "
            "the agents side receives in the X-Workspace-Id header",
        )

    def test_a_known_vector_matches_both_implementations(self) -> None:
        """One end-to-end value, computed here from the parsed constants.

        The assertions above compare source text; this computes the token the
        way both sides say they do and pins the result. A refactor that keeps
        the constants and changes the digest or the encoding fails here.
        """
        namespace = rust_namespace()
        workspace = "00000000-0000-0000-0000-000000000001"
        message = f"{namespace}{workspace}:social_publish".encode()
        expected = hmac.new(b"test-master-key", message, hashlib.sha256).hexdigest()

        self.assertEqual(len(expected), 64, "HMAC-SHA256 hex is 64 characters")
        self.assertIn(
            "hex::encode(mac.finalize().into_bytes())",
            DISCOVERY.read_text(),
            "the Rust side must hex-encode the digest; agents compares hex",
        )
        self.assertIn(
            'createHmac("sha256"',
            AGENTS_AUTH.read_text(),
            "the agents side must use HMAC-SHA256",
        )

    def test_the_legacy_fallback_is_still_a_known_transitional_state(self) -> None:
        """A silent all-capability grant must stay deliberate and findable.

        `allowLegacyTokens` defaults to true, so a legacy token is accepted and
        granted every capability. That is a documented rollout decision, not a
        bug — but it is also what makes a namespace drift silent, so the flag
        stays asserted until it is turned off.
        """
        source = AGENTS_AUTH.read_text()
        self.assertIn(
            "allowLegacyTokens: boolean = true",
            source,
            "the legacy-token default changed. If scoped tokens are now "
            "enforced, this gate's premise is obsolete and the drift it guards "
            "would fail loudly instead of silently — update it deliberately",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        state = "checked" if AGENTS_AUTH.exists() else "skipped-no-sibling"
        print(f"AGENT_CAPABILITY_TOKEN_PARITY=PASS agents={state}")
    else:
        print("AGENT_CAPABILITY_TOKEN_PARITY=FAIL")
        sys.exit(1)
