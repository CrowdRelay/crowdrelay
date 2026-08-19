#!/usr/bin/env python3
"""Validate VIRYA's private n8n desired state before attestation/heartbeat.

This script is intentionally narrow: it catches the dangerous blue/green state
where the canonical team-email workflow exists but is inactive while an older
workflow is still active for the same event. It reads private exports locally
and prints only IDs/status, never workflow parameters or credentials.
"""
from __future__ import annotations

import argparse
import csv
import json
from pathlib import Path
from typing import Any

TEAM_EVENT = "viryaos.team.assignment_email_requested"
TEAM_CAPABILITY = "team.email"
TEAM_WORKFLOW_ID = "VOSTEAMEMAIL001"


def read_manifest(path: Path) -> list[dict[str, str]]:
    with path.open(newline="") as handle:
        rows = list(csv.DictReader(handle, delimiter="\t"))
    required = {"event_type", "workflow_id", "capability", "enabled"}
    if not rows or not required.issubset(rows[0]):
        raise ValueError("production manifest is empty or has an invalid header")
    return rows


def load_workflows(directory: Path) -> dict[str, dict[str, Any]]:
    workflows: dict[str, dict[str, Any]] = {}
    for path in sorted(directory.glob("*.json")):
        document = json.loads(path.read_text())
        candidates = document if isinstance(document, list) else [document]
        for workflow in candidates:
            if not isinstance(workflow, dict):
                continue
            workflow_id = workflow.get("id")
            if isinstance(workflow_id, str) and workflow_id:
                if workflow_id in workflows:
                    raise ValueError(f"duplicate private workflow id: {workflow_id}")
                workflows[workflow_id] = workflow
    if not workflows:
        raise ValueError("no private n8n workflow exports found")
    return workflows


def validate(rows: list[dict[str, str]], workflows: dict[str, dict[str, Any]]) -> None:
    team_rows = [
        row
        for row in rows
        if row.get("event_type") == TEAM_EVENT or row.get("capability") == TEAM_CAPABILITY
    ]
    if len(team_rows) != 1:
        raise ValueError(f"expected exactly one team.email route, found {len(team_rows)}")
    row = team_rows[0]
    if row.get("event_type") != TEAM_EVENT:
        raise ValueError("team.email capability is mapped to the wrong event")
    if row.get("workflow_id") != TEAM_WORKFLOW_ID:
        raise ValueError(
            f"team.email must map to canonical workflow {TEAM_WORKFLOW_ID}, got {row.get('workflow_id')}"
        )
    if row.get("enabled") != "1":
        raise ValueError("canonical team.email route must be enabled before release")

    canonical = workflows.get(TEAM_WORKFLOW_ID)
    if canonical is None:
        raise ValueError(f"canonical private workflow export is missing: {TEAM_WORKFLOW_ID}")
    if canonical.get("active") is not True:
        raise ValueError(f"canonical team.email workflow is inactive: {TEAM_WORKFLOW_ID}")

    stale_active: list[str] = []
    for workflow_id, workflow in workflows.items():
        if workflow_id == TEAM_WORKFLOW_ID or workflow.get("active") is not True:
            continue
        # This stays local: only the offending workflow ID is surfaced. The
        # workflow body itself is never printed or copied into public artifacts.
        if TEAM_EVENT in json.dumps(workflow, sort_keys=True, separators=(",", ":")):
            stale_active.append(workflow_id)
    if stale_active:
        raise ValueError(
            "non-canonical active workflow(s) still reference team-email event: "
            + ",".join(sorted(stale_active))
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--workflow-dir", type=Path, required=True)
    args = parser.parse_args()
    validate(read_manifest(args.manifest), load_workflows(args.workflow_dir))
    print(
        "VIRYA_N8N_DESIRED_STATE=PASS "
        f"team_email={TEAM_WORKFLOW_ID} enabled=true active=true"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
