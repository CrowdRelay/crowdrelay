#!/usr/bin/env python3
"""A switch that stops the product working must be in the config surface.

`CROWDRELAY_REDDIT_WRITE_ENABLED` gates every Reddit post. It is checked before
`CROWDRELAY_COMMUNITY_AUTO_POST` and overrides it, so with the write switch unset
the auto-post flag has no effect whatsoever.

It appeared in no `.env.example`, no compose file and no document — only in Rust
doc comments. An operator approved every brain suggestion, set auto-post true,
believed they had autopilot everywhere, and watched five drafts sit in
`awaiting_manual_post` with nothing anywhere naming the switch they were missing.

The one surface they did have said the opposite. `GrowthReadiness` reported
`community_executor_enabled: community_executor.is_some()` — the worker is
constructed in manual mode too, so the field answered "was it built" while its
own documentation claims to answer "will posts be published". Same shape as a
connection that reads `connected` with an invalid credential.

So this gate holds three things:

  * every `CROWDRELAY_*` variable the worker reads is named in `.env.example`;
  * the readiness fields report what will publish, not what was constructed;
  * the watchdog reports drafts that nothing will publish, and names the switch.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKER_SRC = ROOT / "crates/crowdrelay-worker/src"
ENV_EXAMPLE = ROOT / ".env.example"
MAIN = WORKER_SRC / "main.rs"
PLATFORMS = WORKER_SRC / "auto_post_platforms.rs"
CONDITIONS = WORKER_SRC / "ops_watchdog/conditions.rs"

# Variables read somewhere other than `.env.example` on purpose: secrets the
# deploy injects, and variables the container runtime sets. Each needs a reason.
EXEMPT = {
    # Set by the deploy script during blue-green cutover, never by an operator.
    "CROWDRELAY_WORKER_STANDBY",
    # Test-suite database URLs. The justfile and the CI workflows export these;
    # a production `.env` has no business carrying them.
    "CROWDRELAY_OUTBOX_TEST_DATABASE_URL",
    "CROWDRELAY_REMINDER_TEST_DATABASE_URL",
    "CROWDRELAY_RETENTION_TEST_DATABASE_URL",
}


def variables_read_by_the_worker() -> set[str]:
    names = set()
    for path in WORKER_SRC.rglob("*.rs"):
        if path.name.endswith("tests.rs"):
            continue
        text = path.read_text()
        names.update(re.findall(r'env::var\("(CROWDRELAY_[A-Z0-9_]+)"\)', text))
        # `flag("CROWDRELAY_...")` and any other single-argument helper that
        # takes the variable name directly.
        names.update(re.findall(r'\b\w+\("(CROWDRELAY_[A-Z0-9_]+)"\)', text))
    return names


class PublishingSwitches(unittest.TestCase):
    def test_every_switch_the_worker_reads_is_documented(self):
        documented = set(
            re.findall(r"^(CROWDRELAY_[A-Z0-9_]+)=", ENV_EXAMPLE.read_text(), re.M)
        ) | set(re.findall(r"(CROWDRELAY_[A-Z0-9_]+)", ENV_EXAMPLE.read_text()))
        missing = sorted(variables_read_by_the_worker() - documented - EXEMPT)
        self.assertEqual(
            missing,
            [],
            "these variables change what the product does and are absent from "
            ".env.example, so nobody configuring it can discover them: "
            f"{missing}",
        )

    def test_the_reddit_write_switch_is_documented_as_load_bearing(self):
        """Present is not enough: it has to say that it overrides the other."""
        text = ENV_EXAMPLE.read_text()
        self.assertIn("CROWDRELAY_REDDIT_WRITE_ENABLED", text)
        block = text[text.index("CROWDRELAY_COMMUNITY_AUTO_POST") :]
        block = block[: block.index("CROWDRELAY_REDDIT_WRITE_ENABLED") + 200]
        self.assertIn(
            "no effect",
            block,
            "the file must say that CROWDRELAY_COMMUNITY_AUTO_POST does nothing "
            "without the write switch; an operator who sets one and not the "
            "other gets silence",
        )

    def test_readiness_reports_what_will_publish_not_what_was_built(self):
        main = MAIN.read_text()
        self.assertIn(
            "community_executor_enabled: posture.reddit.publishes()",
            main,
            "the readiness field must answer 'will posts be published'. "
            "`community_executor.is_some()` answers 'was the worker built', "
            "which is true in manual mode — that is how the only surface an "
            "operator had reported Reddit posting as enabled while it would "
            "never post.",
        )
        self.assertNotIn(
            "social_post_executor_enabled: true",
            main,
            "a hardcoded true cannot be a reading; the social executor's own "
            "documentation says it runs in manual mode",
        )

    def test_one_posture_is_read_so_the_surfaces_cannot_disagree(self):
        main = MAIN.read_text()
        self.assertEqual(
            len(re.findall(r"PublishingPosture::from_env\(", main)),
            1,
            "read the switches once. Two readers drift, and the readiness log, "
            "the executor's mode and the watchdog must not disagree about "
            "whether anything will publish.",
        )

    def test_the_switch_order_puts_the_overriding_one_first(self):
        platforms = PLATFORMS.read_text()
        order = re.findall(r'missing: "(CROWDRELAY_[A-Z0-9_]+)"', platforms)
        self.assertEqual(
            order[:3],
            [
                "CROWDRELAY_REDDIT_WRITE_ENABLED",
                "CROWDRELAY_COMMUNITY_AUTO_POST",
                "CROWDRELAY_AGENT_SERVICE_AUTH_KEY",
            ],
            "the write switch overrides the other two, so it must be reported "
            "first. Naming the auto-post flag instead sends an operator to "
            "check a switch they already set.",
        )

    def test_the_watchdog_reports_drafts_nothing_will_publish(self):
        conditions = CONDITIONS.read_text()
        self.assertIn("publishing.drafts_with_no_publisher", conditions)
        block = conditions[conditions.index("publishing.drafts_with_no_publisher") :]
        block = block[: block.index("Condition {", 10)] if "Condition {" in block[10:] else block
        self.assertIn(
            "missing_switch",
            block,
            "the condition must name the switch. The count of waiting drafts is "
            "what the operator could already see; the switch is what they could "
            "not.",
        )
        self.assertIn(
            "!posture.reddit.publishes()",
            block,
            "both halves are required: drafts with no publisher is the fault, "
            "an empty queue with publishing off is a setting",
        )


if __name__ == "__main__":
    unittest.main()
