#!/usr/bin/env python3
"""Fail on material CrowdRelay runtime regressions against the previous good baseline."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def load(path: Path) -> dict[str, Any]:
    report = json.loads(path.read_text())
    if report.get("schema") != 2 or report.get("aggregation") != "median":
        raise ValueError(f"{path}: expected median aggregate schema 2")
    return report


def pct_change(candidate: float, baseline: float) -> float:
    if baseline <= 0:
        raise ValueError("baseline metric must be positive")
    return ((candidate - baseline) / baseline) * 100.0


def compare(
    candidate: dict[str, Any],
    baseline: dict[str, Any],
    *,
    max_latency_regression_pct: float,
    max_rps_drop_pct: float,
    latency_noise_floor_ms: float,
) -> dict[str, Any]:
    if candidate["requests_per_run"] != baseline["requests_per_run"]:
        raise ValueError("candidate and baseline request counts differ")
    if candidate["concurrency"] != baseline["concurrency"]:
        raise ValueError("candidate and baseline concurrency differ")
    if set(candidate["by_path"]) != set(baseline["by_path"]):
        raise ValueError("candidate and baseline route sets differ")

    checks: list[dict[str, Any]] = []

    def latency_check(name: str, candidate_ms: float, baseline_ms: float) -> None:
        regression_pct = pct_change(candidate_ms, baseline_ms)
        delta_ms = candidate_ms - baseline_ms
        failed = (
            regression_pct > max_latency_regression_pct
            and delta_ms > latency_noise_floor_ms
        )
        checks.append(
            {
                "metric": name,
                "baseline": baseline_ms,
                "candidate": candidate_ms,
                "delta_ms": delta_ms,
                "regression_pct": regression_pct,
                "failed": failed,
            }
        )

    latency_check(
        "global.p95_ms",
        float(candidate["latency_ms"]["p95"]),
        float(baseline["latency_ms"]["p95"]),
    )
    for path in sorted(candidate["by_path"]):
        latency_check(
            f"route[{path}].p95_ms",
            float(candidate["by_path"][path]["p95_ms"]),
            float(baseline["by_path"][path]["p95_ms"]),
        )

    candidate_rps = float(candidate["requests_per_second"])
    baseline_rps = float(baseline["requests_per_second"])
    rps_change_pct = pct_change(candidate_rps, baseline_rps)
    checks.append(
        {
            "metric": "requests_per_second",
            "baseline": baseline_rps,
            "candidate": candidate_rps,
            "change_pct": rps_change_pct,
            "failed": rps_change_pct < -max_rps_drop_pct,
        }
    )
    failed = [check for check in checks if check["failed"]]
    return {
        "schema": 1,
        "status": "fail" if failed else "pass",
        "policy": {
            "max_latency_regression_pct": max_latency_regression_pct,
            "max_rps_drop_pct": max_rps_drop_pct,
            "latency_noise_floor_ms": latency_noise_floor_ms,
        },
        "checks": checks,
        "failed_metrics": [check["metric"] for check in failed],
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--max-latency-regression-pct", type=float, default=25.0)
    parser.add_argument("--max-rps-drop-pct", type=float, default=20.0)
    parser.add_argument("--latency-noise-floor-ms", type=float, default=15.0)
    args = parser.parse_args()
    if args.max_latency_regression_pct <= 0 or args.max_rps_drop_pct <= 0:
        parser.error("percentage thresholds must be positive")
    if args.latency_noise_floor_ms < 0:
        parser.error("latency noise floor cannot be negative")
    try:
        result = compare(
            load(args.candidate),
            load(args.baseline),
            max_latency_regression_pct=args.max_latency_regression_pct,
            max_rps_drop_pct=args.max_rps_drop_pct,
            latency_noise_floor_ms=args.latency_noise_floor_ms,
        )
    except (KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    if result["status"] == "fail":
        print(
            "PERFORMANCE_REGRESSION=FAIL metrics="
            + ",".join(result["failed_metrics"])
        )
        return 1
    print("PERFORMANCE_REGRESSION=PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
