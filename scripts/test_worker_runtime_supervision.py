#!/usr/bin/env python3
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKER = (ROOT / "crates/crowdrelay-worker/src/main.rs").read_text()


class WorkerRuntimeSupervisionContract(unittest.TestCase):
    def test_all_long_lived_workers_share_one_joinset_supervisor(self):
        self.assertIn("let mut runtime_tasks = JoinSet::new();", WORKER)
        self.assertGreaterEqual(WORKER.count("runtime_tasks.spawn(async move"), 9)
        self.assertIn("first_exit = runtime_tasks.join_next()", WORKER)
        self.assertIn("unexpected_worker_exit(first_exit)", WORKER)
        for name in (
            "outbox worker",
            "event reminder scheduler",
            "retention worker",
            "event sync worker",
            "weighted draw worker",
            "ViryaOS Autopilot worker",
            "ViryaOS team-email worker",
            "fan push delivery worker",
            "CrowdRelay ops watchdog",
        ):
            self.assertIn(f'"{name}"', WORKER)

    def test_unexpected_exit_still_runs_bounded_graceful_shutdown(self):
        first_exit = WORKER.index("first_exit = runtime_tasks.join_next()")
        notify = WORKER.index("let _ = shutdown_sender.send(true);", first_exit)
        drain = WORKER.index("drain_worker_tasks(", notify)
        result = WORKER.index("runtime_result.and(shutdown_result)", drain)
        self.assertLess(first_exit, notify)
        self.assertLess(notify, drain)
        self.assertLess(drain, result)
        self.assertIn("runtime_tasks.abort_all();", WORKER)
        self.assertIn("timeout(deadline, drain_worker_tasks_inner(runtime_tasks))", WORKER)
        self.assertNotIn("await_worker_shutdown(", WORKER)


if __name__ == "__main__":
    unittest.main()
