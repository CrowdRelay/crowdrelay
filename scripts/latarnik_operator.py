#!/usr/bin/env python3
"""CrowdRelay Signal Latarnik operator client.

This intentionally uses only the Python standard library. Invitation responses contain
single-use capabilities; when written to disk, this tool creates the output with mode
0600. It never stores invitation capabilities in CrowdRelay outbox events.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

DEFAULT_API_BASE = "https://signal-api.virya.music/v1"


class OperatorError(RuntimeError):
    pass


def env(name: str, *, required: bool = True) -> str:
    value = os.environ.get(name, "").strip()
    if required and not value:
        raise OperatorError(f"missing environment variable: {name}")
    return value


def api_base() -> str:
    return os.environ.get("CROWDRELAY_API_BASE", DEFAULT_API_BASE).strip().rstrip("/")


def request_json(
    method: str,
    path: str,
    *,
    bearer: str,
    payload: dict[str, Any] | None = None,
) -> Any:
    body = None if payload is None else json.dumps(payload, separators=(",", ":")).encode()
    headers = {
        "Accept": "application/json",
        "Authorization": f"Bearer {bearer}",
        "User-Agent": "crowdrelay-latarnik-operator/1",
    }
    if body is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(
        f"{api_base()}/{path.lstrip('/')}",
        data=body,
        headers=headers,
        method=method,
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as response:
            raw = response.read(2_000_000)
    except urllib.error.HTTPError as error:
        detail = error.read(16_384).decode("utf-8", "replace")
        raise OperatorError(f"HTTP {error.code} {path}: {detail}") from error
    except urllib.error.URLError as error:
        raise OperatorError(f"request failed for {path}: {error.reason}") from error
    if not raw:
        return None
    try:
        return json.loads(raw)
    except json.JSONDecodeError as error:
        raise OperatorError(f"non-JSON response from {path}") from error


def admin_request(method: str, path: str, payload: dict[str, Any] | None = None) -> Any:
    return request_json(method, path, bearer=env("CROWDRELAY_ADMIN_API_KEY"), payload=payload)


def internal_request(method: str, path: str, payload: dict[str, Any] | None = None) -> Any:
    return request_json(method, path, bearer=env("CROWDRELAY_COMMERCE_API_KEY"), payload=payload)


def secure_dump(value: Any, output: str | None) -> None:
    encoded = (json.dumps(value, indent=2, ensure_ascii=False) + "\n").encode()
    if not output or output == "-":
        sys.stdout.buffer.write(encoded)
        return
    path = Path(output).expanduser()
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(encoded)
    finally:
        try:
            os.chmod(path, 0o600)
        except OSError:
            pass
    print(f"wrote sensitive Latarnik output to {path} (mode 0600)", file=sys.stderr)


def bounded_int(minimum: int, maximum: int):
    def parse(value: str) -> int:
        try:
            parsed = int(value)
        except ValueError as error:
            raise argparse.ArgumentTypeError("must be an integer") from error
        if not minimum <= parsed <= maximum:
            raise argparse.ArgumentTypeError(f"must be between {minimum} and {maximum}")
        return parsed

    return parse


def candidates(_: argparse.Namespace) -> Any:
    return admin_request("GET", "admin/autopilot/beacon-signal/candidates")


def dashboard(_: argparse.Namespace) -> Any:
    return admin_request("GET", "admin/autopilot/beacon-signal")


def invite_batch(args: argparse.Namespace) -> Any:
    beacon_ids = list(dict.fromkeys(args.beacon_id or []))
    if args.top:
        response = candidates(args)
        ranked = response.get("candidates", []) if isinstance(response, dict) else []
        beacon_ids.extend(
            row.get("beaconId") for row in ranked[: args.top] if isinstance(row, dict) and row.get("beaconId")
        )
        beacon_ids = list(dict.fromkeys(beacon_ids))
    if not beacon_ids:
        raise OperatorError("invite-batch needs --beacon-id or --top")
    if len(beacon_ids) > 200:
        raise OperatorError("invite batch is hard-limited to 200 contacts")
    return admin_request(
        "POST",
        "admin/autopilot/beacons/signal-invites/batch",
        {
            "beaconIds": beacon_ids,
            "ttlDays": args.ttl_days,
            "radiusKm": args.radius_km,
            "locale": args.locale,
        },
    )


def press_requests(_: argparse.Namespace) -> Any:
    return admin_request("GET", "admin/autopilot/beacon-press-requests")


def engagements(_: argparse.Namespace) -> Any:
    return admin_request("GET", "admin/autopilot/beacon-signal-engagements")


def coverage(_: argparse.Namespace) -> Any:
    return admin_request("GET", "admin/autopilot/beacon-coverage")


def resolve_request(args: argparse.Namespace) -> Any:
    payload: dict[str, Any] = {"status": args.status, "resolutionNote": args.note}
    return admin_request(
        "POST",
        f"admin/autopilot/beacon-press-requests/{args.request_id}/resolve",
        payload,
    )


def set_state(args: argparse.Namespace) -> Any:
    return admin_request(
        "POST",
        f"admin/autopilot/beacons/{args.beacon_id}/signal-state",
        {"status": args.status},
    )


def emit_wave(args: argparse.Namespace) -> Any:
    return internal_request(
        "POST",
        "internal/beacon/notifications/emit-due",
        {"limit": args.limit, "leadDays": args.lead_days},
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Operate CrowdRelay Signal Latarnik")
    parser.add_argument("--output", help="JSON output path; invite files are created as 0600 (use - for stdout)")
    sub = parser.add_subparsers(dest="command", required=True)

    command = sub.add_parser("candidates", help="ranked verified Beacons eligible for invitation")
    command.set_defaults(handler=candidates)

    command = sub.add_parser("dashboard", help="Signal Latarnik operational dashboard")
    command.set_defaults(handler=dashboard)

    command = sub.add_parser("invite-batch", help="mint a bounded batch of one-time invitations")
    command.add_argument("--beacon-id", action="append", default=[], help="Beacon UUID; repeat as needed")
    command.add_argument("--top", type=bounded_int(1, 200), metavar="N", help="also invite top N ranked candidates")
    command.add_argument("--ttl-days", type=bounded_int(1, 30), default=14)
    command.add_argument("--radius-km", type=bounded_int(10, 500), default=100)
    command.add_argument("--locale", choices=("pl", "en"), default="pl")
    command.set_defaults(handler=invite_batch)

    command = sub.add_parser("press-requests", help="list Latarnik press requests")
    command.set_defaults(handler=press_requests)

    command = sub.add_parser("engagements", help="list Beacon×event lifecycle records")
    command.set_defaults(handler=engagements)

    command = sub.add_parser("coverage", help="list submitted coverage")
    command.set_defaults(handler=coverage)

    command = sub.add_parser("resolve-request", help="resolve or cancel one press request")
    command.add_argument("request_id")
    command.add_argument("--status", choices=("resolved", "cancelled"), required=True)
    command.add_argument("--note")
    command.set_defaults(handler=resolve_request)

    command = sub.add_parser("state", help="activate, pause or revoke one Latarnik profile")
    command.add_argument("beacon_id")
    command.add_argument("--status", choices=("active", "paused", "revoked"), required=True)
    command.set_defaults(handler=set_state)

    command = sub.add_parser("emit-wave", help="emit one bounded nearby-show push wave")
    command.add_argument("--limit", type=bounded_int(1, 100), default=20)
    command.add_argument("--lead-days", type=bounded_int(1, 180), default=60)
    command.set_defaults(handler=emit_wave)
    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    try:
        result = args.handler(args)
        secure_dump(result, args.output)
    except OperatorError as error:
        print(f"LATARNIK_OPERATOR=FAIL {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
