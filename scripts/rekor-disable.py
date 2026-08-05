#!/usr/bin/env python3
"""Emergency kill switch for external proof anchoring."""
from __future__ import annotations
import json, os, secrets, sys, urllib.request
from pathlib import Path

base = os.getenv("CROWDRELAY_PUBLIC_URL", "https://signal-api.virya.music").rstrip("/")
if base.endswith("/v1"): base = base[:-3]
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
        print(response.read().decode())
except Exception as error:
    print(f"Failed to disable Rekor anchoring: {error}", file=sys.stderr)
    raise SystemExit(1)
