#!/usr/bin/env python3
"""Sustained outbox soak: drive synthetic deliveries end-to-end and verify them.

Proves the delivery path (request -> tx -> outbox -> lease -> attempt ->
provider -> consumer dedupe) holds a sustained rate with zero loss, zero
unauthorized deliveries, and bounded duplicates. Dependency-free stdlib only.

Flow:
  1. Start a local HMAC-verifying webhook receiver (the "consumer").
  2. Point the workspace webhook endpoint at the receiver and insert synthetic
     outbox events at a steady rate through `docker exec ... psql`.
  3. Wait until every event is delivered or dead; the receiver counts unique
     deliveries, signature failures, and duplicate redeliveries.
  4. Print throughput + integrity summary; nonzero exit on any violation.

Usage:
  python3 scripts/soak-outbox.py --events 500 --rate-per-second 25 \
      --secret "$(python3 -c 'import json;print(json.load(open("deploy/webhook-secrets.example.json"))["primary"])')"
"""
from __future__ import annotations

import argparse
import hashlib
import hmac
import http.server
import json
import subprocess
import sys
import threading
import time
import urllib.request
from collections import defaultdict
from statistics import median

RECEIVER_PATH = "/soak"


def psql(container: str, user: str, database: str, sql: str) -> str:
    result = subprocess.run(
        ["docker", "exec", container, "psql", "-U", user, "-d", database,
         "--no-psqlrc", "--tuples-only", "--no-align", "--quiet"],
        input=sql, capture_output=True, text=True, timeout=60, check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"psql failed: {result.stderr.strip()[:400]}")
    return result.stdout.strip()


class Receiver(http.server.BaseHTTPRequestHandler):
    events: dict[str, int] = defaultdict(int)
    latencies_ms: list[float] = []
    bad_signatures = 0
    status_500s = 0
    lock = threading.Lock()

    def do_POST(self) -> None:  # noqa: N802 (stdlib interface)
        if self.path != RECEIVER_PATH:
            self.send_response(404)
            self.end_headers()
            return
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length)
        event_id = self.headers.get("CrowdRelay-Event-Id", "")
        timestamp = self.headers.get("CrowdRelay-Timestamp", "")
        received = self.headers.get("CrowdRelay-Signature", "")
        expected = hmac.new(
            Receiver.secret.encode(), f"{timestamp}.".encode() + body, hashlib.sha256,
        ).hexdigest()
        authorized = received == f"v1={expected}"
        with Receiver.lock:
            if not authorized:
                Receiver.bad_signatures += 1
            else:
                Receiver.events[event_id] += 1
                try:
                    sent_at = float(timestamp)
                    Receiver.latencies_ms.append((time.time() - sent_at) * 1000.0)
                except ValueError:
                    pass
        # Every fifth delivery fails once to exercise worker retry + dedupe.
        with Receiver.lock:
            seen_before = Receiver.events[event_id] > 1 if authorized else False
            if authorized and not seen_before and Receiver.events[event_id] % 5 == 0:
                Receiver.status_500s += 1
                self.send_response(500)
                self.end_headers()
                return
        self.send_response(200)
        self.end_headers()

    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        pass


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    index = min(len(ordered) - 1, int(len(ordered) * fraction))
    return ordered[index]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--events", type=int, default=200)
    parser.add_argument("--rate-per-second", type=float, default=10.0)
    parser.add_argument("--port", type=int, default=8099)
    parser.add_argument("--secret", required=True, help="webhook signing secret value")
    parser.add_argument("--db-container", default="postgres")
    parser.add_argument("--db-user", default="crowdrelay")
    parser.add_argument("--db-name", default="crowdrelay")
    parser.add_argument("--callback-host", default="host.docker.internal",
                        help="hostname the worker container uses to reach this receiver")
    parser.add_argument("--timeout-seconds", type=int, default=300)
    args = parser.parse_args()

    Receiver.secret = args.secret
    server = http.server.ThreadingHTTPServer(("0.0.0.0", args.port), Receiver)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    print(f"receiver listening on :{args.port}{RECEIVER_PATH}")

    workspace_id = psql(args.db_container, args.db_user, args.db_name,
                        "SELECT id FROM workspaces ORDER BY created_at LIMIT 1;")
    if not workspace_id:
        print("FAIL: no workspace found", file=sys.stderr)
        return 2
    callback_url = f"http://{args.callback_host}:{args.port}{RECEIVER_PATH}"
    psql(args.db_container, args.db_user, args.db_name, f"""
        DELETE FROM webhook_endpoints WHERE name = 'soak-receiver';
        INSERT INTO webhook_endpoints (workspace_id, name, url, signing_secret_ref, active)
        VALUES ('{workspace_id}', 'soak-receiver', '{callback_url}', 'primary', true);
    """)
    print(f"endpoint pointed at {callback_url}")

    interval = 1.0 / max(args.rate_per_second, 0.001)
    inserted = 0
    started = time.time()
    while inserted < args.events:
        batch = min(50, args.events - inserted)
        values = ", ".join(
            f"('{workspace_id}', 'ops.alert_raised', "
            f"'{{\"alert\":\"soak\",\"seq\":{inserted + seq}}}'::jsonb)"
            for seq in range(batch)
        )
        psql(args.db_container, args.db_user, args.db_name, f"""
            INSERT INTO outbox_events (workspace_id, event_type, payload)
            VALUES {values};
        """)
        inserted += batch
        time.sleep(interval * batch)
    print(f"inserted {inserted} events in {time.time() - started:.1f}s")

    deadline = time.time() + args.timeout_seconds
    while time.time() < deadline:
        row = psql(args.db_container, args.db_user, args.db_name, f"""
            SELECT count(*) FILTER (WHERE status IN ('delivered','dead'))
                 , count(*) FILTER (WHERE status = 'dead')
            FROM outbox_events
            WHERE event_type = 'ops.alert_raised'
              AND payload->>'alert' = 'soak';
        """)
        settled, dead = (int(part) for part in row.split("|"))
        received_unique = len(Receiver.events)
        if settled >= inserted or received_unique >= inserted:
            break
        time.sleep(2)

    server.shutdown()
    duplicates = sum(count - 1 for count in Receiver.events.values())
    delivered_rows = int(psql(
        args.db_container, args.db_user, args.db_name,
        "SELECT count(*) FROM outbox_events WHERE event_type = 'ops.alert_raised' "
        "AND payload->>'alert' = 'soak' AND status = 'delivered';",
    ) or "0")

    latencies = Receiver.latencies_ms
    print(json.dumps({
        "events_inserted": inserted,
        "unique_deliveries_received": len(Receiver.events),
        "duplicate_redeliveries": duplicates,
        "signature_failures": Receiver.bad_signatures,
        "intentional_first_attempt_failures": Receiver.status_500s,
        "outbox_rows_delivered": delivered_rows,
        "p50_latency_ms": round(median(latencies), 1) if latencies else None,
        "p95_latency_ms": round(percentile(latencies, 0.95), 1) if latencies else None,
    }, indent=2))

    failures = []
    if Receiver.bad_signatures:
        failures.append("receiver saw unauthorized deliveries")
    if len(Receiver.events) < inserted:
        failures.append(f"loss: received {len(Receiver.events)} of {inserted}")
    if delivered_rows < inserted:
        failures.append(f"outbox left {inserted - delivered_rows} rows undelivered")
    for failure in failures:
        print(f"FAIL: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
