#!/usr/bin/env python3
"""Safely enable and verify CrowdRelay's Rekor anchor, rolling back on failure."""

from __future__ import annotations

import argparse
import json
import os
import secrets
import ssl
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

MAX_RESPONSE_BYTES = 1024 * 1024
FLAG = "external_proof_anchoring_enabled"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", default=os.getenv("CROWDRELAY_PUBLIC_URL", "https://signal-api.virya.music"))
    parser.add_argument(
        "--admin-key-file",
        default=os.getenv("CROWDRELAY_ADMIN_API_KEY_FILE", "deploy/secrets/crowdrelay_admin_api_key"),
    )
    parser.add_argument("--timeout-seconds", type=int, default=420)
    parser.add_argument("--poll-seconds", type=float, default=3.0)
    parser.add_argument("--stalled-seconds", type=int, default=120)
    parser.add_argument("--batch-limit", type=int, default=64)
    return parser.parse_args()


def normalize_base(value: str) -> str:
    url = urllib.parse.urlsplit(value.strip())
    if url.scheme != "https" or not url.netloc or url.username or url.password:
        raise ValueError("base URL must be credential-free HTTPS")
    path = url.path.rstrip("/")
    if path.endswith("/v1"):
        path = path[:-3]
    return urllib.parse.urlunsplit((url.scheme, url.netloc, path, "", "")).rstrip("/")


def read_secret(path: str) -> str:
    value = Path(path).read_text(encoding="utf-8").strip()
    if not 24 <= len(value) <= 512:
        raise ValueError("admin API key length is invalid")
    return value


class Client:
    def __init__(self, base: str, token: str) -> None:
        self.base = base
        self.token = token
        self.ssl = ssl.create_default_context()

    def json(
        self,
        path: str,
        *,
        method: str = "GET",
        body: dict[str, Any] | None = None,
        admin: bool = False,
        idempotency_key: str | None = None,
        timeout: int = 20,
    ) -> Any:
        headers = {"Accept": "application/json"}
        data = None
        if body is not None:
            data = json.dumps(body, separators=(",", ":")).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if admin:
            headers["Authorization"] = f"Bearer {self.token}"
        if idempotency_key:
            headers["Idempotency-Key"] = idempotency_key
        request = urllib.request.Request(f"{self.base}{path}", data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(request, timeout=timeout, context=self.ssl) as response:
                raw = response.read(MAX_RESPONSE_BYTES + 1)
                if len(raw) > MAX_RESPONSE_BYTES:
                    raise RuntimeError("response too large")
                return json.loads(raw or b"null")
        except urllib.error.HTTPError as error:
            raw = error.read(16_384).decode("utf-8", "replace")
            raise RuntimeError(f"HTTP {error.code} for {path}: {raw[:500]}") from error


def set_flag(client: Client, enabled: bool, reason: str, run_id: str) -> None:
    client.json(
        f"/v1/admin/ecosystem/flags/{FLAG}",
        method="POST",
        body={"enabled": enabled, "reason": reason},
        admin=True,
        idempotency_key=f"rekor-canary-{run_id}-flag-{'on' if enabled else 'off'}",
    )


def verify_rekor_entry(anchor_url: str, entry_id: str) -> None:
    parsed = urllib.parse.urlsplit(anchor_url)
    if parsed.scheme != "https" or not parsed.netloc or parsed.username or parsed.password:
        raise RuntimeError("unsafe anchor URL")
    if not (64 <= len(entry_id) <= 128 and all(ch in "0123456789abcdef" for ch in entry_id)):
        raise RuntimeError("invalid Rekor entry ID")
    url = urllib.parse.urlunsplit((parsed.scheme, parsed.netloc, f"{parsed.path.rstrip('/')}/api/v1/log/entries/{entry_id}", "", ""))
    request = urllib.request.Request(url, headers={"Accept": "application/json"}, method="GET")
    with urllib.request.urlopen(request, timeout=20, context=ssl.create_default_context()) as response:
        raw = response.read(MAX_RESPONSE_BYTES + 1)
        if len(raw) > MAX_RESPONSE_BYTES:
            raise RuntimeError("Rekor response too large")
        payload = json.loads(raw)
    if entry_id not in payload:
        raise RuntimeError("Rekor response does not contain the confirmed entry")


def main() -> int:
    args = parse_args()
    if not 15 <= args.timeout_seconds <= 900:
        raise ValueError("timeout must be between 15 and 900 seconds")
    if not 30 <= args.stalled_seconds <= 600:
        raise ValueError("stalled timeout must be between 30 and 600 seconds")
    if not 1 <= args.batch_limit <= 10_000:
        raise ValueError("batch limit outside supported range")
    base = normalize_base(args.base_url)
    token = read_secret(args.admin_key_file)
    client = Client(base, token)
    run_id = f"{int(time.time())}-{secrets.token_hex(5)}"
    enabled = False

    try:
        ready = client.json("/v1/health/ready")
        if not isinstance(ready, dict):
            raise RuntimeError("CrowdRelay readiness payload is invalid")

        set_flag(client, True, "Rekor production canary", run_id)
        enabled = True

        created = client.json(
            "/v1/admin/proofs/audit-batches",
            method="POST",
            body={"limit": args.batch_limit},
            admin=True,
            idempotency_key=f"rekor-canary-{run_id}-batch",
        )
        batch = created.get("batch") if isinstance(created, dict) else None
        if not isinstance(batch, dict) or not isinstance(batch.get("id"), str):
            raise RuntimeError("canary did not create a proof batch")
        batch_id = batch["id"]
        deadline = time.monotonic() + args.timeout_seconds
        last_observation: tuple[Any, ...] | None = None
        last_progress_at = time.monotonic()

        while time.monotonic() < deadline:
            current = client.json(f"/v1/public/proofs/batches/{urllib.parse.quote(batch_id)}")
            status = current.get("status") if isinstance(current, dict) else None
            observation = (
                status,
                current.get("attempts") if isinstance(current, dict) else None,
                current.get("max_attempts") if isinstance(current, dict) else None,
                current.get("last_error_kind") if isinstance(current, dict) else None,
                current.get("available_at") if isinstance(current, dict) else None,
            )
            if observation != last_observation:
                print(
                    "Rekor canary progress: "
                    f"status={observation[0]} attempts={observation[1]}/{observation[2]} "
                    f"error={observation[3]} available_at={observation[4]}",
                    file=sys.stderr,
                )
                last_observation = observation
                last_progress_at = time.monotonic()
            if status == "confirmed":
                if current.get("anchor_kind") != "sigstore.rekor.v1":
                    raise RuntimeError("confirmed batch has an unexpected anchor kind")
                anchor_url = current.get("anchor_url")
                entry_id = current.get("anchor_entry_id")
                fingerprint = current.get("signer_fingerprint")
                if not isinstance(anchor_url, str) or not isinstance(entry_id, str):
                    raise RuntimeError("confirmed batch lacks Rekor coordinates")
                if not isinstance(fingerprint, str) or not fingerprint.startswith("sha256:"):
                    raise RuntimeError("confirmed batch lacks signer fingerprint")
                verify_rekor_entry(anchor_url, entry_id)
                print(json.dumps({
                    "status": "confirmed",
                    "batch_id": batch_id,
                    "entry_id": entry_id,
                    "log_index": current.get("anchor_sequence"),
                    "signer_fingerprint": fingerprint,
                    "flag_enabled": True,
                }, ensure_ascii=False, indent=2))
                return 0
            if status == "failed":
                raise RuntimeError(
                    "proof batch entered failed state: "
                    f"attempts={current.get('attempts')}/{current.get('max_attempts')} "
                    f"error={current.get('last_error_kind')}"
                )
            if status == "dead":
                raise RuntimeError(f"proof batch entered dead state: {current.get('last_error_kind')}")
            if status == "processing" and time.monotonic() - last_progress_at >= args.stalled_seconds:
                raise RuntimeError(
                    "proof batch stalled in processing; "
                    f"last_observation={last_observation}"
                )
            time.sleep(args.poll_seconds)
        raise RuntimeError(
            "timed out waiting for Rekor confirmation; "
            f"last_observation={last_observation}"
        )
    except BaseException as error:
        if enabled:
            try:
                set_flag(client, False, f"Automatic rollback after failed Rekor canary: {type(error).__name__}", run_id)
            except Exception as rollback_error:
                print(f"CRITICAL: failed to disable {FLAG}: {rollback_error}", file=sys.stderr)
        if isinstance(error, KeyboardInterrupt):
            print("Rekor canary interrupted; feature flag rolled back", file=sys.stderr)
            return 130
        print(f"Rekor canary failed; feature flag rolled back: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
