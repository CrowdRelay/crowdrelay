#!/usr/bin/env python3
import importlib.util
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def load_module(name, relative):
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


aggregate_module = load_module("aggregate_benchmarks", "scripts/aggregate-benchmarks.py")
compare_module = load_module("compare_performance", "scripts/compare-performance.py")


def raw_report(p95, rps, route_p95=None):
    route_p95 = p95 if route_p95 is None else route_p95
    return {
        "schema": 1,
        "requests": 600,
        "concurrency": 24,
        "requests_per_second": rps,
        "errors": 0,
        "latency_ms": {
            "min": p95 / 10,
            "mean": p95 / 2,
            "p50": p95 / 3,
            "p95": p95,
            "p99": p95 * 1.2,
            "max": p95 * 1.5,
        },
        "by_path": {
            "/v1/public/events": {
                "requests": 150,
                "p50_ms": route_p95 / 3,
                "p95_ms": route_p95,
                "p99_ms": route_p95 * 1.2,
            }
        },
    }


class PerformanceTrendTest(unittest.TestCase):
    def test_aggregate_uses_median_to_reject_single_runner_outlier(self):
        result = aggregate_module.aggregate(
            [raw_report(100, 1000), raw_report(105, 980), raw_report(900, 100)]
        )
        self.assertEqual(result["sample_runs"], 3)
        self.assertEqual(result["latency_ms"]["p95"], 105.0)
        self.assertEqual(result["requests_per_second"], 980.0)

    def test_material_latency_regression_fails(self):
        baseline = aggregate_module.aggregate([raw_report(100, 1000)] * 3)
        candidate = aggregate_module.aggregate([raw_report(140, 990)] * 3)
        result = compare_module.compare(
            candidate,
            baseline,
            max_latency_regression_pct=25,
            max_rps_drop_pct=20,
            latency_noise_floor_ms=15,
        )
        self.assertEqual(result["status"], "fail")
        self.assertIn("global.p95_ms", result["failed_metrics"])

    def test_small_absolute_latency_noise_does_not_flap(self):
        baseline = aggregate_module.aggregate([raw_report(10, 1000)] * 3)
        candidate = aggregate_module.aggregate([raw_report(14, 980)] * 3)
        result = compare_module.compare(
            candidate,
            baseline,
            max_latency_regression_pct=25,
            max_rps_drop_pct=20,
            latency_noise_floor_ms=15,
        )
        self.assertEqual(result["status"], "pass")

    def test_material_throughput_drop_fails(self):
        baseline = aggregate_module.aggregate([raw_report(100, 1000)] * 3)
        candidate = aggregate_module.aggregate([raw_report(100, 750)] * 3)
        result = compare_module.compare(
            candidate,
            baseline,
            max_latency_regression_pct=25,
            max_rps_drop_pct=20,
            latency_noise_floor_ms=15,
        )
        self.assertEqual(result["status"], "fail")
        self.assertIn("requests_per_second", result["failed_metrics"])


if __name__ == "__main__":
    unittest.main()
