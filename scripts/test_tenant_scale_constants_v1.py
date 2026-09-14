#!/usr/bin/env python3
"""A decision threshold may not be written in absolute outcome units.

Some numbers in the brain are properties of the *method* — a decay factor, a
prior variance, a reliability-bucket count, a number of days. Those are correctly
constants and this gate ignores them.

Others are properties of the *tenant*: how many outcomes an action is expected to
yield, how large an effect is worth acting on, how many app installs a dispatch
converts. Written as a constant and read where the brain decides, each is correct
for exactly one tenant size and silently wrong for every other.
`MEANINGFUL_EFFECT_THRESHOLD = 1.0` is a 4.5% lift for a workspace with 22 fans
and noise for one with ten thousand — and the ranking path read it directly, so no
tenant could change it without editing the crate.

The rule is scoped to the *enclosing function*, not to the shape of the line. A
tenant-scale constant may be read where a model is being built — a constructor, a
`Default` impl, a serde default, a prior — because that is the definition of a
default, and a default somebody can override is the fix. It may not be read
anywhere else, because everywhere else is code that ranks, scores or decides, and
it should be reading the model's own field instead.

Line-shaped rules were tried first and rejected: they flagged `impl Default for
NormalPosterior` and the `pub use` re-export alongside the genuine defects, and a
gate whose output is mostly false positives gets an allowlist bolted on and then
gets ignored.

Curated rather than pattern-matched. A gate that flagged every `const … : f64`
would report forty-four constants, forty-one of which are method parameters.
Adding a scale-bound constant means adding it here, which is the review step the
rule exists to force.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BRAIN = ROOT / "crates/crowdrelay-brain/src"

# Constants expressed in a tenant's own outcome units. Each has a field on
# `CausalModel` set through `with_tenant_scale_and_signal`.
TENANT_SCALE_CONSTANTS = {
    "DEFAULT_EXPECTED_FANS": "outcomes expected per dispatch",
    "MEANINGFUL_EFFECT_THRESHOLD": "effect size worth acting on",
    "DEFAULT_EXPECTED_SIGNAL": "app installs expected per dispatch",
}

# Functions whose whole job is to build a model. A default belongs here.
CONSTRUCTORS = re.compile(
    r"^\s*(?:pub(?:\(crate\))? )?(?:const )?fn "
    r"(default|new|prior|with_\w+|default_\w+)\s*[(<]"
)

# Any function header, so the scanner knows which one it is inside.
ANY_FN = re.compile(r"^\s*(?:pub(?:\(crate\))? )?(?:const |async )*fn (\w+)")


def brain_sources() -> list[Path]:
    return [
        path
        for path in sorted(BRAIN.rglob("*.rs"))
        if "tests" not in path.name and "tests" not in path.parts
    ]


def offending_reads(constant: str) -> list[str]:
    """Reads of `constant` from a function that is not building a model.

    Module-level lines — the `const` itself, a `pub use` — sit inside no
    function and are not reads by decision logic, so they pass.
    """
    found: list[str] = []
    for path in brain_sources():
        enclosing = None
        for number, line in enumerate(path.read_text().splitlines(), 1):
            # Any unindented line is a top-level item, so whatever function was
            # open has closed. `cargo fmt` guarantees this: nothing inside a
            # function body starts at column zero. Cheaper and less brittle than
            # counting braces through string literals, and it is what makes a
            # module-level `const` or `pub use` read as module-level.
            if line and not line[0].isspace():
                enclosing = None
            match = ANY_FN.match(line)
            if match:
                # None while inside a constructor, the name while inside
                # anything else. Stays None at module level.
                enclosing = None if CONSTRUCTORS.match(line) else match.group(1)
            stripped = line.strip()
            # Doc comments discuss these by name constantly; they are prose.
            if stripped.startswith("//"):
                continue
            if not re.search(rf"\b{constant}\b", line):
                continue
            if enclosing is None:
                continue
            found.append(
                f"{path.relative_to(ROOT).as_posix()}:{number}  "
                f"in fn {enclosing}()  {stripped[:64]}"
            )
    return found


class TenantScaleConstants(unittest.TestCase):
    def test_each_listed_constant_still_exists(self):
        """If one is renamed or removed, the rest of this file guards nothing."""
        source = "\n".join(path.read_text() for path in brain_sources())
        for constant in TENANT_SCALE_CONSTANTS:
            self.assertRegex(
                source,
                rf"const {constant}: f64 = ",
                f"{constant} is no longer defined; re-read this gate before "
                "assuming the rule still holds",
            )

    def test_no_tenant_scale_constant_is_read_outside_a_constructor(self):
        offenders: list[str] = []
        for constant, meaning in TENANT_SCALE_CONSTANTS.items():
            offenders += [f"[{meaning}] {site}" for site in offending_reads(constant)]
        self.assertEqual(
            offenders,
            [],
            "these read a tenant-scale constant from code that decides, instead "
            "of reading the model's own field, so no tenant can change them "
            "without editing the crate:\n  " + "\n  ".join(offenders),
        )

    def test_the_model_exposes_each_as_a_field(self):
        """The rule is only satisfiable if there is something to read instead."""
        model = (BRAIN / "causal_model.rs").read_text()
        for field in ("meaningful_effect_threshold", "expected_signal_per_dispatch"):
            self.assertRegex(
                model,
                rf"    {field}: f64,",
                f"{field} must be a field so the deciding path can read it",
            )
            self.assertRegex(
                model,
                rf"#\[serde\(default = \"default_\w+\"\)\]\n    {field}",
                f"{field} must carry a serde default, or a checkpoint written "
                "before it existed deserialises to zero",
            )

    def test_a_constructor_sets_every_tenant_scale_number(self):
        model = (BRAIN / "causal_model.rs").read_text()
        self.assertIn(
            "pub fn with_tenant_scale_and_signal(",
            model,
            "one constructor should set every tenant-scale number, so a caller "
            "configuring a tenant has one place to look",
        )


if __name__ == "__main__":
    unittest.main()
