#!/usr/bin/env python3
"""Build a fail-closed n8n executor heartbeat from the public attestation.

The operator never hand-types capability lists or attestation metadata. The
heartbeat is derived from the exact production route manifest and the exact
secretless attestation generated from private workflow exports + provider smoke.
"""
from __future__ import annotations

import argparse
import csv
import hashlib
import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

MAX_ATTESTATION_AGE = timedelta(days=14)
MAX_TTL = timedelta(hours=2)


def parse_time(value: str) -> datetime:
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError("timestamp must include timezone")
    return parsed.astimezone(timezone.utc)


def sha256_bytes(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read_manifest(path: Path) -> list[dict[str, str]]:
    with path.open(newline="") as handle:
        rows = list(csv.DictReader(handle, delimiter="\t"))
    if not rows:
        raise ValueError("production workflow manifest is empty")
    return rows


def validate_attestation(
    manifest: Path,
    rows: list[dict[str, str]],
    attestation_path: Path,
    now: datetime,
) -> tuple[dict[str, Any], str]:
    attestation = json.loads(attestation_path.read_text())
    manifest_digest = sha256_bytes(manifest)
    if attestation.get("routeManifestSha256") != manifest_digest:
        raise ValueError("attestation route-manifest SHA does not match production manifest")
    generated_at_raw = attestation.get("generatedAt")
    if not isinstance(generated_at_raw, str):
        raise ValueError("attestation generatedAt is missing")
    generated_at = parse_time(generated_at_raw)
    age = now - generated_at
    if age < timedelta(minutes=-5) or age > MAX_ATTESTATION_AGE:
        raise ValueError("attestation is stale or from the future")

    workflow_by_id = {
        item.get("workflowId"): item
        for item in attestation.get("workflows", [])
        if isinstance(item, dict) and isinstance(item.get("workflowId"), str)
    }
    for row in rows:
        if row.get("enabled") != "1":
            continue
        workflow_id = row.get("workflow_id")
        if not workflow_id or workflow_id == "UNAVAILABLE":
            raise ValueError(f"enabled capability has no workflow: {row.get('capability')}")
        evidence = workflow_by_id.get(workflow_id)
        if not evidence or evidence.get("enabled") is not True or evidence.get("active") is not True:
            raise ValueError(f"enabled workflow lacks active attestation: {workflow_id}")
        smoke = evidence.get("smoke")
        if not isinstance(smoke, dict):
            raise ValueError(f"enabled workflow lacks smoke evidence: {workflow_id}")
        for key in ("eventValidation", "providerReceipt", "receiptBeforeRetry", "credentialCheck"):
            if smoke.get(key) is not True:
                raise ValueError(f"workflow smoke check {key} is not proven: {workflow_id}")
        if smoke.get("executionClaim") is False:
            raise ValueError(f"workflow execution claim is not proven: {workflow_id}")
    return attestation, manifest_digest


def build_heartbeat(
    manifest: Path,
    attestation_path: Path,
    executor_id: str,
    version: str,
    ttl_minutes: int,
    now: datetime,
) -> dict[str, Any]:
    if not executor_id.strip() or len(executor_id) > 120:
        raise ValueError("executor id must be 1..120 characters")
    if not version.strip() or len(version) > 80:
        raise ValueError("executor version must be 1..80 characters")
    ttl = timedelta(minutes=ttl_minutes)
    if ttl <= timedelta(0) or ttl > MAX_TTL:
        raise ValueError("heartbeat TTL must be in (0, 120] minutes")
    rows = read_manifest(manifest)
    attestation, manifest_digest = validate_attestation(manifest, rows, attestation_path, now)
    capabilities = sorted({row["capability"] for row in rows if row.get("enabled") == "1"})
    if not capabilities:
        raise ValueError("production manifest advertises no capabilities")
    attestation_digest = sha256_bytes(attestation_path)
    generated_at = str(attestation["generatedAt"])
    return {
        "executor_id": executor_id,
        "version": version,
        "manifest_sha": manifest_digest,
        "capabilities": [{"capability": capability, "version": "1"} for capability in capabilities],
        "metadata": {
            "workflow_attestation_sha": attestation_digest,
            "workflow_attestation_manifest_sha": manifest_digest,
            "workflow_attested_at": generated_at,
            "heartbeat_builder": "crowdrelay-repo-v1",
        },
        "observed_at": now.replace(microsecond=0).isoformat().replace("+00:00", "Z"),
        "expires_at": (now + ttl).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--attestation", type=Path, required=True)
    parser.add_argument("--executor-id", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--ttl-minutes", type=int, default=90)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    payload = build_heartbeat(
        args.manifest,
        args.attestation,
        args.executor_id,
        args.version,
        args.ttl_minutes,
        datetime.now(timezone.utc),
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    print(
        "N8N_HEARTBEAT_BUILD=PASS "
        f"capabilities={len(payload['capabilities'])} manifest={payload['manifest_sha']} output={args.output}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
