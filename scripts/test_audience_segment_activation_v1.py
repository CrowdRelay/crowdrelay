#!/usr/bin/env python3
"""While the autopilot forces `audience_segments.active` true, nothing may set it false.

This is a conditional gate for a latent defect rather than a live one, and the
condition is the point.

Three autopilot statements upsert `audience_segments` with `active=true` in the
conflict clause, across two files:

  * `autopilot/operations/execution.rs` — two of them
  * `autopilot/operations/show_growth_execution.rs` — one

That is the P23 shape — a literal assigned to a decision column by an unattended
periodic writer, and the autopilot cycle runs every five minutes. It is not a live
bug today for one reason only: **nothing anywhere sets that column false**, so it
has a single reachable value and there is no decision to revert.

The readers make the stakes concrete. `audience/campaign_handlers.rs` requires
`AND active` to attach a campaign to a segment, and `execute_signal_push` in
`operations/execution.rs` treats an inactive segment as a reason to fall back to
broadcasting to *every* consented fan. So an operator deactivating a segment is
doing two things at once — stopping targeted sends through it, and widening an
autopilot push that names it — and the autopilot would undo the first within five
minutes while the second stayed undone.

No behavioural test can fail today: there is no deactivation path to exercise. So
this gate encodes the implication instead. The day somebody adds one, it fails and
names the conflict clauses that have to stop forcing the column true.

Written because "recorded in a note" was not good enough. A note is read by whoever
goes looking; this fails the build for whoever adds the path.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CRATES = ROOT / "crates"

TABLE = "audience_segments"
# Three statements across two files: `execution.rs` carries two of them.
FORCING_STATEMENT_COUNT = 3
FORCING_FILES = (
    "crates/crowdrelay-infra/src/autopilot/operations/execution.rs",
    "crates/crowdrelay-infra/src/autopilot/operations/show_growth_execution.rs",
)


def rust_sources() -> list[Path]:
    return [
        path
        for path in sorted(CRATES.rglob("*.rs"))
        if "target" not in path.parts
    ]


def forcing_statements() -> list[tuple[str, int]]:
    """Every statement that assigns this column a literal true on conflict."""
    found: list[tuple[str, int]] = []
    for path in rust_sources():
        text = path.read_text()
        for match in re.finditer(
            rf"INSERT INTO {TABLE}\b.{{0,1200}}?ON CONFLICT.{{0,400}}?DO UPDATE(?P<set>.{{0,400}}?)(?:RETURNING|\"#)",
            text,
            re.DOTALL | re.I,
        ):
            if re.search(r"\bactive\s*=\s*true\b", match.group("set"), re.I):
                line = text[: match.start()].count("\n") + 1
                found.append((path.relative_to(ROOT).as_posix(), line))
    return found


def deactivation_sites() -> list[tuple[str, int, str]]:
    """Anything that sets the column to false, or to a bound parameter.

    A bound parameter counts: `SET active = $3` can carry false, and this gate
    cannot know what the caller passes. Either way the column gains a second
    reachable value, which is the condition that makes the forcing clauses a live
    defect.
    """
    found: list[tuple[str, int, str]] = []
    for path in rust_sources():
        if "tests" in path.parts or path.name.endswith("tests.rs"):
            continue
        text = path.read_text()
        for match in re.finditer(
            rf"(?:UPDATE\s+{TABLE}\b|INSERT INTO {TABLE}\b)(?P<body>.{{0,1200}})",
            text,
            re.DOTALL | re.I,
        ):
            body = match.group("body")
            for assignment in re.finditer(r"\bactive\s*=\s*(false|\$\d+)", body, re.I):
                line = text[: match.start()].count("\n") + 1
                found.append(
                    (path.relative_to(ROOT).as_posix(), line, assignment.group(1))
                )
    return found


class AudienceSegmentActivation(unittest.TestCase):
    def test_the_forcing_statements_are_still_the_ones_this_gate_knows(self):
        """If the set changes, the reasoning below is about the wrong code.

        Counted, not just grouped by file. A file-set comparison passed while a
        mutation removed one of the two forcing statements in `execution.rs` —
        the other one kept the file in the set. That is the third time today a
        membership check has hidden a change to one of several instances.
        """
        sites = forcing_statements()
        self.assertEqual(
            len(sites),
            FORCING_STATEMENT_COUNT,
            f"there are now {len(sites)} statements forcing "
            f"audience_segments.active true, not {FORCING_STATEMENT_COUNT}: "
            f"{sites}. If one was removed, good — check whether the rest of this "
            "gate still has a reason to exist. If one was added, it needs the "
            "same scrutiny as the others.",
        )
        self.assertEqual(
            {path for path, _ in sites},
            set(FORCING_FILES),
            "the statements that force audience_segments.active true have moved. "
            "Re-read this gate before trusting it: its whole argument is that "
            "these are the only writers and the column has one reachable value.",
        )

    def test_nothing_can_deactivate_a_segment_while_the_autopilot_revives_it(self):
        sites = deactivation_sites()
        self.assertEqual(
            sites,
            [],
            "something now sets audience_segments.active to false or to a bound "
            f"value ({sites}), so the column has a second reachable value. The "
            "autopilot upserts in "
            + ", ".join(FORCING_FILES)
            + " force it true on conflict and run every five minutes, so a "
            "deactivated segment is revived within one cycle. Remove "
            "`active=true` from those conflict clauses — re-seeing a segment is "
            "an observation that it exists, not a decision that it should send.",
        )

    def test_the_readers_still_make_this_matter(self):
        """Pinned because the stakes are what justify the rule.

        If the readers stop distinguishing active from inactive, this gate is
        guarding a column nobody acts on and should be deleted rather than kept
        as decoration.
        """
        campaigns = (
            CRATES / "crowdrelay-api/src/audience/campaign_handlers.rs"
        ).read_text()
        self.assertRegex(
            campaigns,
            rf"FROM {TABLE}\s+WHERE workspace_id = \$1 AND slug = \$2 AND active",
            "attaching a campaign to a segment should still require an active one",
        )
        execution = (
            CRATES / "crowdrelay-infra/src/autopilot/operations/execution.rs"
        ).read_text()
        self.assertIn(
            "inactive",
            execution,
            "execute_signal_push should still document that an inactive segment "
            "widens the push to every consented fan rather than narrowing it",
        )


if __name__ == "__main__":
    unittest.main()
