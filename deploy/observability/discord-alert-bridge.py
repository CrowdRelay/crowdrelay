#!/usr/bin/env python3
"""Translate Alertmanager webhook payloads into Discord `content` messages.

The production smoke probe already posts {"content": ...} to the same
DISCORD_WEBHOOK_URL, so operators keep exactly one notification channel.
Stdlib only; no secrets are logged. Fails the request (500) when forwarding
fails so Alertmanager retries per its own backoff.
"""

from __future__ import annotations

import json
import os
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

WEBHOOK_URL = os.environ["DISCORD_WEBHOOK_URL"]
MAX_CONTENT_CHARS = 1900


def render(alerts: list[dict]) -> str:
    lines: list[str] = []
    for alert in alerts:
        labels = alert.get("labels", {})
        name = labels.get("alertname", "UnknownAlert")
        status = alert.get("status", "firing").upper()
        summary = alert.get("annotations", {}).get("summary", "")
        severity = labels.get("severity", "info")
        prefix = "[RESOLVED] " if status == "RESOLVED" else ""
        line = f"{prefix}{name} ({severity}): {summary}".strip()
        lines.append(line[: MAX_CONTENT_CHARS // 2])
        if len(lines) * 2 >= MAX_CONTENT_CHARS:
            break
    return "\n".join(lines)[:MAX_CONTENT_CHARS]


class Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:  # noqa: N802 (stdlib interface)
        if self.path != "/alert":
            self.send_response(404)
            self.end_headers()
            return
        length = int(self.headers.get("content-length", "0"))
        if not 0 < length <= 1_048_576:
            self.send_response(413)
            self.end_headers()
            return
        payload = json.loads(self.rfile.read(length))
        content = render(payload.get("alerts", []))
        if not content:
            self.send_response(204)
            self.end_headers()
            return
        request = urllib.request.Request(
            WEBHOOK_URL,
            data=json.dumps({"content": content}).encode(),
            headers={"content-type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                status = response.status
        except OSError:
            status = 502
        self.send_response(status if status < 400 else 502)
        self.end_headers()

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        pass


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", 9880), Handler).serve_forever()
