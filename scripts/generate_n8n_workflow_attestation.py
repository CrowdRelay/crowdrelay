#!/usr/bin/env python3
"""Generate a public, secretless attestation from private n8n workflow exports.

The attestation binds structural workflow facts to a smoke-test result that names
that exact workflow SHA. Production JSON stays private; only hashes, node types,
safe persistence settings, activity and contract/smoke booleans are emitted.
"""
from __future__ import annotations

import argparse
import csv
import hashlib
import json
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

CLAIM_CONTRACT = "execution-claim-v1"
RECEIPT_CONTRACT = "execution-report-v1"
CLAIM_OPTIONAL_CAPABILITIES = {"calendar.upsert"}
SAFE_SETTINGS = {
    "saveDataErrorExecution": "none",
    "saveDataSuccessExecution": "none",
    "saveManualExecutions": False,
}


def canonical_sha(value: Any) -> str:
    payload = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
    return hashlib.sha256(payload).hexdigest()


def manifest_sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read_manifest(path: Path) -> list[dict[str, str]]:
    with path.open(newline="") as handle:
        rows = list(csv.DictReader(handle, delimiter="\t"))
    required = {"event_type", "workflow_id", "capability", "enabled"}
    if not rows or set(rows[0]) != required:
        raise ValueError(f"unexpected production manifest columns in {path}")
    return rows


def workflow_documents(path: Path) -> list[dict[str, Any]]:
    raw = json.loads(path.read_text())
    if isinstance(raw, list):
        docs = raw
    elif isinstance(raw, dict) and isinstance(raw.get("nodes"), list):
        docs = [raw]
    elif isinstance(raw, dict) and isinstance(raw.get("data"), list):
        docs = raw["data"]
    else:
        raise ValueError(f"unsupported n8n export shape: {path}")
    return [item for item in docs if isinstance(item, dict)]


def load_workflows(directory: Path) -> dict[str, dict[str, Any]]:
    workflows: dict[str, dict[str, Any]] = {}
    for path in sorted(directory.glob("*.json")):
        for workflow in workflow_documents(path):
            workflow_id = str(workflow.get("id") or "").strip()
            if not workflow_id:
                continue
            if workflow_id in workflows:
                raise ValueError(f"duplicate workflow id {workflow_id}")
            workflows[workflow_id] = workflow
    return workflows


def persistence_summary(workflow: dict[str, Any]) -> dict[str, Any]:
    settings = workflow.get("settings") if isinstance(workflow.get("settings"), dict) else {}
    return {
        "saveDataErrorExecution": settings.get("saveDataErrorExecution"),
        "saveDataSuccessExecution": settings.get("saveDataSuccessExecution"),
        "saveManualExecutions": settings.get("saveManualExecutions"),
        "saveExecutionProgress": settings.get("saveExecutionProgress", False),
    }


def safe_persistence(summary: dict[str, Any]) -> bool:
    return all(summary.get(key) == expected for key, expected in SAFE_SETTINGS.items()) and summary.get(
        "saveExecutionProgress"
    ) in (False, None)


def node_type_counts(workflow: dict[str, Any]) -> dict[str, int]:
    counter: Counter[str] = Counter()
    for node in workflow.get("nodes") or []:
        if isinstance(node, dict) and isinstance(node.get("type"), str):
            counter[node["type"]] += 1
    return dict(sorted(counter.items()))


def parse_timestamp(value: str) -> datetime:
    normalized = value.replace("Z", "+00:00")
    parsed = datetime.fromisoformat(normalized)
    if parsed.tzinfo is None:
        raise ValueError("smoke testedAt must include timezone")
    return parsed.astimezone(timezone.utc)


def smoke_is_fresh(tested_at: str, now: datetime, max_age_days: int) -> bool:
    tested = parse_timestamp(tested_at)
    age_seconds = (now - tested).total_seconds()
    return -300 <= age_seconds <= max_age_days * 86400


def smoke_template(rows: list[dict[str, str]], workflows: dict[str, dict[str, Any]]) -> dict[str, Any]:
    mappings: dict[str, dict[str, Any]] = defaultdict(lambda: {"capabilities": set(), "enabled": False})
    for row in rows:
        if row["workflow_id"] == "UNAVAILABLE":
            continue
        entry = mappings[row["workflow_id"]]
        entry["capabilities"].add(row["capability"])
        entry["enabled"] = entry["enabled"] or row["enabled"] == "1"
    result: dict[str, Any] = {}
    for workflow_id, mapping in sorted(mappings.items()):
        workflow = workflows.get(workflow_id)
        if workflow is None:
            if mapping["enabled"]:
                raise ValueError(f"enabled workflow export missing: {workflow_id}")
            continue
        caps = mapping["capabilities"]
        requires_claim = not all(capability in CLAIM_OPTIONAL_CAPABILITIES for capability in caps)
        result[workflow_id] = {
            "workflowSha256": canonical_sha(workflow),
            "candidateEnabled": bool(mapping["enabled"]),
            "testedAt": None,
            "eventValidation": False,
            "executionClaim": False if requires_claim else None,
            "providerReceipt": False,
            "receiptBeforeRetry": False,
            "credentialCheck": False,
            "claimContractVersion": CLAIM_CONTRACT,
            "receiptContractVersion": RECEIPT_CONTRACT,
        }
    return result


def build_attestation(
    manifest_path: Path,
    rows: list[dict[str, str]],
    workflows: dict[str, dict[str, Any]],
    smoke: dict[str, Any],
    now: datetime,
    max_smoke_age_days: int,
) -> dict[str, Any]:
    grouped: dict[str, list[dict[str, str]]] = defaultdict(list)
    for row in rows:
        if row["workflow_id"] != "UNAVAILABLE":
            grouped[row["workflow_id"]].append(row)

    attestations: list[dict[str, Any]] = []
    failures: list[str] = []
    for workflow_id, mappings in sorted(grouped.items()):
        enabled = any(row["enabled"] == "1" for row in mappings)
        workflow = workflows.get(workflow_id)
        if workflow is None:
            if enabled:
                failures.append(f"enabled workflow export missing: {workflow_id}")
            continue

        workflow_sha = canonical_sha(workflow)
        persistence = persistence_summary(workflow)
        active = bool(workflow.get("active"))
        caps = sorted({row["capability"] for row in mappings})
        events = sorted({row["event_type"] for row in mappings})
        requires_claim = enabled and not all(cap in CLAIM_OPTIONAL_CAPABILITIES for cap in caps)
        smoke_result = smoke.get(workflow_id) if isinstance(smoke, dict) else None

        smoke_public: dict[str, Any] | None = None
        if enabled:
            if not active:
                failures.append(f"enabled workflow is not active: {workflow_id}")
            if not safe_persistence(persistence):
                failures.append(f"unsafe execution-data persistence: {workflow_id}")
            if not isinstance(smoke_result, dict):
                failures.append(f"smoke evidence missing: {workflow_id}")
            else:
                tested_at = smoke_result.get("testedAt")
                if smoke_result.get("workflowSha256") != workflow_sha:
                    failures.append(f"smoke hash does not match export: {workflow_id}")
                if not isinstance(tested_at, str) or not smoke_is_fresh(tested_at, now, max_smoke_age_days):
                    failures.append(f"smoke evidence stale or invalid: {workflow_id}")
                for key in ("eventValidation", "providerReceipt", "receiptBeforeRetry", "credentialCheck"):
                    if smoke_result.get(key) is not True:
                        failures.append(f"smoke check {key} failed: {workflow_id}")
                if requires_claim and smoke_result.get("executionClaim") is not True:
                    failures.append(f"execution claim check failed: {workflow_id}")
                if smoke_result.get("claimContractVersion") != CLAIM_CONTRACT:
                    failures.append(f"claim contract drift: {workflow_id}")
                if smoke_result.get("receiptContractVersion") != RECEIPT_CONTRACT:
                    failures.append(f"receipt contract drift: {workflow_id}")
                smoke_public = {
                    "testedAt": tested_at,
                    "eventValidation": smoke_result.get("eventValidation") is True,
                    "executionClaim": smoke_result.get("executionClaim") if requires_claim else None,
                    "providerReceipt": smoke_result.get("providerReceipt") is True,
                    "receiptBeforeRetry": smoke_result.get("receiptBeforeRetry") is True,
                    "credentialCheck": smoke_result.get("credentialCheck") is True,
                    "claimContractVersion": smoke_result.get("claimContractVersion"),
                    "receiptContractVersion": smoke_result.get("receiptContractVersion"),
                }

        attestations.append(
            {
                "workflowId": workflow_id,
                "workflowSha256": workflow_sha,
                "events": events,
                "capabilities": caps,
                "enabled": enabled,
                "active": active,
                "nodeTypeCounts": node_type_counts(workflow),
                "persistence": persistence,
                "smoke": smoke_public,
            }
        )

    if failures:
        raise ValueError("; ".join(failures))

    return {
        "schemaVersion": 1,
        "generatedAt": now.replace(microsecond=0).isoformat().replace("+00:00", "Z"),
        "routeManifestSha256": manifest_sha(manifest_path),
        "claimContractVersion": CLAIM_CONTRACT,
        "receiptContractVersion": RECEIPT_CONTRACT,
        "workflows": attestations,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--workflow-dir", type=Path, required=True)
    parser.add_argument("--smoke-results", type=Path)
    parser.add_argument("--smoke-template-out", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--max-smoke-age-days", type=int, default=14)
    args = parser.parse_args()

    rows = read_manifest(args.manifest)
    workflows = load_workflows(args.workflow_dir)
    if args.smoke_template_out:
        template = smoke_template(rows, workflows)
        args.smoke_template_out.parent.mkdir(parents=True, exist_ok=True)
        args.smoke_template_out.write_text(json.dumps(template, indent=2, sort_keys=True) + "\n")
        print(f"N8N_ATTEST_TEMPLATE=PASS workflows={len(template)} output={args.smoke_template_out}")
        if not args.smoke_results:
            return 0

    if not args.smoke_results or not args.output:
        parser.error("final attestation requires --smoke-results and --output")
    smoke = json.loads(args.smoke_results.read_text())
    now = datetime.now(timezone.utc)
    attestation = build_attestation(
        args.manifest,
        rows,
        workflows,
        smoke,
        now,
        args.max_smoke_age_days,
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    encoded = json.dumps(attestation, indent=2, sort_keys=True) + "\n"
    args.output.write_text(encoded)
    digest = hashlib.sha256(encoded.encode()).hexdigest()
    sha_path = args.output.with_suffix(args.output.suffix + ".sha256")
    sha_path.write_text(digest + "\n")
    print(f"N8N_ATTEST=PASS workflows={len(attestation['workflows'])} sha256={digest} output={args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
