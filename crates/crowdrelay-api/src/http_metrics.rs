//! Tiny bounded HTTP request telemetry used by `/metrics` and Server-Timing.
//! No paths, headers, fan identifiers or payload data are retained.

use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

const MAX_ROUTE_SERIES: usize = 1024;
const ROUTE_BUCKET_MS: [u64; 7] = [50, 100, 250, 500, 1_000, 2_500, 5_000];

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct RouteKey {
    method: String,
    route: String,
    status_class: String,
}

#[derive(Debug, Default, Clone)]
struct RouteStats {
    count: u64,
    micros_sum: u64,
    buckets: [u64; 7],
}

#[derive(Debug, Default)]
pub(crate) struct HttpMetrics {
    total: AtomicU64,
    errors_4xx: AtomicU64,
    errors_5xx: AtomicU64,
    latency_micros_sum: AtomicU64,
    le_50_ms: AtomicU64,
    le_100_ms: AtomicU64,
    le_250_ms: AtomicU64,
    le_500_ms: AtomicU64,
    le_1000_ms: AtomicU64,
    le_2500_ms: AtomicU64,
    le_5000_ms: AtomicU64,
    legacy_area_claim_import_attempts: AtomicU64,
    legacy_area_wallet_import_attempts: AtomicU64,
    legacy_area_claim_imports: AtomicU64,
    legacy_area_wallet_imports: AtomicU64,
    legacy_static_staff_auth: AtomicU64,
    route_series: Mutex<HashMap<RouteKey, RouteStats>>,
    route_series_dropped: AtomicU64,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct HttpMetricsSnapshot {
    pub total: u64,
    pub errors_4xx: u64,
    pub errors_5xx: u64,
    pub latency_micros_sum: u64,
    pub le_50_ms: u64,
    pub le_100_ms: u64,
    pub le_250_ms: u64,
    pub le_500_ms: u64,
    pub le_1000_ms: u64,
    pub le_2500_ms: u64,
    pub le_5000_ms: u64,
    pub legacy_area_claim_import_attempts: u64,
    pub legacy_area_wallet_import_attempts: u64,
    pub legacy_area_claim_imports: u64,
    pub legacy_area_wallet_imports: u64,
    pub legacy_static_staff_auth: u64,
}

impl HttpMetrics {
    pub(crate) fn record(&self, elapsed_micros: u64, status: u16) {
        self.total.fetch_add(1, Ordering::Relaxed);
        self.latency_micros_sum
            .fetch_add(elapsed_micros, Ordering::Relaxed);
        if (400..500).contains(&status) {
            self.errors_4xx.fetch_add(1, Ordering::Relaxed);
        } else if status >= 500 {
            self.errors_5xx.fetch_add(1, Ordering::Relaxed);
        }
        let elapsed_ms = elapsed_micros / 1_000;
        if elapsed_ms <= 50 {
            self.le_50_ms.fetch_add(1, Ordering::Relaxed);
        }
        if elapsed_ms <= 100 {
            self.le_100_ms.fetch_add(1, Ordering::Relaxed);
        }
        if elapsed_ms <= 250 {
            self.le_250_ms.fetch_add(1, Ordering::Relaxed);
        }
        if elapsed_ms <= 500 {
            self.le_500_ms.fetch_add(1, Ordering::Relaxed);
        }
        if elapsed_ms <= 1_000 {
            self.le_1000_ms.fetch_add(1, Ordering::Relaxed);
        }
        if elapsed_ms <= 2_500 {
            self.le_2500_ms.fetch_add(1, Ordering::Relaxed);
        }
        if elapsed_ms <= 5_000 {
            self.le_5000_ms.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_route(&self, method: &str, route: &str, elapsed_micros: u64, status: u16) {
        let status_class = format!("{}xx", status / 100);
        let key = RouteKey {
            method: method.to_owned(),
            route: route.to_owned(),
            status_class,
        };
        let Ok(mut series) = self.route_series.try_lock() else {
            self.route_series_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if !series.contains_key(&key) && series.len() >= MAX_ROUTE_SERIES {
            self.route_series_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let stat = series.entry(key).or_default();
        stat.count = stat.count.saturating_add(1);
        stat.micros_sum = stat.micros_sum.saturating_add(elapsed_micros);
        let elapsed_ms = elapsed_micros / 1_000;
        for (bucket, bound) in stat.buckets.iter_mut().zip(ROUTE_BUCKET_MS.iter()) {
            if elapsed_ms <= *bound {
                *bucket = bucket.saturating_add(1);
            }
        }
    }

    pub(crate) fn route_prometheus(&self) -> String {
        let Ok(series) = self.route_series.lock() else {
            return String::new();
        };
        let mut rows = series.iter().collect::<Vec<_>>();
        rows.sort_by(|(left, _), (right, _)| {
            left.route
                .cmp(&right.route)
                .then(left.method.cmp(&right.method))
                .then(left.status_class.cmp(&right.status_class))
        });
        let mut out = String::from(
            "# HELP crowdrelay_http_route_request_duration_seconds HTTP request latency by bounded route template.\n# TYPE crowdrelay_http_route_request_duration_seconds histogram\n",
        );
        for (key, stat) in rows {
            let route = key.route.replace('\\', "\\\\").replace('"', "\\\"");
            let labels = format!(
                "method=\"{}\",route=\"{}\",status_class=\"{}\"",
                key.method, route, key.status_class
            );
            for (bucket, bound) in stat.buckets.iter().zip(ROUTE_BUCKET_MS.iter()) {
                out.push_str(&format!("crowdrelay_http_route_request_duration_seconds_bucket{{{labels},le=\"{:.2}\"}} {}\n", *bound as f64 / 1000.0, bucket));
            }
            out.push_str(&format!("crowdrelay_http_route_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {}\n", stat.count));
            out.push_str(&format!(
                "crowdrelay_http_route_request_duration_seconds_sum{{{labels}}} {:.6}\n",
                stat.micros_sum as f64 / 1_000_000.0
            ));
            out.push_str(&format!(
                "crowdrelay_http_route_request_duration_seconds_count{{{labels}}} {}\n",
                stat.count
            ));
        }
        out.push_str("# HELP crowdrelay_http_route_series_dropped_total Route metric observations dropped at the cardinality cap.\n# TYPE crowdrelay_http_route_series_dropped_total counter\n");
        out.push_str(&format!(
            "crowdrelay_http_route_series_dropped_total {}\n",
            self.route_series_dropped.load(Ordering::Relaxed)
        ));
        out
    }

    pub(crate) fn record_legacy_area_claim_import_attempt(&self) {
        self.legacy_area_claim_import_attempts
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_legacy_area_wallet_import_attempt(&self) {
        self.legacy_area_wallet_import_attempts
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records a newly applied compatibility import, not an idempotent replay.
    pub(crate) fn record_legacy_area_claim_import(&self) {
        self.legacy_area_claim_imports
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records a newly applied compatibility import, not an idempotent replay.
    pub(crate) fn record_legacy_area_wallet_import(&self) {
        self.legacy_area_wallet_imports
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_legacy_static_staff_auth(&self) {
        self.legacy_static_staff_auth
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> HttpMetricsSnapshot {
        HttpMetricsSnapshot {
            total: self.total.load(Ordering::Relaxed),
            errors_4xx: self.errors_4xx.load(Ordering::Relaxed),
            errors_5xx: self.errors_5xx.load(Ordering::Relaxed),
            latency_micros_sum: self.latency_micros_sum.load(Ordering::Relaxed),
            le_50_ms: self.le_50_ms.load(Ordering::Relaxed),
            le_100_ms: self.le_100_ms.load(Ordering::Relaxed),
            le_250_ms: self.le_250_ms.load(Ordering::Relaxed),
            le_500_ms: self.le_500_ms.load(Ordering::Relaxed),
            le_1000_ms: self.le_1000_ms.load(Ordering::Relaxed),
            le_2500_ms: self.le_2500_ms.load(Ordering::Relaxed),
            le_5000_ms: self.le_5000_ms.load(Ordering::Relaxed),
            legacy_area_claim_import_attempts: self
                .legacy_area_claim_import_attempts
                .load(Ordering::Relaxed),
            legacy_area_wallet_import_attempts: self
                .legacy_area_wallet_import_attempts
                .load(Ordering::Relaxed),
            legacy_area_claim_imports: self.legacy_area_claim_imports.load(Ordering::Relaxed),
            legacy_area_wallet_imports: self.legacy_area_wallet_imports.load(Ordering::Relaxed),
            legacy_static_staff_auth: self.legacy_static_staff_auth.load(Ordering::Relaxed),
        }
    }
}
