#!/usr/bin/env python3
"""CrowdRelay's HTTP timeouts must stay above the agent service's own budgets.

Every browser-backed Reddit route in crowdrelay-agents carries a total budget
that must expire *before* the caller's HTTP timeout. Otherwise the client's clock
fires first, and a client-side timeout can only ever report `operation timed out`
— it cannot say a login was still running or that the browser had crashed. That
is not hypothetical: it hid an out-of-memory Chromium for days while 41
communities stayed unjoined and 50 drafted posts had nowhere to go.

The budgets live in crowdrelay-agents and the timeouts live here, so the pairing
spans two repositories and neither side could check it. The agents-side test
hard-codes the numbers below, which means lowering one *here* breaks that repo and
fails nothing. This gate closes that direction: the edit happens in CrowdRelay, so
the failure belongs in CrowdRelay's gates.

Skips when the sibling checkout is absent, the same way
`test_rekor_inventory_v12.py` skips an untracked operator file. CI has no
credential for the other repository; the build host that runs `just ci` before
shipping has both, which is where the pairing actually matters.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
AGENTS = ROOT.parent / "crowdrelay-agents"
BUDGETS = AGENTS / "src/agent/reddit-timeouts.ts"

# Every caller of a browser-backed route, and which budget answers it.
# `read` routes are /reddit/metrics and /reddit/observe; the rest are writes.
CALLERS = {
    "JOIN_API_TIMEOUT": ("crates/crowdrelay-worker/src/community_join_executor.rs", "write"),
    "AGENTS_SUBMIT_TIMEOUT": ("crates/crowdrelay-worker/src/community_executor.rs", "write"),
    "AGENTS_SCRAPE_TIMEOUT": ("crates/crowdrelay-worker/src/discovery.rs", "write"),
    "AGENTS_METRICS_TIMEOUT": ("crates/crowdrelay-worker/src/community_executor.rs", "read"),
}

# The agent service needs room to build and send its answer after its own budget
# expires. Below this the deadline buys nothing: the client still gives up first.
MARGIN_MS = 20_000


def rust_timeout_ms(relative_path: str, name: str) -> int:
    source = (ROOT / relative_path).read_text()
    match = re.search(
        rf"const {name}: Duration = Duration::from_secs\((\d+)\);", source
    )
    if match is None:
        raise AssertionError(f"{name} not found in {relative_path}")
    return int(match.group(1)) * 1000


def budget_ms(name: str) -> int:
    source = BUDGETS.read_text()
    match = re.search(
        rf'export const {name} = envDurationMs\(\s*"[A-Z_]+",\s*([\d_]+),', source
    )
    if match is None:
        raise AssertionError(f"{name} not found in {BUDGETS.name}")
    return int(match.group(1).replace("_", ""))


class AgentsTimeoutParity(unittest.TestCase):
    def setUp(self):
        if not BUDGETS.is_file():
            self.skipTest(
                "crowdrelay-agents is not checked out beside this repository; "
                "the pairing is checked on the build host, which has both"
            )

    def test_every_write_caller_outlives_the_operation_budget(self):
        operation = budget_ms("OPERATION_DEADLINE_MS")
        for name, (path, kind) in CALLERS.items():
            if kind != "write":
                continue
            caller = rust_timeout_ms(path, name)
            self.assertGreaterEqual(
                caller - operation,
                MARGIN_MS,
                f"{name} is {caller}ms against a {operation}ms operation budget: "
                f"the agent service cannot answer before this client gives up, so "
                f"the only symptom recordable is 'operation timed out'",
            )

    def test_every_read_caller_outlives_the_read_budget(self):
        read = budget_ms("READ_DEADLINE_MS")
        for name, (path, kind) in CALLERS.items():
            if kind != "read":
                continue
            caller = rust_timeout_ms(path, name)
            self.assertGreaterEqual(
                caller - read,
                MARGIN_MS,
                f"{name} is {caller}ms against a {read}ms read budget",
            )

    def test_community_intelligence_outlives_the_read_budget(self):
        # /reddit/observe, whose browser fallback goes through the same session.
        # Named separately because its constant is `FETCH_TIMEOUT`, a name two
        # other modules also use — matching it loosely would read the wrong one.
        caller = rust_timeout_ms(
            "crates/crowdrelay-worker/src/community_intelligence/reddit.rs",
            "FETCH_TIMEOUT",
        )
        read = budget_ms("READ_DEADLINE_MS")
        self.assertGreaterEqual(caller - read, MARGIN_MS)

    def test_the_agents_side_ladder_is_internally_ordered(self):
        # Cheap to assert from here and it catches the case where somebody widens
        # a budget in the other repository past the caller this file knows about.
        session = budget_ms("SESSION_DEADLINE_MS")
        operation = budget_ms("OPERATION_DEADLINE_MS")
        read = budget_ms("READ_DEADLINE_MS")
        self.assertLess(session, operation)
        self.assertLess(read, operation)

    def test_the_tightest_read_caller_is_the_one_the_agents_test_pins(self):
        # The agents-side test asserts its read budget against a hard-coded 90s.
        # If a tighter read caller appears here, that hard-coded number is stale
        # and the budget it protects is too loose.
        read_callers = [
            rust_timeout_ms(path, name)
            for name, (path, kind) in CALLERS.items()
            if kind == "read"
        ]
        read_callers.append(
            rust_timeout_ms(
                "crates/crowdrelay-worker/src/community_intelligence/reddit.rs",
                "FETCH_TIMEOUT",
            )
        )
        self.assertEqual(
            min(read_callers),
            90_000,
            "the tightest read caller changed; update TIGHTEST_READ_CALLER_MS in "
            "crowdrelay-agents/tests/reddit-browser.test.ts to match",
        )


if __name__ == "__main__":
    unittest.main()
