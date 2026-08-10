//! Tiny bounded HTTP request telemetry used by `/metrics` and Server-Timing.
//! No paths, headers, fan identifiers or payload data are retained.

use std::sync::atomic::{AtomicU64, Ordering};

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
    legacy_area_claim_imports: AtomicU64,
    legacy_area_wallet_imports: AtomicU64,
    legacy_static_staff_auth: AtomicU64,
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

    pub(crate) fn record_legacy_area_claim_import(&self) {
        self.legacy_area_claim_imports
            .fetch_add(1, Ordering::Relaxed);
    }

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
            legacy_area_claim_imports: self.legacy_area_claim_imports.load(Ordering::Relaxed),
            legacy_area_wallet_imports: self.legacy_area_wallet_imports.load(Ordering::Relaxed),
            legacy_static_staff_auth: self.legacy_static_staff_auth.load(Ordering::Relaxed),
        }
    }
}
