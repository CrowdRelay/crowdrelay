//! Tiny bounded HTTP request telemetry used by `/metrics` and Server-Timing.
//! No paths, headers, fan identifiers or payload data are retained.
//!
//! The per-route series key is a single owned `String` ("METHOD route Nxx")
//! assembled with one allocation per observation; lookups borrow the string,
//! so steady-state recording costs exactly one small heap write per request.

use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

const MAX_ROUTE_SERIES: usize = 1024;
const ROUTE_BUCKET_MS: [u64; 7] = [50, 100, 250, 500, 1_000, 2_500, 5_000];

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
    rate_limited_public_auth: AtomicU64,
    rate_limited_privileged: AtomicU64,
    rate_limited_general: AtomicU64,
    route_series: Mutex<HashMap<String, RouteStats>>,
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
    pub rate_limited_public_auth: u64,
    pub rate_limited_privileged: u64,
    pub rate_limited_general: u64,
}

#[derive(Debug, Default, Clone)]
struct RouteStats {
    count: u64,
    micros_sum: u64,
    buckets: [u64; 7],
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
        let status_class = match status / 100 {
            1 => "1xx",
            2 => "2xx",
            3 => "3xx",
            4 => "4xx",
            _ => "5xx",
        };
        let mut key = String::with_capacity(method.len() + route.len() + status_class.len() + 2);
        key.push_str(method);
        key.push(' ');
        key.push_str(route);
        key.push(' ');
        key.push_str(status_class);

        let Ok(mut series) = self.route_series.try_lock() else {
            self.route_series_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if !series.contains_key(key.as_str()) && series.len() >= MAX_ROUTE_SERIES {
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
        rows.sort_by_key(|(key, _)| *key);
        let mut out = String::from(
            "# HELP crowdrelay_http_route_request_duration_seconds HTTP request latency by bounded route template.\n# TYPE crowdrelay_http_route_request_duration_seconds histogram\n",
        );
        for (key, stat) in rows {
            let (method, tail) = key.split_once(' ').unwrap_or(("", key.as_str()));
            let (route, status_class) = tail.rsplit_once(' ').unwrap_or((tail, "2xx"));
            let escaped_route = route.replace('\\', "\\\\").replace('"', "\\\"");
            let labels = format!(
                "method=\"{method}\",route=\"{escaped_route}\",status_class=\"{status_class}\""
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

    pub(crate) fn record_rate_limited(&self, class: &'static str) {
        let counter = match class {
            "public_auth" => &self.rate_limited_public_auth,
            "privileged" => &self.rate_limited_privileged,
            _ => &self.rate_limited_general,
        };
        counter.fetch_add(1, Ordering::Relaxed);
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
            rate_limited_public_auth: self.rate_limited_public_auth.load(Ordering::Relaxed),
            rate_limited_privileged: self.rate_limited_privileged.load(Ordering::Relaxed),
            rate_limited_general: self.rate_limited_general.load(Ordering::Relaxed),
        }
    }
}
