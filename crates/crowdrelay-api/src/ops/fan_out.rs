// Request plumbing shared by every operations endpoint: how a page is sized,
// how an id and an idempotency key are validated, and how a fan-out arm is
// bounded in both time and connections.
//
// Split out of `query_support.rs`, which crossed the 1000-line chunk limit
// when the attention view's timeout wrapper grew a connection limiter. These
// are the pieces that belong to serving a request rather than to loading a
// particular read model, so they are the ones that leave.

fn page_size(limit: Option<i64>) -> Result<i64, OpsError> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE);
    (1..=MAX_PAGE_SIZE)
        .contains(&limit)
        .then_some(limit)
        .ok_or(OpsError::BadRequest)
}

fn parse_id(id: &str) -> Result<Uuid, OpsError> {
    Uuid::parse_str(id).map_err(|_| OpsError::BadRequest)
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, OpsError> {
    let value = headers
        .get(&IDEMPOTENCY_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| {
            (8..=128).contains(&value.len())
                && value.bytes().all(|byte| (b'!'..=b'~').contains(&byte))
        })
        .ok_or(OpsError::BadRequest)?;
    Ok(value.to_owned())
}

async fn run_with_timeout<T, E>(
    duration: Duration,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, OpsError>
where
    E: Into<OpsError>,
{
    timeout(duration, future)
        .await
        .map_err(|_| OpsError::Unavailable)?
        .map_err(Into::into)
}

/// The process-wide connection budget bounding the control-plane surface.
///
/// This type replaced a per-request limiter (`ops_fan_out_limit`, half the
/// pool) after measuring the page it guarded: one operations-page load fires
/// nine endpoints at once, several of which fan out further inside, and held
/// 10 of 10 connections for the duration. Each endpoint's own budget was
/// sized against the entire pool on the assumption it ran alone; they never
/// run alone, so the per-request budgets composed into pool exhaustion — the
/// same failure one layer up.
///
/// Three quarters of the pool, floored at one — at production's pool of 8,
/// six permits for control-plane reads and two connections always free for
/// everything else this API serves: public signups, ticketing, the mobile fan
/// surface. Those are exactly the requests a page refresh must never starve.
/// The size is read from the pool rather than written down because four files
/// already disagreed about the pool once; deriving it from
/// `pool.options().get_max_connections()` makes the ratio true in every
/// deployment without depending on which one somebody shipped.
///
/// The permit unit is one concurrent query, not one request. A handler whose
/// reads are sequential pays one permit however many queries it makes, a
/// `tokio::join!` pays one per in-flight arm (`hold`), and a call whose
/// internals fan out pays its documented width up front (`budgeted`). What
/// pays: every endpoint the operations page fires — its single-connection
/// arms included, because nine of them unbudgeted still take nine connections
/// from a pool of eight — and every control-plane read that fans out further,
/// wherever it lives (`cycle/preview` pays five for the snapshot join inside
/// it). Sequential one-connection reads elsewhere on the surface stay
/// unbudgeted on purpose: one connection is already their fair share, the
/// pool's own acquire queue spreads them, and the headroom quarter absorbs
/// the ones that happen to fire beside a page load. The ops retry/clear
/// endpoints ride the same bound even though they write — they hold a
/// connection while their transaction runs, which is the thing being bounded.
///
/// Cloning shares the budget — the semaphore lives behind one `Arc` so every
/// `AppState` clone bounds the same pool, not a fresh allowance each.
#[derive(Clone)]
pub(crate) struct ControlPlaneReadBudget {
    inner: std::sync::Arc<ControlPlaneReadBudgetInner>,
}

struct ControlPlaneReadBudgetInner {
    semaphore: tokio::sync::Semaphore,
    limit: usize,
}

impl ControlPlaneReadBudget {
    /// Sizes the budget from the pool this process was actually configured
    /// with: three quarters, floored at one so a pool of 1 serialises reads
    /// rather than deadlocking on a zero-permit semaphore.
    pub(crate) fn new(pool: &sqlx::PgPool) -> Self {
        let pool_size = pool.options().get_max_connections() as usize;
        let limit = (pool_size * 3 / 4).max(1);
        Self {
            inner: std::sync::Arc::new(ControlPlaneReadBudgetInner {
                semaphore: tokio::sync::Semaphore::new(limit),
                limit,
            }),
        }
    }

    /// Waits for one permit. `None` only if the semaphore were ever closed —
    /// nothing closes it, and a caller holding `None` runs the read anyway:
    /// when the bound itself is broken, queueing at the pool's own acquire
    /// is the safer failure than hanging on a permit that can never come.
    async fn acquire(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        self.inner.semaphore.acquire().await.ok()
    }

    /// Waits for `width` permits at once, for a read whose internals fan out
    /// to `width` connections — `load_control_overview` runs four sequential
    /// branches concurrently, so it pays four.
    ///
    /// `width` clamps to the budget: `acquire_many` of more than the total
    /// would wait until the caller's timeout fired on every request, which on
    /// a small pool would turn one endpoint into a permanent failure instead
    /// of a slower read.
    async fn acquire_many(&self, width: usize) -> Option<tokio::sync::SemaphorePermit<'_>> {
        self.inner
            .semaphore
            .acquire_many(width.min(self.inner.limit) as u32)
            .await
            .ok()
    }

    /// Total permits, for the tests that pin the pool ratio.
    #[cfg(test)]
    fn limit(&self) -> usize {
        self.inner.limit
    }
}

/// `run_with_timeout`, holding one read-budget permit for the read's lifetime.
///
/// The timeout starts before the permit is acquired on purpose: a read that
/// spends its whole budget waiting for a connection has failed to answer in
/// time, and saying so is more honest than reporting a fast query that never
/// ran.
async fn run_limited<T, E>(
    budget: &ControlPlaneReadBudget,
    duration: Duration,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, OpsError>
where
    E: Into<OpsError>,
{
    timeout(duration, async {
        let _permit = budget.acquire().await;
        future.await.map_err(Into::into)
    })
    .await
    .map_err(|_| OpsError::Unavailable)?
}

/// `timeout(duration, …)` around a read holding `width` budget permits.
///
/// Returns `None` when the timeout fires — the caller maps `None` onto
/// whichever "temporarily unavailable" its own error type carries, so this
/// stays usable from modules whose errors are not `OpsError`. The permit wait
/// is inside the timeout on purpose: queueing time is part of the read's
/// latency, not a hidden preamble.
pub(crate) async fn budgeted<T>(
    budget: &ControlPlaneReadBudget,
    width: usize,
    duration: Duration,
    future: impl Future<Output = T>,
) -> Option<T> {
    timeout(duration, async {
        let _permit = budget.acquire_many(width.max(1)).await;
        future.await
    })
    .await
    .ok()
}

/// Holds one read-budget permit for the duration of a `tokio::join!` arm.
///
/// The permit unit is the in-flight query, so an arm that fans out further
/// wraps each of its leaves rather than paying once for the whole arm. No
/// timeout of its own — the join's outer timeout already bounds the wait.
pub(crate) async fn hold<T>(
    budget: &ControlPlaneReadBudget,
    future: impl Future<Output = T>,
) -> T {
    let _permit = budget.acquire().await;
    future.await
}

#[cfg(test)]
mod fan_out_tests {
    use super::{budgeted, hold, ControlPlaneReadBudget};
    use sqlx::postgres::PgPoolOptions;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Builds a pool without connecting, so the budget can be checked at every
    /// size the deployed configuration actually uses.
    fn pool_of(max: u32) -> sqlx::PgPool {
        PgPoolOptions::new()
            .max_connections(max)
            .connect_lazy("postgres://invalid/invalid")
            .expect("lazy pool")
    }

    /// Three quarters of the pool, at each size the configuration actually
    /// uses.
    ///
    /// These four numbers are why the budget is read rather than written down:
    /// the code default is 20, `.env.example` says 10, the production
    /// deployment carries 8. A fixed 4 was "half the pool" for none of them.
    #[tokio::test]
    async fn the_budget_is_three_quarters_of_whatever_pool_this_process_has() {
        assert_eq!(ControlPlaneReadBudget::new(&pool_of(20)).limit(), 15);
        assert_eq!(ControlPlaneReadBudget::new(&pool_of(10)).limit(), 7);
        assert_eq!(ControlPlaneReadBudget::new(&pool_of(5)).limit(), 3);
        assert_eq!(ControlPlaneReadBudget::new(&pool_of(8)).limit(), 6);
    }

    /// A page load must never be able to take the whole pool.
    ///
    /// This is the property the budget exists for, and it has to hold at every
    /// size rather than at the one somebody had in mind.
    #[tokio::test]
    async fn a_page_load_always_leaves_connections_for_the_rest_of_the_api() {
        for pool_size in 2_u32..=64 {
            let limit = ControlPlaneReadBudget::new(&pool_of(pool_size)).limit();
            assert!(
                limit < pool_size as usize,
                "pool {pool_size} would let control-plane reads take {limit} of {pool_size}"
            );
        }
    }

    /// A pool of one is a valid configuration.
    ///
    /// `3 / 4` is 0, and a semaphore of zero permits deadlocks every read
    /// instead of serialising them — the floor is what stops that being a hang.
    #[tokio::test]
    async fn a_pool_of_one_serialises_rather_than_deadlocks() {
        assert_eq!(ControlPlaneReadBudget::new(&pool_of(1)).limit(), 1);
    }

    /// The budget is the ceiling on concurrent reads, not a per-request one.
    ///
    /// Five reads against a two-permit budget must never hold three permits at
    /// once — and must all still finish, because queueing is the point and
    /// dropping work is not.
    #[tokio::test]
    async fn concurrent_reads_never_exceed_the_budget_and_all_complete() {
        let budget = ControlPlaneReadBudget::new(&pool_of(3));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut arms = Vec::new();
        for _ in 0..5 {
            let in_flight = Arc::clone(&in_flight);
            let peak = Arc::clone(&peak);
            let budget = budget.clone();
            arms.push(tokio::spawn(async move {
                hold(&budget, async move {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for arm in arms {
            arm.await.expect("read task");
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    /// A read wider than the whole budget must not wait forever.
    ///
    /// `load_control_overview` wants four permits; on a two-connection pool the
    /// budget is one. The clamp is what makes that a serialised read instead
    /// of a `acquire_many` that can never be satisfied.
    #[tokio::test]
    async fn a_read_wider_than_the_budget_clamps_instead_of_deadlocking() {
        let budget = ControlPlaneReadBudget::new(&pool_of(2));
        let result = budgeted(&budget, 4, Duration::from_secs(5), async { 42 }).await;
        assert_eq!(result, Some(42));
    }

    /// `None` on timeout is the contract every caller maps onto its own
    /// "unavailable" — including the wait for a permit, not just the query.
    #[tokio::test]
    async fn the_permit_wait_counts_against_the_timeout() {
        let budget = ControlPlaneReadBudget::new(&pool_of(1));
        let held = budget.clone();
        let _blocker = held.acquire().await;
        let result = budgeted(&budget, 1, Duration::from_millis(20), async { 1 }).await;
        assert_eq!(result, None);
    }
}

