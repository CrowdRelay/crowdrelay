#!/usr/bin/env python3
"""Pin the operator-initiated autopilot cycle.

The manual "run a cycle now" button is a side-effecting control: it makes the
brain dispatch real outreach. Three properties keep it honest, and none of them
is visible from any single file:

1. The NOTIFY channel name is written twice — once where the request is sent
   (infra) and once where it is received (worker). A typo in either would make
   the button silently do nothing, because NOTIFY to a channel nobody listens
   on is not an error.

2. The manual path must not have its own execution route. It wakes the worker's
   existing loop, so it runs the same `run_once` a scheduled tick runs and is
   therefore subject to the same 24-hour action quota, which is enforced in the
   transaction that writes an action. An API handler that evaluated or
   dispatched directly would bypass that.

3. The run endpoint must respect the autopilot master switch, or the button
   becomes a way to start a disabled autopilot.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

WORKER = ROOT / "crates/crowdrelay-worker/src/autopilot.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/cycle_trigger.rs"
API = ROOT / "crates/crowdrelay-api/src/autopilot/cycle.rs"
ROUTER = ROOT / "crates/crowdrelay-api/src/control_plane.rs"

CHANNEL = re.compile(r'AUTOPILOT_CYCLE_CHANNEL:\s*&str\s*=\s*"([a-z_]+)"')


class AutopilotManualCycleV1(unittest.TestCase):
    def test_notify_channel_matches_on_both_sides(self) -> None:
        worker = CHANNEL.search(WORKER.read_text())
        infra = CHANNEL.search(INFRA.read_text())
        self.assertIsNotNone(worker, "worker does not declare AUTOPILOT_CYCLE_CHANNEL")
        self.assertIsNotNone(infra, "infra does not declare AUTOPILOT_CYCLE_CHANNEL")
        assert worker is not None and infra is not None
        self.assertEqual(
            worker.group(1),
            infra.group(1),
            "sender and listener disagree on the channel; the button would do nothing",
        )

    def test_the_worker_actually_listens(self) -> None:
        worker = WORKER.read_text()
        self.assertIn("PgListener", worker, "worker never subscribes to the channel")
        self.assertIn(
            "listen(AUTOPILOT_CYCLE_CHANNEL)",
            worker,
            "worker builds a listener but does not subscribe to the cycle channel",
        )

    def test_a_missing_listener_does_not_stop_scheduled_cycles(self) -> None:
        """Losing the listener costs the button, never the timer."""
        worker = WORKER.read_text()
        self.assertIn(
            "scheduled cycles continue",
            worker,
            "a listener failure must degrade to timer-only, not take autopilot down",
        )

    def test_the_api_does_not_evaluate_or_dispatch_directly(self) -> None:
        """The manual path must reuse the worker loop, not reimplement it."""
        api = API.read_text()
        for forbidden in (
            "EvaluateAutopilot",
            "load_growth_intelligence_snapshots",
            "run_once",
        ):
            self.assertNotIn(
                forbidden,
                api,
                f"the run endpoint references {forbidden}; a second execution path "
                f"would bypass the action quota enforced in the worker's",
            )

    def test_the_run_endpoint_respects_the_master_switch(self) -> None:
        api = API.read_text()
        self.assertIn(
            "autopilot_runtime_enabled",
            api,
            "the button must not be able to start a disabled autopilot",
        )

    def test_both_routes_are_registered_under_the_control_plane(self) -> None:
        router = ROUTER.read_text()
        for path in (
            "/v1/control-plane/autopilot/cycle/preview",
            "/v1/control-plane/autopilot/cycle/run",
        ):
            self.assertIn(path, router, f"{path} is not registered")

    def test_the_preview_is_read_only(self) -> None:
        """Preview is offered before a run, so it must never write."""
        infra = INFRA.read_text()
        preview = infra.split("pub async fn preview_autopilot_cycle", 1)[1]
        for write in ("INSERT", "UPDATE", "DELETE", "pg_notify"):
            self.assertNotIn(
                write,
                preview,
                f"preview performs a {write}; it is documented as read-only",
            )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        channel = CHANNEL.search(INFRA.read_text())
        name = channel.group(1) if channel else "?"
        print(f"AUTOPILOT_MANUAL_CYCLE_V1=PASS channel={name} path=worker-loop guarded=quota+master-switch")
    else:
        print("AUTOPILOT_MANUAL_CYCLE_V1=FAIL")
        sys.exit(1)
