#!/usr/bin/env python3
"""Emergency kill switch for external proof anchoring."""
from __future__ import annotations
import json, os, secrets, sys, urllib.request, urllib.parse
from pathlib import Path

base = os.getenv("CROWDRELAY_PUBLIC_URL", "https://signal-api.virya.music").rstrip("/")
if base.endswith("/v1"): base = base[:-3]
parsed_base = urllib.parse.urlsplit(base)
if parsed_base.scheme != "https" or not parsed_base.hostname or parsed_base.username or parsed_base.password:
    print("CROWDRELAY_PUBLIC_URL must be HTTPS without embedded credentials", file=sys.stderr)
    raise SystemExit(2)
MAX_RESPONSE_BYTES = 128 * 1024
key_file = os.getenv("CROWDRELAY_ADMIN_API_KEY_FILE", "deploy/secrets/crowdrelay_admin_api_key")
token = Path(key_file).read_text(encoding="utf-8").strip()
body = json.dumps({"enabled": False, "reason": "Emergency Rekor kill switch"}).encode()
request = urllib.request.Request(
    f"{base}/v1/admin/ecosystem/flags/external_proof_anchoring_enabled",
    data=body,
    headers={
        "Accept": "application/json",
        "Content-Type": "application/json",
        "Authorization": f"Bearer {token}",
        "Idempotency-Key": f"rekor-disable-{secrets.token_hex(12)}",
    },
    method="POST",
)
try:
    with urllib.request.urlopen(request, timeout=20) as response:
        raw = response.read(MAX_RESPONSE_BYTES + 1)
        if len(raw) > MAX_RESPONSE_BYTES:
            raise ValueError("Rekor disable response exceeds size limit")
        print(raw.decode())
except Exception as error:
    print(f"Failed to disable Rekor anchoring: {error}", file=sys.stderr)
    raise SystemExit(1)
