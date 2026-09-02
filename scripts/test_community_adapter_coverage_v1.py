#!/usr/bin/env python3
"""Every community platform an operator can seed must have an adapter.

The community-intelligence sweep matches work by platform:

    WHERE platform = $1 AND status = 'active'      -- $1 = adapter.id()

Production seeded 66 active `discovery_places` — 28 reddit, 12 forum, 10
telegram, 8 discord, 5 lemmy, 3 instagram — and registered exactly one
adapter, `brutalland`. No place carries that platform, so every sweep matched
zero rows, took the `places.is_empty()` branch, recorded a success, and wrote
nothing. `community_observations` and `community_entities` were both empty
while the worker reported healthy.

Nothing failed. That is what made it survive: a source with no work looks
identical to a source with nothing to report.

This checks the two halves of that trap:

* every adapter registered in the worker is a real `SourceAdapter` whose `id()`
  is a platform the seed data actually uses, and
* the empty-places branch is loud, so the next unclaimed platform shows up in
  the log on the first sweep instead of months later in a table count.

It does not require an adapter for every platform — telegram and lemmy have
none yet, and that is a roadmap fact, not a regression. It requires that the
gap is visible.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKER = ROOT / "crates/crowdrelay-worker/src"
MOD = WORKER / "community_intelligence/mod.rs"
SWEEP = WORKER / "community_intelligence/worker.rs"
MAIN = WORKER / "main.rs"

# Platforms that `discovery_places` rows are actually seeded with. An adapter
# whose id is outside this set can never match a single row.
SEEDED_PLATFORMS = {
    "reddit",
    "forum",
    "telegram",
    "discord",
    "lemmy",
    "instagram",
    "brutalland",
}


def adapter_ids() -> dict[str, str]:
    """Maps each adapter module to the id its `fn id()` returns."""
    ids: dict[str, str] = {}
    for module in (WORKER / "community_intelligence").glob("*.rs"):
        if module.name in {"mod.rs", "worker.rs", "adapter.rs"}:
            continue
        source = module.read_text()
        found = re.search(r'fn id\(&self\) -> &str \{\s*"([^"]+)"', source)
        if found:
            ids[module.stem] = found.group(1)
    return ids


class CommunityAdapterCoverage(unittest.TestCase):
    def test_at_least_one_adapter_exists(self) -> None:
        self.assertTrue(adapter_ids(), "no community source adapters found at all")

    def test_every_adapter_id_can_match_a_seeded_place(self) -> None:
        for module, adapter_id in adapter_ids().items():
            self.assertIn(
                adapter_id,
                SEEDED_PLATFORMS,
                f"adapter {module} claims platform {adapter_id!r}, which no "
                f"discovery_places row uses; it will sweep forever and observe "
                f"nothing. Seeded platforms: {sorted(SEEDED_PLATFORMS)}",
            )

    def test_reddit_is_claimed(self) -> None:
        """28 of the 66 seeded places are Reddit — the largest single block."""
        self.assertIn(
            "reddit",
            adapter_ids().values(),
            "no adapter claims platform 'reddit'; the 28 active Reddit "
            "discovery places would be observed by nothing",
        )

    def test_every_adapter_module_is_registered_in_the_worker(self) -> None:
        """An adapter nobody constructs is the same as no adapter."""
        main = MAIN.read_text()
        declared = MOD.read_text()
        for module in adapter_ids():
            self.assertIn(
                f"pub mod {module};",
                declared,
                f"{module}.rs is not declared in community_intelligence/mod.rs",
            )
            struct = "".join(part.capitalize() for part in module.split("_")) + "Adapter"
            self.assertIn(
                struct,
                main,
                f"{struct} is never constructed in the worker's adapter list, "
                f"so its platform stays unclaimed",
            )

    def test_a_source_with_no_places_says_so(self) -> None:
        """Silence on the empty branch is what hid this for months."""
        source = SWEEP.read_text()
        empty_branch = re.search(
            r"if places\.is_empty\(\) \{(.*?)\n        \}", source, re.DOTALL
        )
        self.assertIsNotNone(
            empty_branch, "the empty-places branch has moved; update this check"
        )
        body = empty_branch.group(1)
        self.assertRegex(
            body,
            r"warn!|error!",
            "a source that matches no places records a success and returns "
            "silently; that is indistinguishable from a healthy source with "
            "nothing to report, and it is exactly how 28 unclaimed Reddit "
            "places went unnoticed",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"COMMUNITY_ADAPTER_COVERAGE=PASS adapters={len(adapter_ids())}")
    else:
        print("COMMUNITY_ADAPTER_COVERAGE=FAIL")
        sys.exit(1)
