#!/usr/bin/env python3
"""The reconciliation sweeps tell the resolver a state their SQL must guarantee.

`receipt_reconciliation.rs` has four sweeps. Each selects actions with
`a.status = 'unknown'` and then calls

    legal_transition(ActionState::Unknown, resolve_observation(evidence), ..)

The `ActionState::Unknown` is a Rust constant. Nothing connects it to the SQL
predicate that makes it true. Widen one of those queries — to pick up
`'processing'`, say, or drop the predicate while refactoring — and the resolver
keeps being told `Unknown` about rows that are not, which is how a *provider-
confirmed* success gets a `Failed` observation resolved against it:
`legal_transition(Unknown, Failed)` is `Apply(Failed)`, where
`legal_transition(Succeeded, Failed)` with provider confirmation is `Conflict`.

Today the damage is bounded because `resolve_action`'s UPDATE carries its own
`AND status = 'unknown'`, so a mis-scoped sweep would decide wrongly and then
write nothing. That is the same accident the execution-report path had before
`test_action_state_parity_v1.py`: monotonicity enforced in SQL while the
resolver believed it was enforcing it. Bounded by accident is not the same as
correct, and the accident is one refactor from ending.

So: every sweep that hands the resolver a hardcoded current state must be
scoped by SQL that guarantees it, and the write must stay pinned too.

`SuccessEvidence` is also asserted here. Every production call site passes
`Premature`, and from `Unknown` the resolver never consults it — the argument is
inert. It stops being inert the moment a sweep is pointed at a non-`Unknown`
state, and `Premature` is exactly the value that disables the `Conflict`
protection. The test that would catch that is this one.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKER = ROOT / "crates/crowdrelay-worker/src/receipt_reconciliation.rs"


def production_source() -> str:
    """The module without its `#[cfg(test)]` tail.

    The unit tests below it exercise the resolver from several states on
    purpose, which is right there and would defeat every assertion here.
    """
    text = WORKER.read_text()
    marker = text.find("#[cfg(test)]")
    return text[:marker] if marker != -1 else text


class ReceiptReconciliationScope(unittest.TestCase):
    def test_every_sweep_hands_the_resolver_the_state_its_sql_guarantees(self) -> None:
        source = production_source()
        states = re.findall(
            r"legal_transition\(\s*ActionState::(\w+)\s*,", source
        )
        self.assertEqual(
            len(states),
            4,
            f"expected the four reconciliation sweeps, found {len(states)} "
            f"resolver call sites: {states}. A new one needs its own scope "
            f"guarantee recorded here",
        )
        self.assertEqual(
            set(states),
            {"Unknown"},
            f"a sweep is telling the resolver a state other than Unknown: "
            f"{states}. Only Unknown is scoped by SQL below; any other state "
            f"has to justify how it is guaranteed",
        )
        scoped = source.count("AND a.status = 'unknown'")
        self.assertEqual(
            scoped,
            len(states),
            f"{len(states)} sweeps tell the resolver the action is Unknown but "
            f"only {scoped} queries guarantee it. The resolver would be "
            f"deciding about rows in a state it was not told about",
        )

    def test_the_write_stays_pinned_to_the_state_it_resolved_from(self) -> None:
        source = production_source()
        body = source[source.index("async fn resolve_action(") :]
        update = body[body.index("UPDATE viryaos_autopilot_actions") :]
        update = update[: update.index('"#')]
        self.assertIn(
            "AND status = 'unknown'",
            update,
            "resolve_action's UPDATE is no longer pinned to the state the "
            "sweep resolved from, so a mis-scoped sweep would land its write "
            "instead of writing nothing",
        )

    def test_success_evidence_is_inert_and_stays_that_way(self) -> None:
        """`Premature` from `Unknown` is a placeholder, not a claim.

        If a sweep is ever pointed at `Succeeded`, `Premature` is precisely the
        value that turns a contradictory failure observation from `Conflict`
        into `Apply(Failed)` — a provider-confirmed success silently overturned
        by a sweep. The first assertion above is what stops that; this one
        records why the argument cannot simply be left as it is.
        """
        source = production_source()
        evidence = re.findall(r"SuccessEvidence::(\w+)", source)
        self.assertEqual(
            set(evidence),
            {"Premature"},
            f"the sweeps pass mixed SuccessEvidence: {evidence}. From Unknown "
            f"the resolver never reads it, so a difference here means a sweep "
            f"is resolving from some other state",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print("RECEIPT_RECONCILIATION_SCOPE=PASS sweeps=4")
    else:
        print("RECEIPT_RECONCILIATION_SCOPE=FAIL")
        sys.exit(1)
