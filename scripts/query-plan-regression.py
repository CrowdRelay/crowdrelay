#!/usr/bin/env python3
"""Verify that PostgreSQL 18 can use the intended indexes for CrowdRelay hot paths.

The check deliberately disables sequential scans inside each EXPLAIN. This is an
index-capability regression test, not a latency benchmark: it catches query/index
shape drift deterministically even on an almost-empty CI database. Real planner
choices and latency are covered by the separate runtime benchmark workflow.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from dataclasses import dataclass


@dataclass(frozen=True)
class Case:
    name: str
    expected_index: str
    sql: str


ZERO = "00000000-0000-0000-0000-000000000001"
OTHER = "00000000-0000-0000-0000-000000000002"

CASES = (
    Case(
        "audit request timeline",
        "audit_events_ops_request_timeline_idx",
        f"SELECT id FROM audit_events WHERE workspace_id='{ZERO}'::uuid AND request_id='{OTHER}'::uuid ORDER BY occurred_at,id LIMIT 100",
    ),
    Case(
        "outbox request timeline",
        "outbox_events_ops_request_timeline_idx",
        f"SELECT id FROM outbox_events WHERE workspace_id='{ZERO}'::uuid AND request_id='{OTHER}'::uuid ORDER BY created_at,id LIMIT 100",
    ),
    Case(
        "operator request timeline",
        "operator_actions_ops_request_timeline_idx",
        f"SELECT id FROM operator_actions WHERE workspace_id='{ZERO}'::uuid AND request_id='{OTHER}'::uuid ORDER BY created_at,id LIMIT 100",
    ),
    Case(
        "autopilot action claim",
        "viryaos_autopilot_actions_due_idx",
        f"SELECT id FROM viryaos_autopilot_actions WHERE workspace_id='{ZERO}'::uuid AND status='queued' AND attempt_count < 5 AND available_at <= now() ORDER BY available_at,id LIMIT 50",
    ),
    Case(
        "autopilot measurement claim",
        "viryaos_autopilot_measurements_due_idx",
        f"SELECT id FROM viryaos_autopilot_measurements WHERE workspace_id='{ZERO}'::uuid AND status='pending' AND attempt_count < 3 AND due_at <= now() AND available_at <= now() ORDER BY due_at,available_at,id LIMIT 50",
    ),
    Case(
        "AREA wallet ledger",
        "area_credit_ledger_player_idx",
        f"SELECT id FROM area_credit_ledger WHERE workspace_id='{ZERO}'::uuid AND player_id='{OTHER}'::uuid ORDER BY created_at DESC LIMIT 100",
    ),
    Case(
        "AREA voucher history",
        "area_reward_vouchers_player_idx",
        f"SELECT id FROM area_reward_vouchers WHERE workspace_id='{ZERO}'::uuid AND player_id='{OTHER}'::uuid ORDER BY issued_at DESC LIMIT 100",
    ),
    Case(
        "AREA ticket reward history",
        "area_ticket_rewards_player_idx",
        f"SELECT id FROM area_ticket_rewards WHERE workspace_id='{ZERO}'::uuid AND player_id='{OTHER}'::uuid ORDER BY created_at DESC LIMIT 100",
    ),
    Case(
        "staff device bearer authentication",
        "staff_device_sessions_active_token_idx",
        f"SELECT EXISTS (SELECT 1 FROM staff_device_sessions WHERE workspace_id='{ZERO}'::uuid AND token_hash=decode(repeat('00',32),'hex') AND revoked_at IS NULL AND expires_at > now())",
    ),
)


def explain(database_url: str, sql: str) -> object:
    command = [
        "psql",
        database_url,
        "-X",
        "-q",
        "-A",
        "-t",
        "-v",
        "ON_ERROR_STOP=1",
        "-c",
        f"SET enable_seqscan=off; EXPLAIN (FORMAT JSON, COSTS OFF) {sql};",
    ]
    result = subprocess.run(command, check=True, capture_output=True, text=True)
    payload = result.stdout.strip()
    if not payload:
        raise RuntimeError("psql returned an empty EXPLAIN payload")
    return json.loads(payload)


def index_names(value: object) -> set[str]:
    found: set[str] = set()
    if isinstance(value, dict):
        name = value.get("Index Name")
        if isinstance(name, str):
            found.add(name)
        for child in value.values():
            found.update(index_names(child))
    elif isinstance(value, list):
        for child in value:
            found.update(index_names(child))
    return found


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--database-url",
        default=os.environ.get("CROWDRELAY_TEST_DATABASE_URL")
        or os.environ.get("CROWDRELAY_DATABASE_URL"),
    )
    args = parser.parse_args()
    if not args.database_url:
        parser.error("--database-url or CROWDRELAY_TEST_DATABASE_URL is required")
    if subprocess.run(["sh", "-c", "command -v psql >/dev/null"], check=False).returncode != 0:
        raise SystemExit("psql is required for query-plan regression checks")

    failures: list[str] = []
    for case in CASES:
        plan = explain(args.database_url, case.sql)
        used = index_names(plan)
        if case.expected_index not in used:
            failures.append(
                f"{case.name}: expected {case.expected_index}, planner exposed {sorted(used) or ['<none>']}"
            )
        else:
            print(f"QUERY_PLAN=PASS case={case.name!r} index={case.expected_index}")
    if failures:
        print("QUERY_PLAN=FAIL", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print(f"QUERY_PLAN_REGRESSION=PASS cases={len(CASES)} postgres=index-capability")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
