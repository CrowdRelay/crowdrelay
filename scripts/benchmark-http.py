#!/usr/bin/env python3
"""Small dependency-free concurrent HTTP benchmark for repeatable CrowdRelay smoke loads."""
from __future__ import annotations

import argparse
import concurrent.futures
import json
import math
import statistics
import time
import urllib.error
import urllib.request
from dataclasses import asdict, dataclass
from pathlib import Path

MAX_RESPONSE_BYTES = 1024 * 1024


def drain_bounded(response) -> None:
    body = response.read(MAX_RESPONSE_BYTES + 1)
    if len(body) > MAX_RESPONSE_BYTES:
        raise ValueError("benchmark response exceeds size limit")


@dataclass
class Sample:
    path: str
    elapsed_ms: float
    status: int


def percentile(values: list[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    rank = max(0, min(len(ordered) - 1, math.ceil((pct / 100.0) * len(ordered)) - 1))
    return ordered[rank]


def fetch(url: str, path: str, timeout: float) -> Sample:
    started = time.perf_counter()
    status = 0
    try:
        request = urllib.request.Request(url.rstrip("/") + path, headers={"User-Agent": "crowdrelay-perf-harness/1"})
        with urllib.request.urlopen(request, timeout=timeout) as response:
            status = response.status
            drain_bounded(response)
    except urllib.error.HTTPError as error:
        status = error.code
        drain_bounded(error)
    except Exception:
        status = 0
    return Sample(path=path, elapsed_ms=(time.perf_counter() - started) * 1000.0, status=status)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", default="http://127.0.0.1:8080")
    parser.add_argument("--path", action="append", dest="paths")
    parser.add_argument("--requests", type=int, default=600)
    parser.add_argument("--concurrency", type=int, default=24)
    parser.add_argument("--timeout-seconds", type=float, default=5.0)
    parser.add_argument("--warmup", type=int, default=30)
    parser.add_argument("--max-errors", type=int, default=0)
    parser.add_argument("--gross-p95-ms", type=float, default=1500.0)
    parser.add_argument("--output", type=Path, default=Path("artifacts/http-benchmark.json"))
    args = parser.parse_args()
    paths = args.paths or ["/health/live", "/v1/public/cities", "/v1/public/events", "/v1/meta"]
    if args.requests <= 0 or args.concurrency <= 0 or not paths:
        parser.error("requests/concurrency/paths must be positive")

    # Warm the application/database/cache before measuring.
    for index in range(args.warmup):
        fetch(args.base_url, paths[index % len(paths)], args.timeout_seconds)

    scheduled = [paths[index % len(paths)] for index in range(args.requests)]
    wall_started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as executor:
        futures = [executor.submit(fetch, args.base_url, path, args.timeout_seconds) for path in scheduled]
        samples = [future.result() for future in futures]
    wall_seconds = time.perf_counter() - wall_started

    latencies = [sample.elapsed_ms for sample in samples]
    errors = [sample for sample in samples if sample.status < 200 or sample.status >= 400]
    report = {
        "schema": 1,
        "base_url": args.base_url,
        "requests": len(samples),
        "concurrency": args.concurrency,
        "wall_seconds": wall_seconds,
        "requests_per_second": len(samples) / wall_seconds if wall_seconds else 0.0,
        "errors": len(errors),
        "latency_ms": {
            "min": min(latencies) if latencies else 0.0,
            "mean": statistics.fmean(latencies) if latencies else 0.0,
            "p50": percentile(latencies, 50),
            "p95": percentile(latencies, 95),
            "p99": percentile(latencies, 99),
            "max": max(latencies) if latencies else 0.0,
        },
        "by_path": {},
    }
    for path in paths:
        selected = [sample.elapsed_ms for sample in samples if sample.path == path]
        report["by_path"][path] = {
            "requests": len(selected),
            "p50_ms": percentile(selected, 50),
            "p95_ms": percentile(selected, 95),
            "p99_ms": percentile(selected, 99),
        }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(
        "HTTP_BENCHMARK "
        f"requests={report['requests']} rps={report['requests_per_second']:.1f} "
        f"p50={report['latency_ms']['p50']:.1f}ms p95={report['latency_ms']['p95']:.1f}ms "
        f"p99={report['latency_ms']['p99']:.1f}ms errors={report['errors']}"
    )
    for path, row in report["by_path"].items():
        print(f"  {path}: p95={row['p95_ms']:.1f}ms p99={row['p99_ms']:.1f}ms n={row['requests']}")
    if len(errors) > args.max_errors:
        print(f"HTTP_BENCHMARK=FAIL errors={len(errors)} max_errors={args.max_errors}")
        return 1
    # This is intentionally only a gross runaway guard on shared CI hardware.
    # The JSON artifact is the source for trend comparisons; do not treat this
    # value as an SLO or stable microbenchmark threshold.
    if report["latency_ms"]["p95"] > args.gross_p95_ms:
        print(f"HTTP_BENCHMARK=FAIL gross_p95_ms={args.gross_p95_ms}")
        return 1
    print("HTTP_BENCHMARK=PASS mode=trend-report+gross-runaway-guard")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
