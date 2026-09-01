#!/usr/bin/env python3
"""Keep the alert channel worth reading, and every alert actionable.

Two failures this repo has already had, encoded so they cannot recur:

1. **Every alert went to Discord.** Warnings and criticals shared one receiver,
   so a single permanently-failed message interrupted a human. A channel that
   cries wolf gets muted, and a muted channel is worse than no channel because
   it still looks like coverage.

2. **Nothing said what to do.** An alert that reports a symptom and leaves the
   operator to guess costs more than it saves, and the guess is usually
   "redeploy" — the most intrusive option, applied to problems a single retry
   would have fixed.

So: only `critical` reaches Discord, `critical` may not fire on a single
failure, and every rule carries a remedy that names the cheapest fix first.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
ALERTS = ROOT / "deploy/observability/alerts.yml"
ALERTMANAGER = ROOT / "deploy/observability/alertmanager.yml"

# Words that describe the most intrusive remedies. A critical alert may
# legitimately end in one, but it must not *start* there.
INTRUSIVE = ("redeploy", "roll back", "rollback")


def rules() -> list[dict]:
    document = yaml.safe_load(ALERTS.read_text())
    return [rule for group in document["groups"] for rule in group["rules"]]


class AlertPolicy(unittest.TestCase):
    def test_every_alert_says_what_to_do(self) -> None:
        for rule in rules():
            remedy = rule.get("annotations", {}).get("remedy", "").strip()
            self.assertTrue(
                remedy,
                f"{rule['alert']} has no remedy: an operator paged by this has "
                f"to guess, and the guess is usually a redeploy",
            )

    def test_only_critical_reaches_discord(self) -> None:
        document = yaml.safe_load(ALERTMANAGER.read_text())
        route = document["route"]
        self.assertNotEqual(
            route.get("receiver"),
            "discord",
            "the default route sends every severity to Discord; that is the "
            "configuration that trained the operator to ignore the channel",
        )
        discord_routes = [
            child
            for child in route.get("routes", [])
            if child.get("receiver") == "discord"
        ]
        self.assertTrue(discord_routes, "nothing routes to Discord at all")
        for child in discord_routes:
            matchers = " ".join(child.get("matchers", []))
            self.assertIn(
                'severity = "critical"',
                matchers,
                f"a Discord route matches more than critical: {matchers!r}",
            )

    def test_a_single_failure_is_never_critical(self) -> None:
        """`> 0` as a paging threshold is how one dead message wakes someone.

        Counting alerts must page on a rate or a real quantity. Liveness and
        ratio alerts are exempt: `up == 0` and a 5% error ratio are already
        statements about the system, not about one record.
        """
        for rule in rules():
            if rule["labels"]["severity"] != "critical":
                continue
            expr = rule["expr"]
            if "up{" in expr or "/" in expr or "lease_age" in expr:
                continue
            self.assertNotRegex(
                expr,
                r">\s*0\s*$",
                f"{rule['alert']} pages on a single occurrence: {expr}",
            )

    def test_criticals_do_not_open_with_the_biggest_hammer(self) -> None:
        for rule in rules():
            if rule["labels"]["severity"] != "critical":
                continue
            remedy = rule["annotations"]["remedy"]
            first = re.split(r"(?<=[.!])\s+", remedy.strip())[0].lower()
            for word in INTRUSIVE:
                self.assertNotIn(
                    word,
                    first,
                    f"{rule['alert']} opens its remedy with '{word}'. Lead with "
                    f"the cheapest fix; redeploying because a message was "
                    f"rejected is the habit this check exists to prevent",
                )

    def test_the_worker_is_watched(self) -> None:
        """The regression that motivated all of this.

        The worker serves no HTTP, so `up{job=...}` cannot cover it. It died on
        a deploy and stayed dead with every alert quiet.
        """
        names = {rule["alert"] for rule in rules()}
        self.assertIn(
            "CrowdRelayWorkerDown",
            names,
            "nothing alerts on the process that runs the brain, the outbox and "
            "every metric sync",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        every = rules()
        critical = [r for r in every if r["labels"]["severity"] == "critical"]
        print(f"ALERT_POLICY=PASS rules={len(every)} discord={len(critical)}")
    else:
        print("ALERT_POLICY=FAIL")
        sys.exit(1)
