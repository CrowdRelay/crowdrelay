#!/usr/bin/env python3
"""A literal status guard in SQL is a state transition. Check it against one.

`UPDATE <table> SET <column> = 'X' ... WHERE ... <column> IN ('Y', 'Z')` is a
state machine written by hand, in SQL, with no compiler between it and the
declared one. Two columns in the autopilot execution path are written this way
from five call sites, and the guards had drifted from what the code can
actually produce.

Two surfaces, with very different backstops:

- **`viryaos_autopilot_actions.status`** is projected into the action ledger by
  the `viryaos_action_ledger_sync` trigger (migration 0190), which raises
  `check_violation` on an edge it does not recognise — aborting whichever
  transaction the write was part of, not just the write. The outbox
  ambiguous-delivery path guarded on
  `('succeeded', 'processing', 'queued', 'running')` before writing
  `'unknown'`: `QUEUED -> UNKNOWN` is not a legal edge, so in
  `mark_exhausted_delivery_leases_dead` one `queued` action rolled back the
  `webhook_deliveries` dead-lettering above it and the same rows were
  re-selected forever. `'running'` was worse in a quieter way — not a value the
  status CHECK allows at all, so the arm matched nothing while reading as
  though it did.

- **`viryaos_experiment_assignments.execution_status`** has no trigger and no
  runtime enforcement whatsoever. `ExecutionStatus::can_transition_to` in
  `crowdrelay-brain/src/experiment.rs` is documented as its state machine and
  has no production caller — only its own unit tests. Every write is a
  hand-typed literal guard, and the three sites disagree: `runtime.rs` admits
  `('dispatched', 'unknown')`, `community_executor.rs` admits `'dispatched'`,
  `AssignmentTransition::requires_from` admits `'unknown'`. Each is currently
  correct for its own caller's reachable states, but nothing says so. This
  script is what makes `can_transition_to` decide anything.

The mirror failure — a guard *narrower* than the states the caller can reach —
is invisible from SQL alone, because the write is skipped rather than raising.
Guards bound to a parameter (`AND status = $4`) are the pattern that closes it:
the state read under `FOR UPDATE` carried into the write, so the decision lives
in one place. Those are skipped here; there is no literal to check.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATES = ROOT / "crates"
TRIGGER = ROOT / "migrations/0190_action_ledger_trigger_unknown.sql"
ACTION_CHECK = ROOT / "migrations/0189_autopilot_action_unknown_status.sql"
ASSIGNMENT_CHECK = ROOT / "migrations/0191_execution_status_unknown.sql"
TARGET_KIND_CHECK = ROOT / "migrations/0138_agent_outreach_targets_community.sql"
EXPERIMENT = CRATES / "crowdrelay-brain/src/experiment.rs"
AGENT_OUTCOMES = CRATES / "crowdrelay-worker/src/agent_outcomes.rs"

# Only the crates that own writes. Integration tests drive the schema directly
# and are allowed to set up states no production path produces.
SOURCE_ROOTS = (
    CRATES / "crowdrelay-infra/src",
    CRATES / "crowdrelay-worker/src",
    CRATES / "crowdrelay-api/src",
)


def check_vocabulary(path: Path, constraint: str) -> set[str]:
    """The values a `CHECK (<column> IN (...))` constraint allows."""
    source = path.read_text()
    start = source.index(constraint)
    body = source[start : source.index("))", start)]
    values = set(re.findall(r"'([a-z_]+)'", body))
    if not values:
        raise AssertionError(f"{constraint} has no values; the parser is wrong")
    return values


def action_ledger_states() -> dict[str, str]:
    """The trigger's `CASE NEW.status` as {action status: LEDGER_STATE}."""
    source = TRIGGER.read_text()
    start = source.index("CASE NEW.status")
    body = source[start : source.index("END", start)]
    pairs = re.findall(r"WHEN\s+'([a-z_]+)'\s+THEN\s+'([A-Z]+)'", body)
    if not pairs:
        raise AssertionError("trigger CASE has no arms; the parser is wrong")
    return dict(pairs)


def action_ledger_edges() -> set[tuple[str, str]]:
    """Ledger-state edges the trigger permits, from its transition guards."""
    source = TRIGGER.read_text()
    edges: set[tuple[str, str]] = set()
    for current, targets in re.findall(
        r"current_ledger_state = '([A-Z]+)' AND new_ledger_state IN \(([^)]+)\)",
        source,
    ):
        for target in re.findall(r"'([A-Z]+)'", targets):
            edges.add((current, target))
    if not edges:
        raise AssertionError("no transition guards found; the parser is wrong")
    return edges


def _rust_fn_body(source: str, signature: str) -> str:
    start = source.index(signature)
    return source[start : source.index("\n    }", start)]


def execution_status_edges() -> set[tuple[str, str]]:
    """`ExecutionStatus::can_transition_to` as DB-string edges.

    Reading it through `as_str` rather than lowercasing the variant keeps the
    two in step: a variant whose stored spelling stops matching its name would
    otherwise pass here and fail in Postgres.
    """
    # `experiment.rs` holds several enums with an `as_str`; take the one
    # inside `impl ExecutionStatus` or the spellings come from a neighbour.
    whole = EXPERIMENT.read_text()
    source = whole[whole.index("impl ExecutionStatus {") :]
    stored = dict(
        re.findall(
            r"Self::(\w+) => \"([a-z_]+)\"",
            _rust_fn_body(source, "pub const fn as_str(self) -> &'static str"),
        )
    )
    body = _rust_fn_body(source, "pub const fn can_transition_to(self, new: Self) -> bool")
    edges = {
        (stored[current], stored[new])
        for current, new in re.findall(r"\(Self::(\w+), Self::(\w+)\)", body)
        if current in stored and new in stored
    }
    if not edges:
        raise AssertionError("can_transition_to has no edges; the parser is wrong")
    return edges


# The SQL string literal holding an UPDATE, from the verb to the end of the
# Rust literal. Both raw (`r#"..."#`) and escaped (`"... \`) forms occur.
def statement_pattern(table: str) -> re.Pattern[str]:
    return re.compile(
        rf"UPDATE {table}\b.*?(?=\"#|\",\s*\n|\"\s*\n\s*\))",
        re.DOTALL,
    )


class Surface:
    """One column, its vocabulary, and the state machine that governs it."""

    def __init__(
        self,
        table: str,
        column: str,
        vocabulary: set[str],
        edges: set[tuple[str, str]],
        state_of: dict[str, str] | None = None,
    ) -> None:
        self.table = table
        self.column = column
        self.vocabulary = vocabulary
        self.edges = edges
        # Action statuses are projected onto ledger states before the edge
        # check; assignment statuses are their own states.
        self.state_of = state_of or {value: value for value in vocabulary}
        self.statement = statement_pattern(table)
        self.set_column = re.compile(rf"SET\s+{column}\s*=\s*'([a-z_]+)'")
        self.guard_eq = re.compile(rf"{column}\s*(?:=|<>|!=)\s*'([a-z_]+)'")
        self.guard_in = re.compile(rf"{column}\s+(?:NOT\s+)?IN\s*\(([^)]*)\)")

    def statements(self) -> list[tuple[Path, str]]:
        found: list[tuple[Path, str]] = []
        for root in SOURCE_ROOTS:
            for path in sorted(root.rglob("*.rs")):
                text = path.read_text()
                if f"UPDATE {self.table}" not in text:
                    continue
                for match in self.statement.finditer(text):
                    found.append((path.relative_to(ROOT), match.group(0)))
        return found

    def guard_states(self, statement: str, target: str) -> set[str]:
        where = statement[statement.index("WHERE") :] if "WHERE" in statement else ""
        states: set[str] = set()
        for listed in self.guard_in.findall(where):
            states.update(re.findall(r"'([a-z_]+)'", listed))
        states.update(self.guard_eq.findall(where))
        # `SET <column> = 'x'` falls inside the slice when WHERE is absent.
        return states - {target}


def surfaces() -> list[Surface]:
    return [
        Surface(
            "viryaos_autopilot_actions",
            "status",
            check_vocabulary(
                ACTION_CHECK,
                "viryaos_autopilot_actions_status_check\n    CHECK (status IN (",
            ),
            action_ledger_edges(),
            action_ledger_states(),
        ),
        Surface(
            "viryaos_experiment_assignments",
            "execution_status",
            check_vocabulary(
                ASSIGNMENT_CHECK,
                "viryaos_experiment_assignments_execution_status_valid\n"
                "    CHECK (execution_status IN (",
            ),
            execution_status_edges(),
        ),
    ]


class AgentTargetKindVocabulary(unittest.TestCase):
    """The one vocabulary a Rust literal list has to match exactly.

    `agent_outreach_targets.target_kind` is written from the agent's own JSON,
    so `insert_outreach_target` checks the value against `AGENT_TARGET_KINDS`
    before the INSERT rather than letting the CHECK constraint be the
    validator. That list is a hand-typed copy of the constraint, and a copy
    that drifts is worse than none: a value the migration added but the list
    lacks is silently refused, and one the migration removed is accepted and
    then raises `check_violation` inside the outcome's transaction.

    The trap this also pins: the list is deliberately *not*
    `OutreachTargetKind`. That enum belongs to `viryaos_outreach_targets`, and
    the two sets differ in both directions — this one accepts `community` and
    rejects `support_slot`.
    """

    def test_the_rust_list_matches_the_check_constraint(self) -> None:
        source = AGENT_OUTCOMES.read_text()
        start = source.index("const AGENT_TARGET_KINDS")
        listed = set(re.findall(r'"([a-z_]+)"', source[start : source.index("];", start)]))
        self.assertEqual(
            listed,
            check_vocabulary(
                TARGET_KIND_CHECK,
                "agent_outreach_targets_target_kind_check\n"
                "    CHECK (target_kind IN (",
            ),
            "AGENT_TARGET_KINDS no longer matches the "
            "agent_outreach_targets_target_kind_check vocabulary",
        )


class StatusTransitionGuards(unittest.TestCase):
    def test_the_sources_are_readable(self) -> None:
        for surface in surfaces():
            self.assertTrue(surface.vocabulary, surface.column)
            self.assertTrue(surface.edges, surface.column)
            self.assertTrue(
                surface.statements(),
                f"no {surface.table} UPDATEs found; the parser is wrong",
            )

    def test_every_status_literal_is_in_the_check_vocabulary(self) -> None:
        for surface in surfaces():
            for path, statement in surface.statements():
                target = surface.set_column.search(statement)
                literals = surface.guard_states(
                    statement, target.group(1) if target else ""
                )
                if target:
                    literals.add(target.group(1))
                unknown = sorted(literals - surface.vocabulary)
                self.assertFalse(
                    unknown,
                    f"{path}: {unknown} are not values the {surface.table} "
                    f"{surface.column} CHECK allows, so these arms match "
                    f"nothing while reading as though they do",
                )

    def test_every_literal_guard_admits_only_legal_edges(self) -> None:
        for surface in surfaces():
            for path, statement in surface.statements():
                target = surface.set_column.search(statement)
                if not target:
                    continue
                new_state = surface.state_of.get(target.group(1))
                if new_state is None:
                    continue
                for source in sorted(surface.guard_states(statement, target.group(1))):
                    current = surface.state_of.get(source)
                    if current is None or current == new_state:
                        continue
                    self.assertIn(
                        (current, new_state),
                        surface.edges,
                        f"{path}: the guard admits {surface.column} '{source}' "
                        f"({current}) but writes '{target.group(1)}' "
                        f"({new_state}), which the state machine does not "
                        f"permit. On {surface.table} that is not a no-op — "
                        f"every row the guard matches is a transition nothing "
                        f"declared legal",
                    )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        counted = sum(len(surface.statements()) for surface in surfaces())
        print(f"STATUS_TRANSITION_GUARDS=PASS surfaces=2 statements={counted}")
    else:
        print("STATUS_TRANSITION_GUARDS=FAIL")
        sys.exit(1)
