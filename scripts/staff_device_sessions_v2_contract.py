#!/usr/bin/env python3
"""Source-level contract for one-time staff pairing and revocable device sessions."""
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
checks = {
    "migration": (
        ROOT / "migrations/0038_staff_device_sessions.sql",
        [
            "CREATE TABLE staff_pairing_codes",
            "CREATE TABLE staff_device_sessions",
            "staff_device_sessions_active_token_idx",
            "octet_length(token_hash) = 32",
        ],
    ),
    "broker": (
        ROOT / "crates/crowdrelay-api/src/staff_sessions.rs",
        [
            "random_token::<20>()",
            "random_token::<32>()",
            "token_hash(&pairing_code)",
            "FOR UPDATE",
            "used_at = now()",
            "revoked_at = COALESCE(revoked_at, now())",
        ],
    ),
    "auth": (
        ROOT / "crates/crowdrelay-api/src/ticketing.rs",
        [
            "staff_device_sessions",
            "expires_at > now()",
            "record_legacy_static_staff_auth",
        ],
    ),
    "routes": (
        ROOT / "crates/crowdrelay-api/src/routing.rs",
        [
            "/v1/admin/staff/pairing-codes",
            "/v1/staff-pairing/exchange",
            "/v1/admin/staff/sessions",
            "/v1/admin/staff/sessions/{session_id}/revoke",
        ],
    ),
    "meta": (
        ROOT / "crates/crowdrelay-api/src/meta.rs",
        ["SCHEMA_VERSION: u32 = 43", '"staff_device_sessions_v2"'],
    ),
    "metrics": (
        ROOT / "crates/crowdrelay-api/src/lib.rs",
        ["crowdrelay_legacy_static_staff_auth_total"],
    ),
    "openapi": (
        ROOT / "openapi/openapi.yaml",
        [
            "url: https://api.example.com/v1",
            "/admin/staff/pairing-codes:",
            "/staff-pairing/exchange:",
            "/admin/staff/sessions:",
            "/admin/staff/sessions/{session_id}/revoke:",
            "StaffDeviceSessionCredential:",
            "StaffDeviceSessionList:",
        ],
    ),
}
errors = []
for name, (path, markers) in checks.items():
    if not path.exists():
        errors.append(f"{name}: missing {path.relative_to(ROOT)}")
        continue
    text = path.read_text()
    for marker in markers:
        if marker not in text:
            errors.append(f"{name}: missing marker {marker!r}")

# Secrets must never be persisted in plaintext DB columns.
migration = (ROOT / "migrations/0038_staff_device_sessions.sql").read_text()
for forbidden in ("pairing_code text", "bearer_token text"):
    if forbidden in migration.lower():
        errors.append(f"migration persists secret material: {forbidden}")

if errors:
    print("STAFF_DEVICE_SESSIONS_V2=FAIL", file=sys.stderr)
    for error in errors:
        print(f"  - {error}", file=sys.stderr)
    raise SystemExit(1)
print(f"STAFF_DEVICE_SESSIONS_V2=PASS checks={sum(len(v[1]) for v in checks.values())}")
