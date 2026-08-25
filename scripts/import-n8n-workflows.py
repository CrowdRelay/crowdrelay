#!/usr/bin/env python3
"""Import CrowdRelay executor workflows into a self-hosted n8n instance.

The repo ships provider-neutral examples under n8n/examples/. Production
copies live under n8n/private-production-exports/ (gitignored) and carry
credential *references* — names and slot markers, never secrets. Secrets
stay inside n8n's own store; this script only matches slots to credentials
already defined there.

Usage:
    # 1. Transform an example into a deployable copy (offline):
    python3 scripts/import-n8n-workflows.py prepare \\
        n8n/examples/autopilot-beacon-invite-batch.example.json

    # 2. Push to virya-home n8n (creates or updates by workflow name):
    N8N_BASE_URL=https://n8n.virya.music N8N_API_KEY=... \\
        python3 scripts/import-n8n-workflows.py push n8n/private-production-exports/*.json

Credential slots marked id="REPLACE_*" are auto-bound when exactly one
credential of the required type exists on the instance (e.g. gmailOAuth2Api);
otherwise the slot is left for one manual pick in the dashboard.
"""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

COMMERCE_CREDENTIAL_NAME = "Virya CrowdRelay commerce API"
PLACEHOLDER_PREFIX = "REPLACE_"
REPLACE_PREFIX = PLACEHOLDER_PREFIX


def api(base_url: str, api_key: str, method: str, path: str, body: dict | None = None) -> dict:
    request = Request(
        base_url.rstrip("/") + "/api/v1" + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={
            "X-N8N-API-KEY": api_key,
            "content-type": "application/json",
        },
    )
    try:
        with urlopen(request, timeout=30) as response:
            payload = json.load(response)
    except HTTPError as error:
        detail = error.read().decode(errors="replace")[:500]
        raise SystemExit(f"n8n API {method} {path} failed: {error.code} {detail}")
    except URLError as error:
        raise SystemExit(f"n8n API unreachable at {base_url}: {error}")
    return payload if isinstance(payload, dict) else {"data": payload}


def bind_credential_slots(
    workflow: dict, credentials_by_type: dict[str, list[dict]]
) -> list[str]:
    """Fill REPLACE_* credential slots by type when unambiguous."""
    unbound: list[str] = []
    for node in workflow.get("nodes", []):
        for credential_type, slot in (node.get("credentials") or {}).items():
            candidates = credentials_by_type.get(credential_type, [])
            marker = slot.get("id")
            if marker == COMMERCE_CREDENTIAL_NAME:
                continue
            if isinstance(marker, str) and marker.startswith(PLACEHOLDER_PREFIX):
                if len(candidates) == 1:
                    only = candidates[0]
                    slot["id"] = only["id"]
                    slot["name"] = only["name"]
                else:
                    unbound.append(f"{node.get('name')}: {credential_type}")
                    slot.setdefault("name", credential_type)
    return unbound


# Credential types each executor node family needs in production copies.
# Public examples carry none of this: deployment metadata stays out of the
# repo (audit-public-tree), and secrets stay inside the n8n instance.
NODE_CREDENTIAL_TYPES = {
    "n8n-nodes-base.gmail": "gmailOAuth2Api",
}

COMMERCE_HEADER_NAME = "Virya CrowdRelay commerce API"


def command_prepare(files: list[Path]) -> None:
    out_dir = Path("n8n/private-production-exports")
    out_dir.mkdir(parents=True, exist_ok=True)
    for file in files:
        workflow = json.loads(file.read_text())
        for node in workflow.get("nodes", []):
            # Executor routes authenticate with the workspace commerce header.
            url = str(node.get("parameters", {}).get("url", ""))
            if node.get("type", "").endswith("httpRequest") and "/v1/internal/" in url:
                headers = (
                    node["parameters"]
                    .setdefault("headerParameters", {})
                    .setdefault("parameters", [])
                )
                if not any(h.get("name") == "Authorization" for h in headers):
                    continue
                node.setdefault("credentials", {})[
                    "httpHeaderAuth"
                ] = {"id": COMMERCE_CREDENTIAL_NAME, "name": COMMERCE_CREDENTIAL_NAME}
            credential_type = NODE_CREDENTIAL_TYPES.get(node.get("type", ""))
            if credential_type:
                node.setdefault("credentials", {})[credential_type] = {
                    "id": REPLACE_PREFIX + credential_type,
                    "name": credential_type,
                }
        destination = out_dir / file.name.replace(".example.", ".")
        destination.write_text(json.dumps(workflow, indent=2))
        print(f"prepared {destination}")


def command_push(files: list[Path]) -> None:
    base_url = os.environ.get("N8N_BASE_URL", "")
    api_key = os.environ.get("N8N_API_KEY", "")
    if not base_url or not api_key:
        raise SystemExit("set N8N_BASE_URL and N8N_API_KEY")
    listed = api(base_url, api_key, "GET", "/workflows?limit=250")
    by_name = {
        item["name"]: item["id"]
        for item in listed.get("data", [])
        if item.get("name")
    }
    listed_credentials = api(base_url, api_key, "GET", "/credentials?limit=250")
    credentials_by_type: dict[str, list[dict]] = {}
    for credential in listed_credentials.get("data", []):
        credentials_by_type.setdefault(credential.get("type"), []).append(credential)

    failures: list[str] = []
    for file in files:
        workflow = json.loads(file.read_text())
        unbound = bind_credential_slots(workflow, credentials_by_type)
        name = workflow["name"]
        payload = {
            "name": name,
            "nodes": workflow.get("nodes", []),
            "connections": workflow.get("connections", {}),
            "settings": workflow.get("settings", {}),
        }
        existing_id = by_name.get(name)
        method = "PUT" if existing_id else "POST"
        path = f"/workflows/{existing_id}" if existing_id else "/workflows"
        api(base_url, api_key, method, path, payload)
        state = "updated" if existing_id else "created"
        print(f"{state}: {name} ({file.name})")
        if unbound:
            for entry in unbound:
                failures.append(f"{file.name}: unbound credential -> {entry}")

    if failures:
        print("\nFinish these in the n8n dashboard (pick the credential once):")
        for failure in failures:
            print(f"  - {failure}")
    print("\nWorkflows stay INACTIVE after import. Activate them from the "
          "dashboard only after their smoke check passes.")


def main() -> None:
    args = sys.argv[1:]
    if not args or args[0] in ("-h", "--help", "help"):
        print("Usage:")
        print("  python3 scripts/import-n8n-workflows.py prepare [files...]   # transform examples -> deployable copies")
        print("  python3 scripts/import-n8n-workflows.py push <files...>      # push deployable copies to n8n")
        print()
        print("  prepare (no files) processes ALL n8n/examples/*.example.json")
        print("  push requires N8N_BASE_URL and N8N_API_KEY environment variables")
        sys.exit(0)

    command = args[0]
    files = args[1:]

    if command == "prepare":
        paths = [Path(f) for f in files] if files else sorted((Path("n8n/examples")).glob("*.example.json"))
        command_prepare(paths)
    elif command == "push":
        paths = [Path(f) for f in files]
        if not paths:
            # Default: push everything in private-production-exports
            export_dir = Path("n8n/private-production-exports")
            paths = sorted(export_dir.glob("*.json"))
            if not paths:
                raise SystemExit(f"No JSON files found in {export_dir}. Run 'prepare' first.")
        command_push(paths)
    else:
        raise SystemExit(f"Unknown command: {command}. Use 'prepare' or 'push'.")


if __name__ == "__main__":
    main()
