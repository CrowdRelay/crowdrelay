#!/usr/bin/env python3
"""A ruled-out community must stay ruled out across a scrape.

`discovery_places.status` is active/archived/blocked. Both upserts in
`crowdrelay-infra/src/audience_graph.rs` used to include `status = 'active'` in
their `ON CONFLICT ... DO UPDATE` clause, and those two statements are the only
writers of the column in the whole codebase. Neither of the other two values
could therefore persist: discovery re-reads the same sources on a schedule, so
every run silently un-blocked every community somebody had ruled out.

The effect was not cosmetic. `agent_outcomes` derives `refused_by_us_or_them`
from `place.status == "blocked"`, `target_discovery::screen_community_candidate`
refuses on that, and the join executor only acts on `status = 'active'`. A
revived status makes a ruled-out community targetable again, all the way to a
post.

Production has drafts for `r/whatisthisthing`, `r/metalgearsolid` and
`r/MetalMemes` — discovery matched the substring "metal". Which communities
deserve a block is a judgement no gate can make. That the judgement survives is
exactly what a gate can hold.

The membership dimension is separately durable: nothing upserts
`membership_state`, so `not_a_fit` and `rejected` already persist, and
`/v1/control-plane/community-intelligence/communities/{place_id}/membership`
records them. This keeps the two dimensions consistent, which is what migration
0217 said they were.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
GRAPH = ROOT / "crates/crowdrelay-infra/src/audience_graph.rs"
CRATES = ROOT / "crates"

CONFLICT = re.compile(
    r"INSERT INTO discovery_places(?P<body>.*?)RETURNING", re.DOTALL
)


def place_upserts() -> list[str]:
    return [match.group("body") for match in CONFLICT.finditer(GRAPH.read_text())]


class DiscoveryPlaceStatusDurability(unittest.TestCase):
    def test_both_upserts_are_still_here(self):
        """If the count changes, the rest of this file is reasoning about the
        wrong statements and should be re-read rather than trusted."""
        self.assertEqual(
            len(place_upserts()),
            2,
            "audience_graph holds two discovery_places upserts; a third needs "
            "the same treatment",
        )

    def test_no_upsert_resets_the_status(self):
        for index, body in enumerate(place_upserts()):
            conflict = body[body.index("ON CONFLICT") :]
            assignments = [
                line.strip()
                for line in conflict.splitlines()
                # Skip SQL comments: the reason this rule exists is written in
                # them, and it names the column.
                if line.strip() and not line.strip().startswith("--")
            ]
            offenders = [line for line in assignments if re.match(r"status\s*=", line)]
            self.assertEqual(
                offenders,
                [],
                f"upsert {index} assigns status on conflict ({offenders}). A "
                "conflict means discovery saw the place again — an observation "
                "that it exists, not a decision that we want it.",
            )

    def test_the_column_has_no_other_writer_to_compensate(self):
        """The rule above is only sufficient because nothing else writes it.

        If a repository ever gains a deliberate `SET status = ...`, that is
        fine — but this gate's reasoning ("the upserts are the only writers")
        stops holding, and whoever adds it should read the tests here.
        """
        writers = []
        for path in CRATES.rglob("*.rs"):
            if "/tests/" in path.as_posix():
                continue
            text = path.read_text()
            for match in re.finditer(r"UPDATE discovery_places(?P<body>.{0,600})", text, re.DOTALL):
                body = match.group("body")
                statement = body[: body.find("WHERE")] if "WHERE" in body else body
                if re.search(r"\bstatus\s*=", statement):
                    writers.append(path.relative_to(ROOT).as_posix())
        self.assertEqual(
            sorted(set(writers)),
            [],
            "a new deliberate writer of discovery_places.status appeared; "
            "re-read scripts/test_discovery_place_status_durability_v1.py and "
            "crates/crowdrelay-infra/tests/discovery_place_status_postgres.rs "
            "before assuming this gate still covers the behaviour",
        )

    def test_the_refusal_paths_still_read_the_status(self):
        """The reason a durable status matters, pinned where it is consumed."""
        outcomes = (ROOT / "crates/crowdrelay-worker/src/agent_outcomes.rs").read_text()
        self.assertIn(
            '== "blocked"',
            outcomes,
            "refused_by_us_or_them reads the place status; if that moves, the "
            "durability requirement moves with it",
        )
        discovery = (
            ROOT / "crates/crowdrelay-domain/src/target_discovery.rs"
        ).read_text()
        self.assertIn(
            "refused_by_us_or_them",
            discovery,
            "screening must still refuse a place we have ruled out",
        )

    def test_an_upsert_still_refreshes_what_a_scrape_is_for(self):
        """Leaving status alone must not turn the upsert into a no-op."""
        for body in place_upserts():
            conflict = body[body.index("ON CONFLICT") :]
            for column in ("name", "genres", "member_count", "updated_at"):
                self.assertRegex(
                    conflict,
                    rf"\b{column}\s*=",
                    f"the conflict clause must still refresh {column}: a "
                    "blocked place is still one we observe",
                )


if __name__ == "__main__":
    unittest.main()
