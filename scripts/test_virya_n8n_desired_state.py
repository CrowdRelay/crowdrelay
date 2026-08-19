#!/usr/bin/env python3
import csv
import json
import tempfile
import unittest
from pathlib import Path

import validate_virya_n8n_desired_state as desired


class ViryaN8nDesiredStateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.manifest = self.root / "manifest.tsv"
        self.workflows = self.root / "workflows"
        self.workflows.mkdir()
        self.write_manifest(enabled="1", workflow_id=desired.TEAM_WORKFLOW_ID)
        self.write_workflow(desired.TEAM_WORKFLOW_ID, active=True, references_event=True)

    def tearDown(self):
        self.temp.cleanup()

    def write_manifest(self, *, enabled: str, workflow_id: str) -> None:
        with self.manifest.open("w", newline="") as handle:
            writer = csv.DictWriter(
                handle,
                fieldnames=["event_type", "workflow_id", "capability", "enabled"],
                delimiter="\t",
            )
            writer.writeheader()
            writer.writerow(
                {
                    "event_type": desired.TEAM_EVENT,
                    "workflow_id": workflow_id,
                    "capability": desired.TEAM_CAPABILITY,
                    "enabled": enabled,
                }
            )

    def write_workflow(self, workflow_id: str, *, active: bool, references_event: bool) -> None:
        payload = {
            "id": workflow_id,
            "active": active,
            "nodes": [
                {
                    "parameters": {
                        "event": desired.TEAM_EVENT if references_event else "other.event"
                    }
                }
            ],
        }
        (self.workflows / f"{workflow_id}.json").write_text(json.dumps(payload))

    def validate(self) -> None:
        desired.validate(
            desired.read_manifest(self.manifest),
            desired.load_workflows(self.workflows),
        )

    def test_canonical_active_team_email_passes(self):
        self.validate()

    def test_canonical_inactive_fails(self):
        self.write_workflow(desired.TEAM_WORKFLOW_ID, active=False, references_event=True)
        with self.assertRaisesRegex(ValueError, "canonical team.email workflow is inactive"):
            self.validate()

    def test_manifest_cannot_leave_team_email_disabled(self):
        self.write_manifest(enabled="0", workflow_id=desired.TEAM_WORKFLOW_ID)
        with self.assertRaisesRegex(ValueError, "must be enabled"):
            self.validate()

    def test_legacy_active_consumer_fails_closed(self):
        self.write_workflow("OLDTEAMFLOW", active=True, references_event=True)
        with self.assertRaisesRegex(ValueError, "non-canonical active workflow"):
            self.validate()

    def test_inactive_legacy_export_is_allowed_for_rollback_history(self):
        self.write_workflow("OLDTEAMFLOW", active=False, references_event=True)
        self.validate()


if __name__ == "__main__":
    unittest.main()
