from __future__ import annotations

import json
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]
EXAMPLES = ROOT / "n8n" / "examples"


class N8nExecuteWorkflowTriggerContract(unittest.TestCase):
    def test_subworkflow_triggers_pass_the_verified_event_through(self) -> None:
        checked = 0
        failures: list[str] = []

        for path in sorted(EXAMPLES.glob("*.json")):
            payload = json.loads(path.read_text())
            for node in payload.get("nodes", []):
                if node.get("type") != "n8n-nodes-base.executeWorkflowTrigger":
                    continue
                checked += 1
                parameters = node.get("parameters") or {}
                if parameters.get("inputSource") != "passthrough":
                    failures.append(
                        f"{path.relative_to(ROOT)}::{node.get('name', '<unnamed>')} "
                        f"must set parameters.inputSource='passthrough'"
                    )

        self.assertGreater(checked, 0, "no executeWorkflowTrigger nodes found")
        self.assertEqual([], failures, "\n".join(failures))


if __name__ == "__main__":
    unittest.main()
