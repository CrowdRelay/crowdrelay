#!/usr/bin/env python3
"""The watchdog's documented alarms must be the alarms it actually has.

`ops_watchdog.rs` opened with "The watchdog monitors ten conditions:" and a list
of ten, while `conditions()` returned seventeen. Seven alarms were undocumented,
two of them critical — `safety.off_platform_push_proposed` and
`learning.posterior_never_updated` — and a third, `brain.phase_failing_every_cycle`.

Nothing was broken. That is what makes it worth a gate. This repository has a
recorded habit of concluding a live capability is missing by reading a stale
list: CLAUDE.md says six times in one session that a capability reported missing
was live in production. The watchdog's module doc is the only place the alarm
surface is described in prose, so a reader deciding whether an alarm exists reads
exactly the list that was wrong.

Three things are checked, each of which was false when this was written:

- Every key `conditions()` returns appears in the module doc.
- The doc names no condition that no longer exists.
- The stated count matches the real one.

A fourth check earns its place for a different reason: every condition must be
named in the watchdog's tests. That one passed at 17/17 already, so it is a
ratchet on something good rather than a fix — an alarm with no test is an alarm
nobody has ever seen fire, and the operator finds out whether it works during the
incident it exists for.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WATCHDOG = ROOT / "crates/crowdrelay-worker/src/ops_watchdog.rs"
CONDITIONS = ROOT / "crates/crowdrelay-worker/src/ops_watchdog/conditions.rs"
TESTS = ROOT / "crates/crowdrelay-worker/src/ops_watchdog/tests.rs"

# `key: "growth.feed_failing",` in the conditions list.
KEY = re.compile(r'key:\s*"([a-z_]+\.[a-z_]+)"')
# A bullet in the module doc: `//! - `growth.feed_failing` — …`
BULLET = re.compile(r"^//! - `([a-z_]+\.[a-z_]+)`", re.M)

NUMBER_WORDS = {
    "ten": 10,
    "eleven": 11,
    "twelve": 12,
    "thirteen": 13,
    "fourteen": 14,
    "fifteen": 15,
    "sixteen": 16,
    "seventeen": 17,
    "eighteen": 18,
    "nineteen": 19,
    "twenty": 20,
}


class WatchdogConditionsDocumented(unittest.TestCase):
    def setUp(self) -> None:
        self.keys = KEY.findall(CONDITIONS.read_text())
        self.doc = WATCHDOG.read_text()
        self.documented = set(BULLET.findall(self.doc))

    def test_the_conditions_list_was_found(self):
        """If the shape changes, every other check here silently passes."""
        self.assertGreaterEqual(
            len(self.keys),
            10,
            "found too few condition keys in conditions.rs — the `key: \"...\"` "
            "shape this gate matches has probably changed",
        )
        self.assertEqual(
            len(self.keys),
            len(set(self.keys)),
            f"two conditions share a key, so one cannot be told from the other "
            f"in `ops/attention`: {sorted(self.keys)}",
        )

    def test_every_condition_is_documented(self):
        missing = sorted(set(self.keys) - self.documented)
        self.assertEqual(
            missing,
            [],
            "these alarms exist and the module doc does not mention them, so a "
            "reader deciding whether the watchdog covers something will conclude "
            f"it does not: {missing}",
        )

    def test_the_doc_names_no_condition_that_does_not_exist(self):
        stale = sorted(self.documented - set(self.keys))
        self.assertEqual(
            stale,
            [],
            "the module doc describes alarms that `conditions()` no longer "
            f"returns, which is worse than silence: {stale}",
        )

    def test_the_stated_count_is_the_real_count(self):
        match = re.search(r"watchdog monitors (\w+) conditions", self.doc)
        self.assertIsNotNone(
            match, "the module doc no longer states how many conditions there are"
        )
        word = match.group(1)
        stated = NUMBER_WORDS.get(word) or (int(word) if word.isdigit() else None)
        self.assertIsNotNone(
            stated,
            f"could not read '{word}' as a number; add it to NUMBER_WORDS or "
            "write the digits",
        )
        self.assertEqual(
            stated,
            len(self.keys),
            f"the doc says {word} conditions and there are {len(self.keys)}",
        )

    def test_every_condition_has_a_test(self):
        """An alarm with no test is one nobody has seen fire."""
        tests = TESTS.read_text()
        untested = sorted(key for key in set(self.keys) if key not in tests)
        self.assertEqual(
            untested,
            [],
            "these conditions are never named in the watchdog's tests, so "
            "whether they fire is unknown until the incident they exist for: "
            f"{untested}",
        )


if __name__ == "__main__":
    unittest.main()
