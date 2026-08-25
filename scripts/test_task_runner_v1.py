"""Contract tests for the task runner and CI drift guard.

`just` replaced the Makefile. The justfile is the source of truth for local
gates; CI inlines the same chain because installing a third-party binary
there costs more supply-chain surface than it saves. These tests keep the
two honest:

- the Makefile is gone and stays gone (no silent resurrection);
- the justfile exposes the canonical recipes (`check`, `ci`, `test-postgres`);
- the CI workflow's inline check block runs the same command set as
  `just ci`, so one cannot gain a gate the other lacks;
- no workflow calls `make` any more.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
JUSTFILE = ROOT / "justfile"
MAKEFILE = ROOT / "Makefile"
CI = ROOT / ".github/workflows/ci.yml"

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

    def test_no_workflow_shells_out_to_make(self) -> None:
        workflows = ROOT / ".github/workflows"
        for path in workflows.glob("*.yml"):
            for line in read(path).splitlines():
                if re.search(r"(?m)^\s*run:.*\bmake\b", line) or "\n      make " in line:
                    self.fail(f"{path.name} still calls make: {line.strip()}")

    def test_the_summary_job_gives_the_panel_one_node(self) -> None:
        ci = read(CI)
        self.assertIn(
            "needs: [rust-tests, rust-checks, rust-postgres, deploy-config, dependency-security, containers]",
            ci,
        )
        self.assertIn("All checks passed", ci)


if __name__ == "__main__":
    unittest.main()
