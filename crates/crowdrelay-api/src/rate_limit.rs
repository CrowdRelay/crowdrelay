//! In-process edge rate limiting for abuse damping and credential brute-force
//! protection. Fixed-window counters keyed by client identity, bounded in
//! memory, configured by validated policy from the composition root. The
//! single-node deployment makes process-local state authoritative; horizontal
//! scale-out would move this behind a shared store before instances multiply.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Extension, Request},
    http::{HeaderMap, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{Problem, request_id};

const WINDOW_SECS: u64 = 60;
const MAX_ENTRIES: usize = 10_240;
/// Minimum spacing between full-map sweeps. Reclamation is amortized: at most
/// one O(n) pass per interval regardless of traffic, instead of per-request
/// scans once the map reaches its entry bound.
const SWEEP_MIN_INTERVAL: Duration = Duration::from_secs(1);
/// How many oldest-bucket entries one cap-pressure pass may drop. One O(n)
/// scan frees a whole batch, so an identity-churn flood pays the scan cost
/// once per batch instead of once per admission.
const CAP_EVICT_BATCH: usize = MAX_ENTRIES / 16;
const MAX_IDENTITY_BYTES: usize = 64;

/// Fan signup. It has its own class because it is the one public endpoint that
/// sends mail to an address the caller chose: at the `General` budget it sat at
/// 600 a minute per IP while `/v1/fans/access`, which does the same thing for
/// an address that already exists, sat at 30. The exposure is not the database
/// write, it is the sending domain -- confirmation mail is the whole funnel
/// under double opt-in, and a reputation hit costs every real signup after it.
const SIGNUP_PATHS: &[&str] = &["/v1/fans"];

const PUBLIC_AUTH_PATHS: &[&str] = &[
    "/v1/fans/access",
    "/v1/fans/confirm",
    "/v1/staff-pairing/exchange",
    "/v1/beacon/invitations/exchange",
    "/v1/me/area/challenge",
    "/v1/passes/claim",
];

const PRIVILEGED_PREFIXES: &[&str] = &[
    "/v1/admin/",
    "/v1/staff/",
    "/v1/internal/",
    "/v1/commerce/",
    "/v1/control-plane/",
];

/// Limits applied per identity per fixed one-minute window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimitPolicy {
    pub enabled: bool,
    pub public_auth_per_minute: u32,
    pub privileged_per_minute: u32,
    pub general_per_minute: u32,
    pub signup_per_minute: u32,
}

impl RateLimitPolicy {
    pub const fn production_default() -> Self {
        Self {
            enabled: true,
            public_auth_per_minute: 30,
            privileged_per_minute: 120,
            general_per_minute: 600,
            signup_per_minute: 120,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum LimitClass {
    PublicAuth,
    Privileged,
    General,
    Signup,
}

impl LimitClass {
    const fn label(self) -> &'static str {
        match self {
            Self::PublicAuth => "public_auth",
            Self::Privileged => "privileged",
            Self::General => "general",
            Self::Signup => "signup",
        }
    }

    const fn limit(self, policy: &RateLimitPolicy) -> u32 {
        match self {
            Self::PublicAuth => policy.public_auth_per_minute,
            Self::Privileged => policy.privileged_per_minute,
            Self::General => policy.general_per_minute,
            Self::Signup => policy.signup_per_minute,
        }
    }
}

#[derive(Debug)]
struct Window {
    bucket: u64,
    count: u32,
}

type EntryKey = (LimitClass, Box<str>);

/// Shared fixed-window limiter. One instance per API process, assembled once
/// by the composition root and shared through request extensions.
#[derive(Debug)]
pub struct RateLimiter {
    policy: RateLimitPolicy,
    entries: Mutex<HashMap<EntryKey, Window>>,
    last_sweep: Mutex<Instant>,
    /// Set once when a poisoned lock is first observed so the fail-open
    /// degradation is reported exactly once instead of per request.
    poison_reported: AtomicBool,
}

impl RateLimiter {
    pub fn new(policy: RateLimitPolicy) -> Self {
        Self {
            policy,
            entries: Mutex::new(HashMap::new()),
            last_sweep: Mutex::new(Instant::now()),
            poison_reported: AtomicBool::new(false),
        }
    }

    fn report_poisoned_lock(&self, lock: &'static str) {
        if !self.poison_reported.swap(true, Ordering::Relaxed) {
            tracing::error!(
                lock,
                "rate limiter mutex poisoned; admission fails open until restart"
            );
        }
    }

    /// Returns `Some(seconds_until_reset)` when the request exceeds its class
    /// budget, `None` when it is admitted.
    fn admit(&self, class: LimitClass, identity: &str) -> Option<u64> {
        let limit = class.limit(&self.policy);
        if limit == 0 {
            return None;
        }
        let now = unix_seconds();
        let bucket = now / WINDOW_SECS;
        let key: EntryKey = (class, Box::from(identity));
        let Ok(mut entries) = self.entries.lock() else {
            self.report_poisoned_lock("entries");
            return None;
        };
        self.sweep_if_due(&mut entries, bucket);
        match entries.get_mut(&key) {
            Some(window) if window.bucket == bucket => {
                if window.count >= limit {
                    Some(WINDOW_SECS - (now % WINDOW_SECS))
                } else {
                    window.count += 1;
                    None
                }
            }
            Some(window) => {
                *window = Window { bucket, count: 1 };
                None
            }
            None => {
                if entries.len() >= MAX_ENTRIES {
                    // Deadline-gated sweep first; only genuinely live entries
                    // past the bound fall through to oldest-bucket eviction.
                    self.sweep_if_due(&mut entries, bucket);
                    self.evict_oldest_batch(&mut entries);
                }
                entries.insert(key, Window { bucket, count: 1 });
                None
            }
        }
    }

    fn sweep_if_due(&self, entries: &mut HashMap<EntryKey, Window>, current_bucket: u64) {
        let Ok(mut last) = self.last_sweep.lock() else {
            self.report_poisoned_lock("last_sweep");
            return;
        };
        if last.elapsed() < SWEEP_MIN_INTERVAL {
            return;
        }
        *last = Instant::now();
        drop(last);
        self.evict_stale(entries, current_bucket);
    }

    fn evict_stale(&self, entries: &mut HashMap<EntryKey, Window>, current_bucket: u64) {
        entries.retain(|_, window| window.bucket >= current_bucket.saturating_sub(1));
    }

    /// Drops up to `CAP_EVICT_BATCH` entries from the oldest window bucket.
    /// The oldest bucket is never empty past the cap, so one O(n) pass always
    /// frees at least one slot and usually a full batch — a fresh identity
    /// admission then skips the scan entirely until the freed room fills up
    /// again. `extract_if` removes victims without cloning their keys.
    fn evict_oldest_batch(&self, entries: &mut HashMap<EntryKey, Window>) {
        let Some(oldest) = entries.values().map(|window| window.bucket).min() else {
            return;
        };
        let mut remaining = CAP_EVICT_BATCH;
        entries.retain(|_, window| {
            if window.bucket == oldest && remaining > 0 {
                remaining -= 1;
                return false;
            }
            true
        });
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

/// Extracts the client identity from proxy-injected headers. The reverse
/// proxies documented under `deploy/reverse-proxy` set these per hop; direct
/// unproxied access shares one bounded bucket rather than failing open.
fn client_identity(headers: &HeaderMap) -> Box<str> {
    for name in ["x-forwarded-for", "x-real-ip"] {
        let value = headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(value) = value {
            let candidate = value.split(',').next().unwrap_or(value).trim();
            if is_plausible_identity(candidate) {
                return Box::from(candidate.to_ascii_lowercase());
            }
        }
    }
    Box::from("unknown")
}

fn is_plausible_identity(candidate: &str) -> bool {
    candidate.len() <= MAX_IDENTITY_BYTES
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'-'))
}

fn classify(method: &Method, path: &str) -> Option<LimitClass> {
    if matches!(
        path,
        "/metrics" | "/v1/health/live" | "/v1/health/ready" | "/health/live" | "/health/ready"
    ) {
        return None;
    }
    if method == Method::POST && SIGNUP_PATHS.contains(&path) {
        return Some(LimitClass::Signup);
    }
    if method == Method::POST && PUBLIC_AUTH_PATHS.contains(&path) {
        return Some(LimitClass::PublicAuth);
    }
    if PRIVILEGED_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
    {
        return Some(LimitClass::Privileged);
    }
    Some(LimitClass::General)
}

/// Middleware admitting or rejecting requests against the configured policy.
pub(crate) async fn enforce_rate_limits(
    Extension(limiter): Extension<Option<Arc<RateLimiter>>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let Some(limiter) = limiter.filter(|limiter| limiter.policy.enabled) else {
        return next.run(request).await;
    };
    let Some(class) = classify(request.method(), request.uri().path()) else {
        return next.run(request).await;
    };
    let identity = client_identity(request.headers());
    if let Some(retry_after) = limiter.admit(class, &identity) {
        crate::record_rate_limited(class.label());
        let mut response =
            Problem::too_many_requests(request_id(request.headers())).into_response();
        let seconds = retry_after.max(1).to_string();
        if let Ok(value) = header::HeaderValue::from_str(&seconds) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn policy(limit: u32) -> RateLimitPolicy {
        RateLimitPolicy {
            enabled: true,
            public_auth_per_minute: limit,
            privileged_per_minute: limit,
            general_per_minute: limit,
            signup_per_minute: limit,
        }
    }

    #[test]
    fn admits_up_to_limit_then_rejects_with_retry_hint() {
        let limiter = RateLimiter::new(policy(2));
        assert!(limiter.admit(LimitClass::General, "203.0.113.7").is_none());
        assert!(limiter.admit(LimitClass::General, "203.0.113.7").is_none());
        let retry = limiter.admit(LimitClass::General, "203.0.113.7");
        assert!(retry.is_some_and(|seconds| (1..=WINDOW_SECS).contains(&seconds)));
    }

    #[test]
    fn identities_are_isolated_per_class() {
        let limiter = RateLimiter::new(policy(1));
        assert!(
            limiter
                .admit(LimitClass::PublicAuth, "203.0.113.7")
                .is_none()
        );
        assert!(
            limiter
                .admit(LimitClass::Privileged, "203.0.113.7")
                .is_none()
        );
        assert!(limiter.admit(LimitClass::General, "203.0.113.7").is_none());
    }

    #[test]
    fn zero_limit_disables_the_class_without_state() {
        let limiter = RateLimiter::new(RateLimitPolicy {
            public_auth_per_minute: 0,
            privileged_per_minute: 0,
            general_per_minute: 0,
            signup_per_minute: 0,
            ..policy(1)
        });
        for _ in 0..100 {
            assert!(limiter.admit(LimitClass::General, "203.0.113.9").is_none());
        }
        assert!(limiter.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn forwarded_for_first_hop_wins_and_normalizes() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "198.51.100.23 , 10.0.0.1".parse().unwrap(),
        );
        assert_eq!(&*client_identity(&headers), "198.51.100.23");

        headers.clear();
        headers.insert("x-real-ip", "2001:DB8::5".parse().unwrap());
        assert_eq!(&*client_identity(&headers), "2001:db8::5");

        headers.clear();
        headers.insert("x-forwarded-for", "not an ip!!".parse().unwrap());
        assert_eq!(&*client_identity(&headers), "unknown");
        assert_eq!(&*client_identity(&HeaderMap::new()), "unknown");
    }

    #[test]
    fn classification_routes_paths_to_classes() {
        let post = Method::POST;
        let get = Method::GET;
        assert_eq!(
            classify(&post, "/v1/fans/access"),
            Some(LimitClass::PublicAuth)
        );
        assert_eq!(classify(&get, "/v1/fans/access"), Some(LimitClass::General));
        assert_eq!(
            classify(&post, "/v1/admin/audience/overview"),
            Some(LimitClass::Privileged)
        );
        assert_eq!(
            classify(&post, "/v1/internal/autopilot/actions/x/execution-claim"),
            Some(LimitClass::Privileged)
        );
        assert_eq!(classify(&get, "/metrics"), None);
        assert_eq!(classify(&get, "/v1/health/live"), None);
        assert_eq!(classify(&get, "/public/events"), Some(LimitClass::General));
        // Signup is the sibling of the two token endpoints above and shares
        // their abuse shape, so it must never fall through to the flood damper.
        assert_eq!(classify(&post, "/v1/fans"), Some(LimitClass::Signup));
        assert_eq!(classify(&get, "/v1/fans"), Some(LimitClass::General));
        assert_ne!(classify(&post, "/v1/fans"), Some(LimitClass::General));
    }

    #[test]
    fn stale_windows_are_swept_under_memory_pressure() {
        let limiter = RateLimiter::new(policy(u32::MAX));
        {
            let mut entries = limiter.entries.lock().unwrap();
            for index in 0..MAX_ENTRIES {
                let identity: Box<str> = format!("10.{index}.0.1").into();
                entries.insert(
                    (LimitClass::General, identity),
                    Window {
                        bucket: 0,
                        count: 1,
                    },
                );
            }
            limiter.evict_stale(&mut entries, unix_seconds() / WINDOW_SECS);
            assert!(entries.len() < MAX_ENTRIES);
        }
    }

    #[test]
    fn at_cap_inserts_admit_and_stay_bounded() {
        let limiter = RateLimiter::new(policy(u32::MAX));
        {
            let mut entries = limiter.entries.lock().unwrap();
            let bucket = unix_seconds() / WINDOW_SECS;
            for index in 0..MAX_ENTRIES {
                let identity: Box<str> = format!("10.{index}.0.1").into();
                entries.insert((LimitClass::General, identity), Window { bucket, count: 1 });
            }
        }
        // A fresh identity past the bound is still admitted once, and the
        // oldest-bucket eviction keeps the map from growing without limit.
        assert!(
            limiter
                .admit(LimitClass::General, "203.0.113.200")
                .is_none()
        );
        assert!(limiter.entries.lock().unwrap().len() <= MAX_ENTRIES);
    }

    #[test]
    fn cap_eviction_frees_a_batch_per_pass_not_one_entry() {
        let limiter = RateLimiter::new(policy(u32::MAX));
        let current = unix_seconds() / WINDOW_SECS;
        let stale_count = CAP_EVICT_BATCH / 2;
        {
            let mut entries = limiter.entries.lock().unwrap();
            for index in 0..stale_count {
                let identity: Box<str> = format!("10.{index}.0.1").into();
                entries.insert(
                    (LimitClass::General, identity),
                    Window {
                        bucket: current - 1,
                        count: 1,
                    },
                );
            }
            for index in 0..(MAX_ENTRIES - stale_count) {
                let identity: Box<str> = format!("172.16.{index}.1").into();
                entries.insert(
                    (LimitClass::General, identity),
                    Window {
                        bucket: current,
                        count: 1,
                    },
                );
            }
            limiter.evict_oldest_batch(&mut entries);
            // One O(n) pass cleared every stale-window entry at once; a
            // per-entry evictor would have needed `stale_count` full scans.
            assert_eq!(entries.len(), MAX_ENTRIES - stale_count);
            assert!(!entries.values().any(|window| window.bucket == current - 1));
        }
        for offset in 0..64 {
            assert!(
                limiter
                    .admit(LimitClass::General, &format!("203.0.113.{offset}"))
                    .is_none()
            );
        }
    }

    #[test]
    fn poisoned_entries_mutex_fails_open_and_reports_once() {
        let limiter = RateLimiter::new(policy(1));
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = limiter.entries.lock().unwrap();
            panic!("poison the entries lock");
        }));
        assert!(poisoned.is_err());
        assert!(limiter.admit(LimitClass::General, "203.0.113.7").is_none());
        assert!(limiter.poison_reported.load(Ordering::Relaxed));
    }

    #[test]
    fn problem_response_carries_too_many_requests_status() {
        let problem = Problem::too_many_requests(None);
        assert_eq!(problem.status, StatusCode::TOO_MANY_REQUESTS.as_u16());
        let response = problem.into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
