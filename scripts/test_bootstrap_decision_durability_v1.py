#!/usr/bin/env python3
"""A deploy reconciles declared facts. It does not overrule decisions.

`scripts/deploy.sh` runs `setup` on every release, before either long-running
service starts, and `setup` runs everything in
`crates/crowdrelay-worker/src/bootstrap/`. That makes every statement in there an
unattended periodic writer: whatever its conflict clause forces, it forces again
on every deploy, forever, with nothing surfacing the reversal.

Two statements forced access back on:

  * `team.rs` set `workspace_members.status = 'active'` and
    `viryaos_team_profiles.active = true`;
  * `admission.rs` set `workspace_members.status = 'active'`.

`admission/support.rs` requires `m.status = 'active'` to operate a gate, and
`autopilot/{team,control}.rs` require `profile.active AND
member.status = 'active'` to route work. Nothing else in the codebase writes
either column, so the only way to turn somebody off was hand SQL — and the only
thing that ever turned them back on was deploying.

The rule this gate holds is the line between the bug and the legitimate case:

  * `column = EXCLUDED.column` or `= $n` takes the value from the bootstrap
    spec. That is declarative reconciliation, and reverting drift is the point.
  * `column = 'active'` / `= true` is a literal. No input can ask for the other
    value, so the column is write-only-on and a decision to turn it off cannot
    survive.

A `CASE` that preserves the off-state is how a literal becomes acceptable, and is
what both fixes use.

The matcher splits the SET list on commas rather than on newlines. A line-based
scan missed the `admission.rs` instance entirely, because that assignment shared
a line with two benign ones.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BOOTSTRAP = ROOT / "crates/crowdrelay-worker/src/bootstrap"

# Columns that record a decision somebody made, as opposed to a fact a refresh
# observes. Kept narrow on purpose: a wide list turns this gate into noise.
DECISION_COLUMN = re.compile(
    r"^(status|active|enabled|eligible|approved|verified|blocked|[a-z_]+_state)$"
)
LITERAL = re.compile(r"^('[a-z_]+'|true|false)$", re.I)
CONFLICT = re.compile(r"ON CONFLICT.*?DO UPDATE(?P<body>.*?)(?:RETURNING|\"#)", re.DOTALL | re.I)


def strip_sql_comments(text: str) -> str:
    """Drops `--` to end of line, not just whole comment lines.

    Dropping whole lines was not enough. A trailing comment glued itself to the
    front of the next comma-separated assignment, so the column name read as
    "<comment text> status" and stopped matching — the matcher reported clean on
    a real violation. Found by mutating the file it was written to guard.

    A `--` inside a string literal would be stripped too. None of these SQL
    bodies contain one; a gate that silently mis-parses is worse than one that
    is narrow, so this is stated rather than handled.
    """
    return "\n".join(line.split("--", 1)[0] for line in text.splitlines())


def decision_assignments(path: Path):
    """Yields (column, right-hand side) for each decision column a conflict
    clause in this file assigns."""
    text = path.read_text()
    for match in CONFLICT.finditer(text):
        # A trailing WHERE is a condition on the update, not part of the SET list.
        body = re.split(r"\n\s*WHERE\b", match.group("body"), maxsplit=1, flags=re.I)[0]
        # Drop the `SET` keyword. There is no comma between it and the first
        # assignment, so leaving it in made the first column read as
        # "SET <column>" — which silently exempted every single-assignment SET
        # list, including the one in `team.rs` this gate was written for. Only
        # the multi-assignment case in `admission.rs` was caught, so the gate
        # looked like it worked. Found by mutating both files, not one.
        body = re.sub(r"^\s*SET\b", "", strip_sql_comments(body), flags=re.I)
        for assignment in body.split(","):
            parts = assignment.split("=", 1)
            if len(parts) != 2:
                continue
            column = parts[0].strip().strip('"')
            if DECISION_COLUMN.match(column):
                yield column, " ".join(parts[1].split())


class BootstrapDecisionDurability(unittest.TestCase):
    def test_bootstrap_exists_where_this_gate_looks(self):
        self.assertTrue(
            BOOTSTRAP.is_dir(),
            "bootstrap moved; this gate is looking at nothing and will pass "
            "vacuously",
        )
        self.assertTrue(
            any(BOOTSTRAP.rglob("*.rs")),
            "no bootstrap sources found",
        )

    def test_setup_still_runs_on_every_deploy(self):
        """The premise. If a deploy stops running setup, re-read this file."""
        deploy = (ROOT / "scripts/deploy.sh").read_text()
        self.assertRegex(
            deploy,
            r"compose run --rm -T setup",
            "deploy.sh no longer runs setup; the periodic-writer premise behind "
            "this gate needs re-checking",
        )

    def test_no_bootstrap_conflict_clause_forces_a_decision_column(self):
        offenders = []
        for path in sorted(BOOTSTRAP.rglob("*.rs")):
            for column, rhs in decision_assignments(path):
                if LITERAL.match(rhs):
                    offenders.append(
                        f"{path.relative_to(ROOT).as_posix()}: {column} = {rhs}"
                    )
        self.assertEqual(
            offenders,
            [],
            "a bootstrap conflict clause assigns a decision column a literal. "
            "setup runs on every deploy, so this overrides the decision on every "
            "release and nothing reports it. Take the value from the spec "
            "(EXCLUDED/$n), or preserve the off-state with a CASE.",
        )

    def test_the_two_known_fixes_are_still_in_place(self):
        """Named, because a regression here is invisible in production."""
        team = (BOOTSTRAP / "team.rs").read_text()
        self.assertIn(
            "WHEN workspace_members.status = 'disabled' THEN 'disabled'",
            team,
            "team bootstrap must not re-enable a disabled member",
        )
        team_profile_conflict = team[team.index("viryaos_team_profiles") :]
        team_profile_conflict = team_profile_conflict[
            : team_profile_conflict.index('"#')
        ]
        self.assertNotRegex(
            strip_sql_comments(
                team_profile_conflict[team_profile_conflict.index("DO UPDATE") :]
            ),
            r"\bactive\s*=\s*true",
            "team bootstrap must not re-activate a deactivated profile",
        )
        admission = (BOOTSTRAP / "admission.rs").read_text()
        self.assertIn(
            "WHEN workspace_members.status = 'disabled' THEN 'disabled'",
            admission,
            "admission bootstrap must not re-enable a disabled member",
        )

    def test_the_gated_columns_still_gate_something(self):
        """The reason this matters, pinned where it is consumed. If these reads
        move, the durability requirement moves with them."""
        support = (ROOT / "crates/crowdrelay-infra/src/admission/support.rs").read_text()
        self.assertRegex(
            support,
            r"m\.status\s*=\s*'active'",
            "gate operation should still require an active member",
        )
        team = (ROOT / "crates/crowdrelay-infra/src/autopilot/team.rs").read_text()
        self.assertRegex(
            team,
            r"profile\.active AND member\.status\s*=\s*'active'",
            "work routing should still require an active member and profile",
        )


if __name__ == "__main__":
    unittest.main()
