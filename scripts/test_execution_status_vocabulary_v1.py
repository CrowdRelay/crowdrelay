#!/usr/bin/env python3
"""`ExecutionStatus` and the column's CHECK must know the same words.

`viryaos_experiment_assignments.execution_status` is the causal layer's answer
to "was the treatment realized". Two things read it, in two languages: the
`ExecutionStatus` enum in `crowdrelay-brain/src/experiment.rs`, and the CHECK
constraint that decides what the column may hold. Nothing made them agree.

The cost of drift is not a parse error. The evidence loader deserializes the
stored string into the enum, and a value the enum does not know produces a
failure that used to become `None` — which the causal layer reads as "legacy
row, no assignment" and defaults, for a treatment row, to `Executed`. So a
status added to the CHECK and not to the enum would have entered the learner as
a realized treatment. It now maps to `Unknown` and logs, which is the honest
answer; this gate is the one that stops the situation arising.

Inert under intent-to-treat, which includes every assigned unit regardless of
execution. Not inert under per-protocol, where the treatment arm must have
executed — and the estimand is one line, guarded by its own contract test,
which is to say the switch is intended rather than hypothetical.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXPERIMENT = ROOT / "crates/crowdrelay-brain/src/experiment.rs"
MIGRATIONS = ROOT / "migrations"


def rust_variants() -> set[str]:
    """`ExecutionStatus` variants as their serde snake_case wire values."""
    source = EXPERIMENT.read_text()
    start = source.index("pub enum ExecutionStatus {")
    body = source[start : source.index("\n}", start)]
    variants = re.findall(r"^\s{4}([A-Z][A-Za-z]*),", body, re.MULTILINE)
    if not variants:
        raise AssertionError("ExecutionStatus has no variants; the parser is wrong")
    # `#[serde(rename_all = "snake_case")]` — every variant here is a single
    # capitalised word, so lowercasing is the whole transform. Assert that,
    # rather than assuming it, so a multi-word variant fails loudly here
    # instead of silently mismatching.
    for variant in variants:
        if not variant[1:].islower():
            raise AssertionError(
                f"{variant} is multi-word; snake_case is no longer a lowercase, "
                f"and this parser has to learn the real transform"
            )
    return {variant.lower() for variant in variants}


def check_vocabulary() -> set[str]:
    """The latest `execution_status` CHECK, from the newest migration that sets one."""
    latest: tuple[str, set[str]] | None = None
    pattern = re.compile(
        r"CHECK\s*\(execution_status IN \(([^)]*)\)\)", re.IGNORECASE | re.DOTALL
    )
    for path in sorted(MIGRATIONS.glob("*.sql")):
        for match in pattern.finditer(path.read_text()):
            values = set(re.findall(r"'([a-z_]+)'", match.group(1)))
            if values:
                latest = (path.name, values)
    if latest is None:
        raise AssertionError("no execution_status CHECK found; the parser is wrong")
    return latest[1]


class ExecutionStatusVocabulary(unittest.TestCase):
    def test_both_sides_are_readable(self) -> None:
        self.assertTrue(rust_variants())
        self.assertTrue(check_vocabulary())

    def test_the_enum_covers_every_value_the_column_may_hold(self) -> None:
        missing = check_vocabulary() - rust_variants()
        self.assertEqual(
            missing,
            set(),
            f"the execution_status CHECK admits {sorted(missing)}, which "
            f"ExecutionStatus cannot parse. The evidence loader maps an "
            f"unparseable status to Unknown and logs — correct, and a value "
            f"the causal layer can never reason about",
        )

    def test_the_enum_claims_no_value_the_column_forbids(self) -> None:
        extra = rust_variants() - check_vocabulary()
        self.assertEqual(
            extra,
            set(),
            f"ExecutionStatus has {sorted(extra)}, which the column cannot "
            f"hold. Either the CHECK was narrowed — which `ADD CONSTRAINT` "
            f"would have aborted on wherever such a row exists — or the enum "
            f"is describing a state nothing can persist",
        )

    def test_the_loader_does_not_read_a_parse_failure_as_a_legacy_row(self) -> None:
        loader = (
            ROOT / "crates/crowdrelay-infra/src/autopilot/operations/evidence.rs"
        ).read_text()
        start = loader.index("let execution_status = row.execution_status")
        # To the next binding at the same indent — the closure body contains
        # its own statements, so the first `;` is nowhere near the end.
        binding = loader[start : loader.index("\n            let ", start)]
        self.assertNotIn(
            ".ok()",
            binding,
            "a failed execution_status parse is becoming None again. None "
            "means 'no assignment', which the causal layer defaults to "
            "Executed for a treatment row — so an execution nobody could read "
            "would count as a realized treatment",
        )
        self.assertIn(
            "ExecutionStatus::Unknown",
            binding,
            "an unparseable execution status must map to Unknown, which is "
            "what 'we cannot establish whether the treatment was realized' "
            "already means",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"EXECUTION_STATUS_VOCABULARY=PASS statuses={len(rust_variants())}")
    else:
        print("EXECUTION_STATUS_VOCABULARY=FAIL")
        sys.exit(1)
