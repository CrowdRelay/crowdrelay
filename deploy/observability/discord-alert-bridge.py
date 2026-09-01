#!/usr/bin/env python3
"""Translate Alertmanager webhook payloads into Discord messages a human can act on.

The production smoke probe already posts {"content": ...} to the same
DISCORD_WEBHOOK_URL, so operators keep exactly one notification channel.
Stdlib only; no secrets are logged. Fails the request (500) when forwarding
fails so Alertmanager retries per its own backoff.

This used to render `AlertName (severity): summary` and nothing else, so the
`remedy` written on every rule never left the repository. Someone paged at
midnight got a symptom and a guess, and the guess is usually "redeploy" — the
most intrusive fix, applied to problems a single retry would have solved.

The message is written for whoever is holding the phone, not for the person who
wrote the alert. That means: what broke, what it means for the band, and a
numbered list starting with the cheapest thing that might fix it.
"""

from __future__ import annotations

import json
import os
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

WEBHOOK_URL = os.environ["DISCORD_WEBHOOK_URL"]
MAX_CONTENT_CHARS = 1900

# Discord renders these; they carry the status faster than the word does.
STATUS_MARK = {"FIRING": "🔴", "RESOLVED": "🟢"}


def render_alert(alert: dict) -> str:
    labels = alert.get("labels", {})
    annotations = alert.get("annotations", {})
    name = labels.get("alertname", "UnknownAlert")
    severity = labels.get("severity", "info")
    status = alert.get("status", "firing").upper()
    mark = STATUS_MARK.get(status, "🔸")

    headline = annotations.get("headline") or annotations.get("summary") or name

    # Resolved alerts need the fact, not the runbook — and the headline is
    # phrased for the failure, so reusing it verbatim reads as though the
    # outage is still on ("🟢 The growth engine has stopped").
    if status == "RESOLVED":
        return "\n".join(
            [f"{mark} **Recovered:** {headline}", f"`{name}` · {severity}"]
        )

    parts = [f"{mark} **{headline}**", f"`{name}` · {severity} · {status.lower()}"]

    impact = annotations.get("impact", "").strip()
    if impact:
        parts.append(f"\n**What this means:** {impact}")

    remedy = annotations.get("remedy", "").strip()
    if remedy:
        # Remedies are authored as "1. … 2. … 3. …" on one line so the YAML
        # stays readable. Break them onto their own lines here, because a
        # numbered list is the whole point of writing them.
        steps = remedy.replace(" 1. ", "\n1. ")
        for number in range(2, 8):
            steps = steps.replace(f" {number}. ", f"\n{number}. ")
        parts.append(f"\n**What to do:**\n{steps.strip()}")

    return "\n".join(parts)


def render(alerts: list[dict]) -> str:
    blocks: list[str] = []
    used = 0
    for alert in alerts:
        block = render_alert(alert)
        # Keep whole alerts rather than truncating one mid-instruction: half a
        # remedy is worse than a missing one.
        if used + len(block) > MAX_CONTENT_CHARS:
            remaining = len(alerts) - len(blocks)
            if remaining > 0:
                blocks.append(f"…and {remaining} more alert(s).")
            break
        blocks.append(block)
        used += len(block) + 2
    return "\n\n".join(blocks)[:MAX_CONTENT_CHARS]


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
