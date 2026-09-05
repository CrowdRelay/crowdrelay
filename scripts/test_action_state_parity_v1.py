#!/usr/bin/env python3
"""Rust and SQL must read `viryaos_autopilot_actions.status` the same way.

Two layers decide what an action's operational state is, in two languages:

- `ActionState::from_action_status` in `crowdrelay-domain/src/action_ledger.rs`,
  which the execution-report resolver uses.
- The `CASE NEW.status` in the `viryaos_action_ledger_sync` trigger
  (migration 0190), which maintains the ledger projection.

They read the same column and must agree. Nothing made them, and the cost of
that was not theoretical: the resolver previously called `ActionState::parse`,
which reads the *ledger's* uppercase vocabulary (`SUCCEEDED`) rather than the
action table's lowercase one (`succeeded`). It returned `None` for every legal
status and the caller defaulted to `Running`, so the resolver decided every
receipt from a constant. `legal_transition(Succeeded, ...)` was unreachable
from that path, its `Conflict` branch could never fire, and what actually kept
a confirmed success from regressing was an `AND status = 'unknown'` predicate
on the UPDATE — monotonicity enforced by accident, in SQL, while the resolver
believed it was enforcing it.

This also pins the one cross-layer transition the success invariant turns on:
the trigger has to permit `SUCCEEDED -> FAILED`, because the resolver can now
apply it when a persisted success is premature. If the trigger stopped allowing
it, correcting a premature success would raise instead of transition.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOMAIN = ROOT / "crates/crowdrelay-domain/src/action_ledger.rs"
TRIGGER = ROOT / "migrations/0190_action_ledger_trigger_unknown.sql"
STATUS_CHECK = ROOT / "migrations/0189_autopilot_action_unknown_status.sql"


def check_vocabulary() -> set[str]:
    """The statuses the `viryaos_autopilot_actions` status CHECK admits.

    This is the column's real alphabet. Rust and the trigger agreeing with
    each other proves only that they drifted together; both can be missing a
    status the database happily stores.
    """
    source = STATUS_CHECK.read_text()
    start = source.index("viryaos_autopilot_actions_status_check\n    CHECK (status IN (")
    body = source[start : source.index("));", start)]
    values = set(re.findall(r"'([a-z_]+)'", body))
    if not values:
        raise AssertionError("the status CHECK has no values; the parser is wrong")
    return values


def rust_mapping() -> dict[str, str]:
    """`from_action_status` as {action status: LEDGER_STATE}."""
    source = DOMAIN.read_text()
    start = source.index("pub fn from_action_status")
    body = source[start : source.index("\n    }", start)]
    pairs = re.findall(r'"([a-z_]+)"\s*=>\s*Some\(Self::(\w+)\)', body)
    if not pairs:
        raise AssertionError("from_action_status has no arms; the parser is wrong")
    return {status: variant.upper() for status, variant in pairs}


def sql_mapping() -> dict[str, str]:
    """The trigger's `CASE NEW.status` as {action status: LEDGER_STATE}."""
    source = TRIGGER.read_text()
    start = source.index("CASE NEW.status")
    body = source[start : source.index("END", start)]
    pairs = re.findall(r"WHEN\s+'([a-z_]+)'\s+THEN\s+'([A-Z]+)'", body)
    if not pairs:
        raise AssertionError("trigger CASE has no arms; the parser is wrong")
    return dict(pairs)


def sql_allowed_transitions() -> set[tuple[str, str]]:
    """Edges the trigger permits, from its `current_ledger_state` guards."""
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


class ActionStateParity(unittest.TestCase):
    def test_both_mappings_are_readable(self) -> None:
        self.assertTrue(rust_mapping())
        self.assertTrue(sql_mapping())

    def test_rust_and_sql_agree_on_every_status(self) -> None:
        rust, sql = rust_mapping(), sql_mapping()
        self.assertEqual(
            rust,
            sql,
            "the resolver and the ledger trigger disagree about what an "
            "action status means. Reading the same column two ways is how the "
            "resolver ended up deciding from a constant",
        )

    def test_the_mapping_covers_the_whole_status_check(self) -> None:
        """Every status the column can hold must map to a ledger state.

        Rust and the trigger matching each other is agreement, not coverage:
        a status added to the CHECK and to neither mapping leaves both sides
        consistently blind. `from_action_status` returning `None` is the
        resolver's signal that it cannot read the row, and the execution-report
        path answers that by auditing the receipt and refusing to transition.
        Correct, and a live action stuck unresolvable forever — so the drift is
        caught here instead.
        """
        self.assertEqual(
            check_vocabulary() - set(rust_mapping()),
            set(),
            "the status CHECK admits values `from_action_status` does not map. "
            "The resolver cannot read those rows and will audit their receipts "
            "without ever resolving them",
        )
        self.assertEqual(
            set(rust_mapping()) - check_vocabulary(),
            set(),
            "`from_action_status` maps statuses the column cannot hold; either "
            "the CHECK was narrowed or the mapping is describing a fiction",
        )

    def test_the_resolver_never_falls_back_to_a_state_it_did_not_read(self) -> None:
        """An unmappable status is not a state; it is the absence of one.

        `unwrap_or(ActionState::Running)` read an unknown status as
        mid-execution. `Running` resolves to `Apply(Succeeded)` on a success
        receipt and `Apply(Failed)` on a failure one, and the UPDATE is pinned
        to the very status string the resolver could not read — so it lands.
        Vocabulary drift would have silently rewritten action rows, and on the
        success arm committed the whole set of success side effects, rather
        than raising.
        """
        runtime = (ROOT / "crates/crowdrelay-infra/src/autopilot/runtime.rs").read_text()
        self.assertNotIn(
            "unwrap_or(ActionState::",
            runtime,
            "the execution-report resolver is defaulting an unreadable action "
            "status to a concrete state and deciding from a constant again",
        )

    def test_the_resolver_does_not_reach_for_the_ledger_vocabulary(self) -> None:
        """`parse` reads uppercase ledger states and silently returns None here."""
        runtime = (ROOT / "crates/crowdrelay-infra/src/autopilot/runtime.rs").read_text()
        self.assertNotIn(
            "and_then(ActionState::parse)",
            runtime,
            "an action status is being fed to ActionState::parse, which reads "
            "the ledger's uppercase vocabulary and returns None for every "
            "legal action status. Use ActionState::from_action_status",
        )

    def test_no_success_side_effect_precedes_the_resolver(self) -> None:
        """No execution-derived side effect may be committed for a Conflict.

        The success side effects — outcome evidence, effect measurement,
        show-growth surfaces, the team-opportunity completion edge — used to run
        *before* `legal_transition` was consulted. A receipt the resolver then
        refused had already written success evidence and flipped the opportunity
        to `submitted` while the action stayed `failed`: an internally
        contradictory history committed in one transaction.

        They now live behind `apply_success_side_effects`, whose single call
        site is the resolver's non-Conflict arm. Pin the ordering, because the
        gate is only a gate while the call stays after the decision.
        """
        runtime = (ROOT / "crates/crowdrelay-infra/src/autopilot/runtime.rs").read_text()
        call = "apply_success_side_effects(&mut transaction"
        self.assertEqual(
            runtime.count(call),
            1,
            "success side effects must have exactly one call site; the resolver "
            "is only a gate while every path to them goes through it",
        )
        start = runtime.index("ExecutorReportStatus::Succeeded =>")
        arm = runtime[start : runtime.index("ExecutorReportStatus::Accepted", start)]
        self.assertTrue(
            call in arm,
            "the success side effects are no longer applied from the success "
            "receipt's own arm, so the resolver there gates nothing",
        )
        self.assertLess(
            arm.index("let transition = legal_transition("),
            arm.index(call),
            "success side effects are applied before the resolver decides, so a "
            "Conflict receipt can still commit accepted-execution evidence",
        )

    def test_the_trigger_permits_correcting_a_premature_success(self) -> None:
        self.assertIn(
            ("SUCCEEDED", "FAILED"),
            sql_allowed_transitions(),
            "the resolver applies SUCCEEDED -> FAILED when a persisted success "
            "is premature; if the trigger no longer permits that edge, the "
            "correction raises a check_violation instead of transitioning",
        )

    def test_success_evidence_decides_the_contradiction(self) -> None:
        """The invariant must stay expressed, not drift back to a bare arm."""
        domain = DOMAIN.read_text()
        self.assertRegex(
            domain,
            r"\(ActionState::Succeeded, ObservedResolution::Failed\) => match success_evidence",
            "a failure observation against Succeeded no longer consults "
            "SuccessEvidence, so premature and provider-confirmed successes "
            "are being treated alike again",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"ACTION_STATE_PARITY=PASS statuses={len(rust_mapping())}")
    else:
        print("ACTION_STATE_PARITY=FAIL")
        sys.exit(1)
