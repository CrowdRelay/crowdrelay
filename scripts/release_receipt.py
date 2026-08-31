#!/usr/bin/env python3
"""Durable release receipt writer for blue-green deployments.

Stores release state outside Git under a root-owned directory:

    /var/lib/<service>/releases/
      current.json
      previous.json
      pending.json
      receipts/<release-id>.json
      configs/<release-id>/...

Each receipt records the full provenance and phase timeline of a release.
State files are written atomically (temp + rename) and never contain secrets.

Usage (called from deploy-bluegreen.sh):

    # Initialise the release state directory
    python3 scripts/release_receipt.py init --service crowdrelay \\
        --state-dir /var/lib/crowdrelay/releases

    # Write pending.json at deploy start
    python3 scripts/release_receipt.py pending --service crowdrelay \\
        --state-dir /var/lib/crowdrelay/releases \\
        --release-id "$release_id" \\
        --source-sha "$TARGET" \\
        --image-digests "api=$API_DIGEST" "worker=$WORKER_DIGEST" \\
        --oci-revision "$TARGET" \\
        --oci-architecture "$arch" \\
        --deploy-color "$DEPLOY_COLOR" \\
        --current-color "$current_color" \\
        --current-container "$CURRENT_API" \\
        --candidate-container "$NEW_API" \\
        --compose-file "$compose_file" \\
        --compose-digest "$compose_digest" \\
        --caddy-file "$EDGE_CADDYFILE" \\
        --caddy-digest "$caddy_digest" \\
        --env-file "$env_file"

    # Record a phase timestamp
    python3 scripts/release_receipt.py phase --state-dir ... \\
        --release-id "$release_id" --phase cutover --status pass

    # Finalise: pending -> current, current -> previous, write receipt
    python3 scripts/release_receipt.py finalize --state-dir ... \\
        --release-id "$release_id" --status pass

    # Rollback: write failure receipt, restore previous -> current
    python3 scripts/release_receipt.py rollback --state-dir ... \\
        --release-id "$release_id" --reason "soak-5xx-threshold"
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


def utc_now() -> str:
    """ISO-8601 UTC timestamp with second precision."""
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def sha256_file(path: str | Path) -> str:
    """Return sha256:hex digest of a file."""
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return f"sha256:{h.hexdigest()}"


def sha256_text(text: str) -> str:
    """Return sha256:hex digest of a UTF-8 string."""
    return f"sha256:{hashlib.sha256(text.encode()).hexdigest()}"


def atomic_write_json(path: Path, data: dict[str, Any]) -> None:
    """Write JSON atomically: temp file in same dir, then rename."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(
        dir=str(path.parent), prefix=path.name + ".", suffix=".tmp"
    )
    try:
        with os.fdopen(fd, "w") as f:
            json.dump(data, f, indent=2, sort_keys=True)
            f.write("\n")
        os.chmod(tmp, 0o644)
        os.rename(tmp, str(path))
    except Exception:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def load_json(path: Path) -> dict[str, Any] | None:
    if not path.exists():
        return None
    with open(path) as f:
        return json.load(f)


def state_dir(args: argparse.Namespace) -> Path:
    d = Path(getattr(args, "state_dir", ""))
    if not d:
        print("ERROR: --state-dir is required", file=sys.stderr)
        sys.exit(1)
    return d


def parse_kv_pairs(items: list[str] | None) -> dict[str, str]:
    """Parse 'key=value' pairs from a list."""
    result: dict[str, str] = {}
    for item in items or []:
        if "=" not in item:
            print(f"ERROR: expected key=value, got: {item}", file=sys.stderr)
            sys.exit(1)
        k, v = item.split("=", 1)
        result[k] = v
    return result


def cmd_init(args: argparse.Namespace) -> None:
    d = state_dir(args)
    (d / "receipts").mkdir(parents=True, exist_ok=True)
    (d / "configs").mkdir(parents=True, exist_ok=True)
    print(f"RECEIPT_INIT=PASS dir={d}")


def cmd_pending(args: argparse.Namespace) -> None:
    d = state_dir(args)
    digests = parse_kv_pairs(args.image_digests)
    config_digests = parse_kv_pairs(args.config_digests)

    # Compute config digests if files are provided but no digest given
    if args.compose_file and "compose" not in config_digests:
        config_digests["compose"] = sha256_file(args.compose_file)
    if args.caddy_file and "caddy" not in config_digests:
        config_digests["caddy"] = sha256_file(args.caddy_file)
    if args.env_file and "env" not in config_digests:
        # env files contain secrets — store only the digest, never the content
        config_digests["env"] = sha256_file(args.env_file)

    receipt: dict[str, Any] = {
        "service": args.service,
        "environment": args.environment,
        "releaseId": args.release_id,
        "sourceSha": args.source_sha,
        "ciRunId": args.ci_run_id or "",
        "imageDigests": digests,
        "ociRevision": args.oci_revision or "",
        "ociArchitecture": args.oci_architecture or "",
        "configDigests": config_digests,
        "deployColor": args.deploy_color or "",
        "currentColor": args.current_color or "",
        "currentContainer": args.current_container or "",
        "candidateContainer": args.candidate_container or "",
        "workerGeneration": args.worker_generation or "",
        "workerLeadership": args.worker_leadership or "",
        "caddyActiveUpstream": args.caddy_active_upstream or "",
        "phases": {
            "pending": {"timestamp": utc_now(), "status": "started"},
        },
        "probeResults": {},
        "observedErrorRate": None,
        "rollbackResult": None,
        "finalizedAt": None,
    }

    pending_path = d / "pending.json"
    atomic_write_json(pending_path, receipt)
    print(f"RECEIPT_PENDING=PASS release_id={args.release_id}")


def cmd_phase(args: argparse.Namespace) -> None:
    d = state_dir(args)
    pending = load_json(d / "pending.json")
    if pending is None:
        print("ERROR: no pending.json — cannot record phase", file=sys.stderr)
        sys.exit(1)
    phases = pending.setdefault("phases", {})
    phases[args.phase] = {
        "timestamp": utc_now(),
        "status": args.status,
        **({"detail": args.detail} if args.detail else {}),
    }
    atomic_write_json(d / "pending.json", pending)
    print(f"RECEIPT_PHASE=PASS phase={args.phase} status={args.status}")


def _write_receipt(d: Path, release_id: str, data: dict[str, Any]) -> Path:
    receipt_path = d / "receipts" / f"{release_id}.json"
    atomic_write_json(receipt_path, data)
    return receipt_path


def cmd_finalize(args: argparse.Namespace) -> None:
    d = state_dir(args)
    pending = load_json(d / "pending.json")
    if pending is None:
        print("ERROR: no pending.json — cannot finalize", file=sys.stderr)
        sys.exit(1)

    pending["phases"]["finalized"] = {
        "timestamp": utc_now(),
        "status": args.status,
    }
    pending["finalizedAt"] = utc_now()

    # Write per-release receipt
    receipt_path = _write_receipt(d, pending["releaseId"], pending)

    # Rotate: current -> previous, pending -> current
    current_path = d / "current.json"
    previous_path = d / "previous.json"

    if current_path.exists():
        shutil.copy2(str(current_path), str(previous_path))

    atomic_write_json(current_path, pending)
    (d / "pending.json").unlink(missing_ok=True)

    print(
        f"RECEIPT_FINALIZE=PASS release_id={pending['releaseId']} "
        f"status={args.status} receipt={receipt_path}"
    )


def cmd_rollback(args: argparse.Namespace) -> None:
    d = state_dir(args)
    pending = load_json(d / "pending.json")
    if pending is None:
        # No pending state — try to write a minimal failure receipt
        pending = {
            "releaseId": args.release_id,
            "service": args.service or "unknown",
            "phases": {},
        }

    pending["phases"]["rollback"] = {
        "timestamp": utc_now(),
        "status": "rolled-back",
        "reason": args.reason,
    }
    pending["rollbackResult"] = {
        "timestamp": utc_now(),
        "reason": args.reason,
        "status": "rolled-back",
    }
    pending["finalizedAt"] = utc_now()

    # Write failure receipt
    receipt_path = _write_receipt(d, pending["releaseId"], pending)

    # The failed release was only in pending.json — it was never promoted
    # to current.json (finalize only runs on success). So current.json
    # still holds the last good release and previous.json holds the one
    # before that. We just remove pending.json; current/previous are
    # untouched.
    (d / "pending.json").unlink(missing_ok=True)

    current_path = d / "current.json"
    current_exists = "true" if current_path.exists() else "false"
    print(
        f"RECEIPT_ROLLBACK=PASS current_intact={current_exists} "
        f"receipt={receipt_path}"
    )


def cmd_show(args: argparse.Namespace) -> None:
    d = state_dir(args)
    which = args.which
    path = d / f"{which}.json"
    data = load_json(path)
    if data is None:
        print(f"ERROR: {path} does not exist", file=sys.stderr)
        sys.exit(1)
    print(json.dumps(data, indent=2, sort_keys=True))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    def add_state_dir(p: argparse.ArgumentParser) -> None:
        p.add_argument("--state-dir", required=True, help="Release state directory")

    # init
    p_init = sub.add_parser("init", help="Create release state directory")
    add_state_dir(p_init)
    p_init.add_argument("--service", required=True)
    p_init.set_defaults(func=cmd_init)

    # pending
    p_pending = sub.add_parser("pending", help="Write pending.json")
    add_state_dir(p_pending)
    p_pending.add_argument("--service", required=True)
    p_pending.add_argument("--environment", default="production")
    p_pending.add_argument("--release-id", required=True)
    p_pending.add_argument("--source-sha", required=True)
    p_pending.add_argument("--ci-run-id", default="")
    p_pending.add_argument("--image-digests", nargs="*", default=[])
    p_pending.add_argument("--oci-revision", default="")
    p_pending.add_argument("--oci-architecture", default="")
    p_pending.add_argument("--deploy-color", default="")
    p_pending.add_argument("--current-color", default="")
    p_pending.add_argument("--current-container", default="")
    p_pending.add_argument("--candidate-container", default="")
    p_pending.add_argument("--worker-generation", default="")
    p_pending.add_argument("--worker-leadership", default="")
    p_pending.add_argument("--caddy-active-upstream", default="")
    p_pending.add_argument("--compose-file", default="")
    p_pending.add_argument("--caddy-file", default="")
    p_pending.add_argument("--env-file", default="")
    p_pending.add_argument("--config-digests", nargs="*", default=[])
    p_pending.set_defaults(func=cmd_pending)

    # phase
    p_phase = sub.add_parser("phase", help="Record a phase timestamp")
    add_state_dir(p_phase)
    p_phase.add_argument("--release-id", required=True)
    p_phase.add_argument("--phase", required=True)
    p_phase.add_argument("--status", required=True, choices=["pass", "fail", "started"])
    p_phase.add_argument("--detail", default="")
    p_phase.set_defaults(func=cmd_phase)

    # finalize
    p_finalize = sub.add_parser("finalize", help="Finalize release")
    add_state_dir(p_finalize)
    p_finalize.add_argument("--release-id", required=True)
    p_finalize.add_argument("--status", default="pass", choices=["pass", "fail"])
    p_finalize.set_defaults(func=cmd_finalize)

    # rollback
    p_rollback = sub.add_parser("rollback", help="Write failure receipt and restore previous")
    add_state_dir(p_rollback)
    p_rollback.add_argument("--release-id", required=True)
    p_rollback.add_argument("--service", default="")
    p_rollback.add_argument("--reason", required=True)
    p_rollback.set_defaults(func=cmd_rollback)

    # show
    p_show = sub.add_parser("show", help="Print a state file")
    add_state_dir(p_show)
    p_show.add_argument("which", choices=["current", "previous", "pending"])
    p_show.set_defaults(func=cmd_show)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
