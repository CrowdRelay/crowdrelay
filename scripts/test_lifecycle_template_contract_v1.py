#!/usr/bin/env python3
"""Every lifecycle template CrowdRelay emits must be documented for the executor.

`crowdrelay.fan_lifecycle.message_requested` names its message with a
`template_key`. CrowdRelay decides which one; something outside this repository
renders and sends it. That consumer had no list to work from — the executor
contract described `template_key` for play steps and never for lifecycle
messages — so it was written against the keys its author happened to have seen.

Three of the seven were missed, and because the handler ended in a default
branch rather than a failure, each was rendered as the Synesthesia follow-up:
`first_ticket_thanks`, which `audience_lifecycle.rs` calls "the single best
moment the band will ever get to turn a buyer into a fan"; `returning_thanks`;
and `referral_thanks`, which fires when a referral converts and is the payoff of
the only compounding loop the product has. A fan would have been thanked for a
quiz they never took, at the moment they had just brought somebody in.

None had fired yet. All three fire as soon as fans start converting, which is
the week this was found.

The fix a repository can enforce is not the executor's control flow — that lives
elsewhere. It is that the vocabulary stops being something a consumer has to
infer. Adding a variant to `LifecycleTemplate` now fails CI until the executor
contract documents what the new key means and when it is sent.
"""
from __future__ import annotations

import re
import unittest
from pathlib import Path

try:
    from rust_source_tree import read_rust_module
except ImportError:  # pragma: no cover - runner always has scripts on sys.path
    import sys

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from rust_source_tree import read_rust_module

ROOT = Path(__file__).resolve().parents[1]
# The evaluator splits its candidate builders across include! chunks; read the
# module root so a later split does not silently zero the emitted-key set.
EVALUATE_MODULE = "crates/crowdrelay-application/src/autopilot/evaluate.rs"
CONTRACT = ROOT / "n8n" / "crowdrelay-executor-contract.md"
EXECUTION = ROOT / "crates" / "crowdrelay-infra" / "src" / "autopilot" / "actions_execution.rs"
TENANT_SETTINGS = ROOT / "crates" / "crowdrelay-infra" / "src" / "tenant_settings.rs"
WORKFLOW = ROOT / "n8n" / "examples" / "autopilot-fan-lifecycle.example.json"


def emitted_keys() -> set[str]:
    """The template keys the brain can put on the wire."""
    text = read_rust_module(ROOT, EVALUATE_MODULE)
    return set(re.findall(r'LifecycleTemplate::\w+\s*=>\s*"([a-z0-9._]+)"', text))


def documented_keys() -> set[str]:
    """The keys the executor contract names in its lifecycle table."""
    text = CONTRACT.read_text(encoding="utf-8")
    section = text.split("### Fan lifecycle messages", 1)
    if len(section) < 2:
        return set()
    body = section[1].split("\n## ", 1)[0]
    return set(re.findall(r"`(crowdrelay\.[a-z0-9._]+\.v\d+)`", body))


class TheVocabularyIsPublished(unittest.TestCase):
    def test_the_brain_still_emits_template_keys(self) -> None:
        """Guards the regex: a silent empty set would make this test vacuous."""
        keys = emitted_keys()
        self.assertGreaterEqual(
            len(keys),
            6,
            "found almost no lifecycle template keys under evaluate.rs; the match "
            "arm shape changed and this contract is no longer reading it",
        )

    def test_every_emitted_key_is_documented(self) -> None:
        missing = sorted(emitted_keys() - documented_keys())
        self.assertEqual(
            missing,
            [],
            "these template keys are emitted but absent from the lifecycle table in "
            f"n8n/crowdrelay-executor-contract.md: {missing}. The executor renders by "
            "key; one it has never been told about is one it renders as something "
            "else.",
        )

    def test_no_documented_key_has_been_retired(self) -> None:
        """A key in the table that nothing emits sends the reader hunting."""
        stale = sorted(documented_keys() - emitted_keys() - {"crowdrelay.fan.welcome.v1"})
        self.assertEqual(
            stale,
            [],
            f"the contract documents template keys nothing emits: {stale}",
        )

    def test_the_contract_says_to_fail_on_an_unknown_key(self) -> None:
        """The instruction is the whole point of the section.

        A default branch is why this happened. Without the instruction the table
        is a list of keys somebody may still choose to fall through.
        """
        body = CONTRACT.read_text(encoding="utf-8")
        section = body.split("### Fan lifecycle messages", 1)
        self.assertGreater(len(section), 1, "the lifecycle section is gone")
        self.assertIn(
            "Fail on a key you do not know",
            section[1].split("\n## ", 1)[0],
            "the lifecycle section no longer tells the executor to fail on an "
            "unknown key, which is the instruction that stops a wrong message "
            "being sent in place of a missing one",
        )

    def test_the_referral_invite_documents_where_its_code_lives(self) -> None:
        """It is the only key carrying `fan.referral_code`, and the link cannot
        be built without knowing that. The first invite to fire reached the
        executor before the emitter carried the code, and the executor was right
        to throw rather than send an invite with no link in it."""
        body = CONTRACT.read_text(encoding="utf-8").split("### Fan lifecycle messages", 1)[1]
        section = body.split("\n## ", 1)[0]
        self.assertIn("fan.referral_code", section)
        self.assertIn("fan.referral_url", section)

    def test_referral_invite_url_is_tenant_native_and_emitted_complete(self) -> None:
        """The executor must never learn a band's hostname.

        The runtime owns tenant settings, so it emits the complete URL and a
        consumer can only send it verbatim. This is what makes the fan→fan
        loop safe for a second band, roster or label.
        """
        contract = CONTRACT.read_text(encoding="utf-8")
        section = contract.split("### Fan lifecycle messages", 1)[1].split("\n## ", 1)[0]
        execution = EXECUTION.read_text(encoding="utf-8")
        settings = TENANT_SETTINGS.read_text(encoding="utf-8")
        self.assertIn('"referral_url": referral_url', execution)
        self.assertIn("brand.referral_url(&code)", execution)
        self.assertIn("pub fn referral_url(&self, code: &str)", settings)
        self.assertNotIn("https://virya.music/r/", section)
        self.assertIn("never construct a hostname", section)

        workflow = WORKFLOW.read_text(encoding="utf-8")
        self.assertIn("referral_url", workflow)
        self.assertIn("brand.wordmark", workflow)
        self.assertNotIn("https://virya.music/r/", workflow)
        self.assertNotIn("'- VIRYA'", workflow)
        self.assertNotIn("'Witamy w VIRYA'", workflow)
        self.assertIn("crowdrelay_workspace_wordmark", execution)


if __name__ == "__main__":
    unittest.main()
