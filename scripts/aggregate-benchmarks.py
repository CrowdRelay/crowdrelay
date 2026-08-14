#!/usr/bin/env python3
"""Aggregate repeated CrowdRelay HTTP benchmark runs with medians.

The raw harness deliberately stays dependency-free and produces one report per run.
This helper reduces runner noise before a historical comparison is made.
"""
from __future__ import annotations

import argparse
import json
import statistics
from pathlib import Path
from typing import Any


def median(values: list[float]) -> float:
    if not values:
        raise ValueError("cannot aggregate an empty metric set")
    return float(statistics.median(values))


def load_report(path: Path) -> dict[str, Any]:
    report = json.loads(path.read_text())
    if report.get("schema") != 1:
        raise ValueError(f"{path}: unsupported benchmark schema {report.get('schema')!r}")
    if int(report.get("errors", 0)) != 0:
        raise ValueError(f"{path}: benchmark contains {report.get('errors')} errors")
    return report


def aggregate(reports: list[dict[str, Any]]) -> dict[str, Any]:
    if len(reports) < 3:
        raise ValueError("at least three benchmark runs are required")
    requests = {int(report["requests"]) for report in reports}
    concurrency = {int(report["concurrency"]) for report in reports}
    path_sets = {tuple(sorted(report["by_path"].keys())) for report in reports}
    if len(requests) != 1 or len(concurrency) != 1 or len(path_sets) != 1:
        raise ValueError("benchmark runs do not share the same workload")

    latency_keys = ("min", "mean", "p50", "p95", "p99", "max")
    paths = sorted(reports[0]["by_path"].keys())
    return {
        "schema": 2,
        "aggregation": "median",
        "sample_runs": len(reports),
        "requests_per_run": requests.pop(),
        "concurrency": concurrency.pop(),
        "requests_per_second": median(
            [float(report["requests_per_second"]) for report in reports]
        ),
        "latency_ms": {
            key: median([float(report["latency_ms"][key]) for report in reports])
            for key in latency_keys
        },
        "by_path": {
            path: {
                "p50_ms": median(
                    [float(report["by_path"][path]["p50_ms"]) for report in reports]
                ),
                "p95_ms": median(
                    [float(report["by_path"][path]["p95_ms"]) for report in reports]
                ),
                "p99_ms": median(
                    [float(report["by_path"][path]["p99_ms"]) for report in reports]
                ),
            }
            for path in paths
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("reports", nargs="+", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        result = aggregate([load_report(path) for path in args.reports])
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(
        "HTTP_BENCHMARK_AGGREGATE=PASS "
        f"runs={result['sample_runs']} "
        f"rps={result['requests_per_second']:.1f} "
        f"p95={result['latency_ms']['p95']:.1f}ms"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
