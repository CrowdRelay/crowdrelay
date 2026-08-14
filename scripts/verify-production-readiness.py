#!/usr/bin/env python3
"""Verify production ViryaOS readiness and emit a secretless release receipt.

Desired state is never treated as deployment proof. The PASS condition binds
operational n8n health to immutable build provenance for every deployable
component, so a green receipt answers both "is it live?" and "what is live?".
"""
from __future__ import annotations

import argparse
import json
import os
import re
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

CODE_COMPONENTS = ("crowdrelay-api", "crowdrelay-worker", "virya-www", "synesthesia", "virya-signal")
MANIFEST_COMPONENTS = ("virya-www", "synesthesia", "virya-signal")
ALL_COMPONENTS = (*CODE_COMPONENTS, "n8n")
GIT_SHA = re.compile(r"^[0-9a-f]{40}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")


def fetch_release_ledger(base_url: str, admin_key: str, timeout: float) -> dict[str, Any]:
    url = base_url.rstrip("/") + "/v1/admin/autopilot/release-ledger"
    request = urllib.request.Request(
        url,
        headers={
            "Authorization": f"Bearer {admin_key}",
            "Accept": "application/json",
            "User-Agent": "viryaos-production-readiness/2",
        },
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        if response.status != 200:
            raise ValueError(f"release ledger returned HTTP {response.status}")
        return json.loads(response.read())


def is_sha256(value: Any) -> bool:
    return isinstance(value, str) and SHA256.fullmatch(value) is not None


def is_git_sha(value: Any) -> bool:
    return isinstance(value, str) and GIT_SHA.fullmatch(value) is not None


def is_digest(value: Any) -> bool:
    return isinstance(value, str) and DIGEST.fullmatch(value) is not None


def component_receipt(item: dict[str, Any]) -> dict[str, Any]:
    return {
        "sourceSha": item.get("source_sha"),
        "artifactDigest": item.get("artifact_digest"),
        "dependencyLockSha256": item.get("dependency_lock_sha256"),
        "artifactManifestSha256": item.get("artifact_manifest_sha256"),
        "deployRef": item.get("deploy_ref"),
        "version": item.get("version"),
        "manifestSha256": item.get("manifest_sha"),
        "observedAt": item.get("observed_at"),
        "stale": item.get("stale") is True,
    }


def evaluate(ledger: dict[str, Any]) -> tuple[list[str], dict[str, Any]]:
    failures: list[str] = []
    if ledger.get("executor_manifest_drift") is not False:
        failures.append("executor-manifest-drift")
    if ledger.get("backend_sha_drift") is not False:
        failures.append("backend-sha-drift")
    if ledger.get("n8n_attestation_ready") is not True:
        failures.append("n8n-attestation-not-ready")
    team_executors = int(ledger.get("active_team_email_executor_count", 0) or 0)
    if team_executors < 1:
        failures.append("team-email-executor-not-live")
    if ledger.get("team_email_live") is not True:
        failures.append("team-email-not-live")

    missing = sorted({str(value) for value in ledger.get("missing_components", [])})
    if missing:
        failures.append("release-components-missing")

    components = {
        item.get("component_key"): item
        for item in ledger.get("components", [])
        if isinstance(item, dict) and isinstance(item.get("component_key"), str)
    }
    for key in ALL_COMPONENTS:
        if key not in components and key not in missing:
            missing.append(key)
    missing = sorted(set(missing))
    if missing and "release-components-missing" not in failures:
        failures.append("release-components-missing")

    for key in CODE_COMPONENTS:
        item = components.get(key) or {}
        if not is_git_sha(item.get("source_sha")):
            failures.append(f"{key}-source-sha-invalid")
        if not is_digest(item.get("artifact_digest")):
            failures.append(f"{key}-artifact-digest-missing")
        if not is_sha256(item.get("dependency_lock_sha256")):
            failures.append(f"{key}-dependency-lock-missing")
        if key in MANIFEST_COMPONENTS and not is_sha256(item.get("artifact_manifest_sha256")):
            failures.append(f"{key}-artifact-manifest-missing")
        if item.get("stale") is True:
            failures.append(f"{key}-stale")

    n8n = components.get("n8n") or {}
    attestation_sha = n8n.get("workflow_attestation_sha")
    attested_at = n8n.get("workflow_attested_at")
    manifest_sha = n8n.get("manifest_sha")
    if not is_sha256(attestation_sha):
        failures.append("n8n-attestation-sha-missing")
    if not is_sha256(manifest_sha):
        failures.append("n8n-manifest-sha-missing")
    if n8n.get("source_sha") != manifest_sha:
        failures.append("n8n-source-manifest-mismatch")
    if not isinstance(attested_at, str) or not attested_at:
        failures.append("n8n-attested-at-missing")
    if n8n.get("stale") is True:
        failures.append("n8n-stale")

    component_provenance = {
        key: component_receipt(components.get(key) or {}) for key in ALL_COMPONENTS
    }
    component_provenance["n8n"].update(
        {
            "workflowAttestationSha256": attestation_sha,
            "workflowAttestedAt": attested_at,
        }
    )
    receipt = {
        "schema": 2,
        "checkedAt": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "status": "pass" if not failures else "fail",
        "teamEmail": {
            "live": ledger.get("team_email_live") is True,
            "activeExecutorCount": team_executors,
        },
        "n8n": {
            "attestationReady": ledger.get("n8n_attestation_ready") is True,
            "attestationSha256": attestation_sha,
            "attestedAt": attested_at,
            "manifestSha256": manifest_sha,
            "sourceSha": n8n.get("source_sha"),
        },
        "components": component_provenance,
        "executorManifestDrift": ledger.get("executor_manifest_drift"),
        "backendShaDrift": ledger.get("backend_sha_drift"),
        "missingComponents": missing,
        "failures": sorted(set(failures)),
    }
    return receipt["failures"], receipt


def write_receipts(output: Path, release_output: Path, receipt: dict[str, Any]) -> None:
    serialized = json.dumps(receipt, indent=2, sort_keys=True) + "\n"
    for path in {output, release_output}:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(serialized)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", default=os.environ.get("CROWDRELAY_PRODUCTION_BASE_URL", ""))
    parser.add_argument("--admin-key-env", default="CROWDRELAY_PRODUCTION_ADMIN_API_KEY")
    parser.add_argument("--input", type=Path, help="offline ledger fixture for tests")
    parser.add_argument("--output", type=Path, default=Path("artifacts/operational-readiness.json"))
    parser.add_argument("--release-output", type=Path, default=Path("artifacts/virya-os-release-receipt.json"))
    parser.add_argument("--timeout-seconds", type=float, default=10.0)
    args = parser.parse_args()

    try:
        if args.input:
            ledger = json.loads(args.input.read_text())
        else:
            if not args.base_url:
                raise ValueError("CROWDRELAY_PRODUCTION_BASE_URL is required")
            admin_key = os.environ.get(args.admin_key_env, "")
            if not admin_key:
                raise ValueError(f"{args.admin_key_env} is required")
            ledger = fetch_release_ledger(args.base_url, admin_key, args.timeout_seconds)
        failures, receipt = evaluate(ledger)
    except (OSError, ValueError, TypeError, json.JSONDecodeError, urllib.error.URLError) as error:
        receipt = {
            "schema": 2,
            "checkedAt": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
            "status": "fail",
            "failures": ["readiness-check-unavailable"],
            "errorClass": type(error).__name__,
        }
        write_receipts(args.output, args.release_output, receipt)
        print(f"PRODUCTION_READINESS=FAIL error_class={type(error).__name__}")
        return 1

    write_receipts(args.output, args.release_output, receipt)
    if failures:
        print("PRODUCTION_READINESS=FAIL checks=" + ",".join(failures))
        return 1
    print(
        "PRODUCTION_READINESS=PASS "
        f"team_email_executors={receipt['teamEmail']['activeExecutorCount']} "
        f"n8n_manifest={receipt['n8n']['manifestSha256']}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
