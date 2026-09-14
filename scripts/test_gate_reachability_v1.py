#!/usr/bin/env python3
"""Every gate must be reachable from the workflow that actually runs.

A gate nobody runs is a file. Three separate instances of this, all found by
looking rather than by a failure:

- `workspace-scope-ratchet.py` sat in `just policy-checks` and in no workflow.
  CLAUDE.md presents it beside the source-size and api-sql ratchets as "the most
  common self-inflicted CI failure"; it could not fail CI, because CI never ran
  it. It is the whole of tenant isolation — a statement reading one of the 232
  `workspace_id`-bearing tables without naming the workspace.

- `sql-result-types.py` and `test_sql_scalar_types_v1.py` need a live schema, and
  the one CI job with a migrated database did not call them. That is the gate
  CLAUDE.md names as covering the cheap half of having no compile-time SQL
  checking, after an uncast `EXTRACT` returning NUMERIC aborted every autopilot
  cycle for two hours having compiled, linted and passed 1684 tests.

- Earlier, gates wired into a workflow that was `disabled_manually`.

**Reachability means from `ci.yml` specifically.** Every other workflow in this
repository is either `workflow_dispatch`-only or disabled — only `ci.yml` and
`publish-images.yml` run on a push, and the YAML cannot say which are disabled
because that is an API state rather than a file. So a gate's presence in some
workflow proves nothing, and this gate deliberately asks the narrower question.

A script counts as reachable two ways: named explicitly in `ci.yml`, or matched by
the `unittest discover -s scripts -p 'test_*.py'` line that `ci.yml` runs. The
second is why most gates here need no wiring, and also why a hyphenated name or a
missing `test_` prefix silently opts a script out — which is what happened to both
of the SQL gates.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
JUSTFILE = ROOT / "justfile"
CI = ROOT / ".github/workflows/ci.yml"

SCRIPT_PATTERN = re.compile(r"scripts/[A-Za-z0-9_.\-]+\.(?:py|sh|ts)")

# The discovery command, and the pattern it expands to.
DISCOVERY = "unittest discover -s scripts -p 'test_*.py'"


def recipe_body(name: str) -> str:
    """The lines of one `just` recipe, up to the next unindented line."""
    text = JUSTFILE.read_text().splitlines()
    body: list[str] = []
    inside = False
    for line in text:
        if re.match(rf"^@?{re.escape(name)}(\s.*)?:", line):
            inside = True
            continue
        if inside:
            # A recipe ends at the first line that is neither indented nor blank.
            if line and not line[0].isspace():
                break
            body.append(line)
    return "\n".join(body)


def scripts_in(text: str) -> set[str]:
    return set(SCRIPT_PATTERN.findall(text))


class GateReachability(unittest.TestCase):
    def setUp(self) -> None:
        self.ci = CI.read_text()
        self.named_in_ci = scripts_in(self.ci)
        self.discovery_runs = DISCOVERY in self.ci

    def test_ci_still_runs_test_discovery(self):
        """Most gates here are reachable only through this one line."""
        self.assertTrue(
            self.discovery_runs,
            f"ci.yml must run `{DISCOVERY}`; without it every `test_*.py` gate "
            "in scripts/ stops running and nothing says so",
        )

    def test_every_policy_check_is_reachable_from_ci(self):
        policy = scripts_in(recipe_body("policy-checks"))
        self.assertTrue(policy, "could not read the policy-checks recipe")
        unreachable = sorted(
            script
            for script in policy
            if script not in self.named_in_ci
            and not (
                self.discovery_runs and Path(script).name.startswith("test_")
                and script.endswith(".py")
            )
        )
        self.assertEqual(
            unreachable,
            [],
            "these run in `just policy-checks` and never in CI, so they only "
            "catch anything when somebody remembers to run them locally:\n  "
            + "\n  ".join(unreachable),
        )

    def test_the_three_ratchets_are_all_in_ci(self):
        """Named, because CLAUDE.md presents them as one set and they were not."""
        for ratchet in (
            "scripts/source-size-ratchet.py",
            "scripts/api-sql-ratchet.py",
            "scripts/workspace-scope-ratchet.py",
        ):
            self.assertIn(
                ratchet,
                self.named_in_ci,
                f"{ratchet} is a ratchet with a committed baseline and a "
                "hyphenated name, so test discovery cannot reach it — ci.yml "
                "must call it by name",
            )

    def test_the_schema_gates_run_where_a_database_exists(self):
        """They can only run in the job that has Postgres, and must run there."""
        postgres_job = self.ci.split("rust-postgres:", 1)
        self.assertEqual(
            len(postgres_job), 2, "ci.yml no longer has a rust-postgres job"
        )
        # Everything from that job header to the next top-level job key.
        body = re.split(r"\n  [a-z][a-z0-9-]*:\n", postgres_job[1])[0]
        for gate in ("scripts/sql-result-types.py", "scripts/test_sql_scalar_types_v1.py"):
            self.assertIn(
                gate,
                body,
                f"{gate} needs a migrated schema and skips silently without one, "
                "so it belongs in the rust-postgres job and nowhere else",
            )


if __name__ == "__main__":
    unittest.main()
