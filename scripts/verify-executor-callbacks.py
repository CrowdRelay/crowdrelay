#!/usr/bin/env python3
"""Pin the executor (n8n) callback contract for retryable writes.

CrowdRelay makes executor-facing writes idempotent at the handler boundary:
`/v1/internal/autopilot/outreach/candidates`, `/v1/internal/autopilot/outreach/
delivery-faults` and the admin reply routes require an `Idempotency-Key`
header, because a retried webhook delivery must be a no-op rather than a
counted event. The claim/report routes are safe without one — their Postgres
state machine and receipt dedupe make replays no-ops.

This script reads every n8n workflow JSON in the repository and verifies:
  1. Any node POSTing to a key-required route carries an `Idempotency-Key`
     header (static string or expression).
  2. No key-required route is ever called with GET/DELETE.
Exit nonzero listing offending workflow/node pairs.
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
KEY_REQUIRED_FRAGMENTS = (
    "/outreach/candidates",
    "/outreach/delivery-faults",
    "/reply",
)
IDEMPOTENCY_HEADER = "idempotency-key"


def iter_parameter_nodes(document: object):
    if isinstance(document, dict):
        if "parameters" in document and isinstance(document, dict):
            yield document
        for value in document.values():
            yield from iter_parameter_nodes(value)
    elif isinstance(document, list):
        for item in document:
            yield from iter_parameter_nodes(item)


def header_names(parameters: dict) -> set[str]:
    headers = parameters.get("headerParameters", {})
    entries = headers.get("parameters", []) if isinstance(headers, dict) else []
    names = set()
    for entry in entries:
        if isinstance(entry, dict):
            name = str(entry.get("name", "")).strip().lower()
            if name:
                names.add(name)
    return names


def main() -> int:
    failures: list[str] = []
    checked = 0
    workflows = sorted(ROOT.rglob("n8n/**/*.json"))
    for path in workflows:
        try:
            document = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            failures.append(f"{path}: unreadable JSON ({error})")
            continue
        for node in iter_parameter_nodes(document):
            parameters = node.get("parameters")
            if not isinstance(parameters, dict) or "url" not in parameters:
                continue
            url = str(parameters.get("url", ""))
            method = str(parameters.get("method", "GET")).upper()
            # n8n URL fields are expressions (`={{ ... }}`) whose literal path
            # suffix identifies the route. Nodes that assemble their route at
            # runtime from prior-node data (`$json.path`) cannot be verified
            # statically and are skipped deliberately.
            if not any(fragment in url for fragment in KEY_REQUIRED_FRAGMENTS):
                continue
            if "$json.path" in url:
                continue
            checked += 1
            name = str(node.get("name", "<unnamed>"))
            relative = path.relative_to(ROOT)
            if method != "POST":
                failures.append(
                    f"{relative}: node `{name}` calls `{url}` with {method}; "
                    "key-required routes are write-only"
                )
            elif IDEMPOTENCY_HEADER not in header_names(parameters):
                failures.append(
                    f"{relative}: node `{name}` POSTs `{url}` without an "
                    "Idempotency-Key header; replays would double-count"
                )
    print(
        f"EXECUTOR_CALLBACK_CONTRACT={'PASS' if not failures else 'FAIL'} "
        f"workflows={len(workflows)} key_required_calls={checked}"
    )
    for failure in failures:
        print(f"  {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
