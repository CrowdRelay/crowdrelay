#!/usr/bin/env python3
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
API = (ROOT / "crates/crowdrelay-api/src/main.rs").read_text()

class ApiRuntimeSupervisionContract(unittest.TestCase):
    def test_server_and_background_tasks_are_supervised_during_runtime(self):
        self.assertIn("let mut runtime_tasks = JoinSet::new();", API)
        self.assertIn("first_exit = runtime_tasks.join_next()", API)
        self.assertIn("unexpected_runtime_exit(first_exit)", API)
        self.assertIn('"click ingestion"', API)
        self.assertIn('"event action ingestion"', API)
        self.assertIn('"smart-link refresh"', API)
        self.assertIn('"event refresh"', API)
        self.assertIn("stopped before shutdown was requested", API)

    def test_unexpected_exit_triggers_bounded_graceful_shutdown(self):
        self.assertIn("let _ = shutdown_sender.send(true);", API)
        self.assertIn("drain_runtime_tasks(", API)
        self.assertIn("timeout(deadline, drain_runtime_tasks_inner(runtime_tasks))", API)
        self.assertIn("runtime_tasks.abort_all();", API)
        self.assertIn("runtime_result.and(shutdown_result)", API)

if __name__ == "__main__":
    unittest.main()
