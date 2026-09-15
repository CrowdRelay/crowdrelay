#!/usr/bin/env python3
"""Every emitted event kind resolves to a real capability, never "unknown".

`emit_external_action` routes each emission through
`ensure_executor_capability`, which fails CLOSED on any workspace with a
registered executor instance: an event kind with no arm in
`executor_capability_for_event` resolves to `"unknown"`, the check returns
`Unavailable`, and the calling arm rolls back on every retry, forever.

This is not hypothetical. The R+3 release report shipped exactly this wedge
in review on 2026-09-15 (fixed in a87531b): `r3_report_due` had no arm, and on
any executor-registered workspace the whole sustain milestone — campaign,
report, milestone mark — would have rolled back on every retry. The same
defect already existed on HEAD for `editorial_pitch_parked` and
`editorial_pitch_escalated`. The pg tests cannot see it either way: a fixture
without an executor-instance row takes the fail-open path, which is why the
test now seeds one — and why the mapping itself needs a source gate rather
than a runtime one.

`test_executor_capability_parity_v1.py` deliberately cannot catch this: it
compares capability *names* against the executor contract and discards
`"unknown"`, so an unmapped event kind passes it cleanly. This gate works the
other side of the lookup — every event kind a call site can emit must appear
as a match arm, so nothing a caller can say resolves to `"unknown"`.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
INFRA = ROOT / "crates/crowdrelay-infra/src"
CAPABILITIES = INFRA / "autopilot/execution_capabilities.rs"

# An event literal a call site hands to emit_external_action: the function's
# fourth argument is the event_type, always written as a string literal.
EVENT_LITERAL = re.compile(r'"(crowdrelay\.[a-z0-9_]+(?:\.[a-z0-9_]+)+)"')
# A match arm's left side inside executor_capability_for_event.
MATCH_ARM = re.compile(r'"(crowdrelay\.[a-z0-9_]+(?:\.[a-z0-9_]+)+)"\s*=>')


def emitted_event_kinds() -> set[str]:
    """Event literals that appear inside an emit_external_action call window."""
    found: set[str] = set()
    for source in INFRA.rglob("*.rs"):
        text = source.read_text()
        for match in re.finditer(r"emit_external_action\(", text):
            # The call's arguments span a few lines; the event literal is the
            # only "crowdrelay.*" string in the argument window (payload keys
            # and values do not use the crowdrelay.* namespace).
            window = text[match.start() : match.start() + 2000]
            found.update(EVENT_LITERAL.findall(window))
    return found


def mapped_event_kinds() -> set[str]:
    return set(MATCH_ARM.findall(CAPABILITIES.read_text()))


class ExecutorEventMapping(unittest.TestCase):
    def test_call_sites_actually_emit(self) -> None:
        # The gate's own guard against matching nothing: if the call-site
        # inventory ever comes back empty the real check below vacuously
        # passes, which is how "passing" gates rot (P28).
        self.assertGreaterEqual(
            len(emitted_event_kinds()),
            20,
            "expected at least 20 emit_external_action event kinds; "
            "the parse found fewer, so the real check is matching nothing",
        )

    def test_every_emitted_kind_has_a_capability_arm(self) -> None:
        unmapped = emitted_event_kinds() - mapped_event_kinds()
        self.assertEqual(
            unmapped,
            set(),
            f"emitted event kinds with no executor_capability_for_event arm: "
            f"{sorted(unmapped)}. Each resolves to \"unknown\" and fails closed "
            f"on every executor-registered workspace, rolling back the calling "
            f"arm on every retry. Add a match arm — report-class events ride "
            f"show.escalation like post_show_report_due.",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
