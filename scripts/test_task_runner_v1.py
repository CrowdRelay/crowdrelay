"""Contract tests for the task runner and CI drift guard.

`just` replaced the Makefile. The justfile is the source of truth for local
gates; CI inlines the same chain because installing a third-party binary
there costs more supply-chain surface than it saves. These tests keep the
two honest:

- the Makefile is gone and stays gone (no silent resurrection);
- the justfile exposes the canonical recipes (`check`, `ci`, `test-postgres`);
- the CI workflow's inline check block runs the same command set as
  `just ci`, so one cannot gain a gate the other lacks;
- the fallback workflow, which exists because CI runs on one self-hosted
  machine and that machine can be offline, runs the same chain too. A
  fallback that gates less than the thing it stands in for is worse than
  none: it reports green for a smaller set of checks and nobody reads the
  job name closely enough to notice;
- no workflow calls `make` any more.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
JUSTFILE = ROOT / "justfile"
MAKEFILE = ROOT / "Makefile"
CI = ROOT / ".github/workflows/ci.yml"
FALLBACK = ROOT / ".github/workflows/fallback-gates.yml"

# Commands that constitute the canonical check chain, extracted from the
# justfile's own recipes. Ordered: fmt, clippy, test, then contract layers.
CHAIN = [
    "cargo fmt --all",
    "cargo clippy --locked --workspace --all-targets --all-features",
    "cargo test --locked --workspace --all-targets --all-features",
    "scripts/validate-contract-assets.ts",
    "unittest discover -s scripts -p 'test_*.py'",
    "audit-public-tree.sh",
]


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class TaskRunnerContract(unittest.TestCase):
    def test_the_makefile_is_gone(self) -> None:
        self.assertFalse(MAKEFILE.exists(), "just replaced it; do not resurrect both")

    def test_justfile_exposes_the_canonical_recipes(self) -> None:
        just = read(JUSTFILE)
        for recipe in ("check", "ci", "fmt", "lint", "test", "test-postgres", "db-up"):
            self.assertRegex(just, rf"(?m)^{recipe}(:|\s.*:)", recipe)

    def test_ci_inline_block_matches_the_check_chain(self) -> None:
        ci = read(CI)
        block = ci.split("Run repository checks", 1)[1].split("  summary:", 1)[0]
        for command in CHAIN:
            self.assertIn(command, block, command)
        self.assertNotIn("make ", block)

    def test_the_fallback_workflow_runs_the_same_chain(self) -> None:
        fallback = read(FALLBACK)
        block = fallback.split("Run repository checks", 1)[1]
        for command in CHAIN:
            self.assertIn(command, block, command)

    def test_the_fallback_workflow_needs_no_self_hosted_runner(self) -> None:
        # The whole point. A fallback that also waits for the offline machine
        # is not a fallback, and the failure mode is silent: the job simply
        # queues for ever beside the one it was meant to cover.
        targets = [
            line.strip()
            for line in read(FALLBACK).splitlines()
            if line.strip().startswith("runs-on:")
        ]
        # The prose at the top of that file says "self-hosted" several times,
        # which is why this reads the runs-on lines rather than the whole text:
        # a gate that matches a comment is a gate that fails on an explanation.
        self.assertTrue(targets, "the fallback workflow declares no runner at all")
        for target in targets:
            self.assertNotIn("self-hosted", target, target)
            self.assertIn("ubuntu-latest", target, target)

    def test_no_workflow_shells_out_to_make(self) -> None:
        workflows = ROOT / ".github/workflows"
        for path in workflows.glob("*.yml"):
            for line in read(path).splitlines():
                if re.search(r"(?m)^\s*run:.*\bmake\b", line) or "\n      make " in line:
                    self.fail(f"{path.name} still calls make: {line.strip()}")

    def test_the_summary_job_gives_the_panel_one_node(self) -> None:
        ci = read(CI)
        # rust-checks and deploy-config were folded into rust-tests on
        # 2026-09-15, and rust-postgres followed on 2026-09-21: on a single
        # self-hosted runner every extra job buys another checkout, toolchain
        # setup and full workspace compile — zero parallelism for pure cost.
        self.assertIn(
            "needs: [rust-tests, dependency-security, containers]",
            ci,
        )
        self.assertIn("All checks passed", ci)


if __name__ == "__main__":
    unittest.main()
