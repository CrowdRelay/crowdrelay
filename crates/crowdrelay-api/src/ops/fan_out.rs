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

/// How many of one endpoint's queries may hold a database connection at once.
///
/// The attention view fans out eleven independent reads through one
/// `tokio::join!`. Without a limit a single page load asks for more connections
/// than exist, takes every one of them, and every other request to this API
/// waits behind one operator refresh. The section that loses the race then
/// reports its own timeout, which is how identical slowness surfaces as a
/// different broken section on each refresh.
///
/// Half the pool, so a page load cannot starve the rest of the API. The
/// remaining arms queue on `acquire()` rather than failing — this bounds
/// concurrency, it does not drop work.
///
/// **Read from the pool rather than written down.** This was `const … = 4` with a
/// comment saying "the pool is eight", and four numbers disagreed about the pool:
/// the code default is 20, `.env.example` says 10, `deploy/env.production.example`
/// said 5, and the comment said 8.
///
/// The comment was the accurate one — `crowdrelay_db_pool_max` reads 8 in
/// production — so the constant genuinely was half the pool there. It was wrong
/// everywhere else: at 20 it ran the eleven-arm page in three waves for nothing,
/// and at the 5 the production example carried it would have taken 80% of the
/// pool for one page, which is the starvation it exists to prevent. The example
/// has since been corrected to 8.
///
/// Reading the pool is what makes the ratio true without depending on which of
/// four files somebody deployed from.
///
/// Floored at 1: a pool of 1 is a valid configuration, and a semaphore of 0 would
/// deadlock every arm rather than serialise them.
fn ops_fan_out_limit(pool: &sqlx::PgPool) -> usize {
    let pool_size = pool.options().get_max_connections() as usize;
    (pool_size / 2).max(1)
}

/// `run_with_timeout`, holding a permit for the duration of the query.
///
/// The timeout starts before the permit is acquired on purpose: an arm that
/// spends its whole budget waiting for a connection has failed to answer in
/// time, and saying so is more honest than reporting a fast query that never
/// ran.
async fn run_limited<T, E>(
    limiter: &tokio::sync::Semaphore,
    duration: Duration,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, OpsError>
where
    E: Into<OpsError>,
{
    timeout(duration, async {
        let _permit = limiter
            .acquire()
            .await
            .map_err(|_| OpsError::Unavailable)?;
        future.await.map_err(Into::into)
    })
    .await
    .map_err(|_| OpsError::Unavailable)?
}

#[cfg(test)]
mod fan_out_tests {
    use super::ops_fan_out_limit;
    use sqlx::postgres::PgPoolOptions;

    /// Builds a pool without connecting, so the budget can be checked at every
    /// size the deployed configuration actually uses.
    fn pool_of(max: u32) -> sqlx::PgPool {
        PgPoolOptions::new()
            .max_connections(max)
            .connect_lazy("postgres://invalid/invalid")
            .expect("lazy pool")
    }

    /// Half the pool, at each size the configuration actually uses.
    ///
    /// These four numbers are why this is read rather than written down: the code
    /// default is 20, `.env.example` says 10, `deploy/env.production.example` says
    /// 5, and the replaced constant's comment claimed 8. A fixed 4 was "half the
    /// pool" for none of them — 80% at five, and a third of the way there at
    /// twenty, which ran the eleven-arm page in three waves for nothing.
    #[tokio::test]
    async fn the_budget_is_half_of_whatever_pool_this_process_has() {
        assert_eq!(ops_fan_out_limit(&pool_of(20)), 10);
        assert_eq!(ops_fan_out_limit(&pool_of(10)), 5);
        assert_eq!(ops_fan_out_limit(&pool_of(5)), 2);
        assert_eq!(ops_fan_out_limit(&pool_of(8)), 4);
    }

    /// A page load must never be able to take the whole pool.
    ///
    /// This is the property the limiter exists for, and it has to hold at every
    /// size rather than at the one somebody had in mind.
    #[tokio::test]
    async fn a_page_load_always_leaves_connections_for_the_rest_of_the_api() {
        for pool_size in 2_u32..=64 {
            let limit = ops_fan_out_limit(&pool_of(pool_size));
            assert!(
                limit < pool_size as usize,
                "pool {pool_size} would let one page take {limit} of {pool_size}"
            );
        }
    }

    /// A pool of one is a valid configuration.
    ///
    /// `1 / 2` is 0, and a semaphore of zero permits deadlocks every arm instead
    /// of serialising them — the floor is what stops that being a hang.
    #[tokio::test]
    async fn a_pool_of_one_serialises_rather_than_deadlocks() {
        assert_eq!(ops_fan_out_limit(&pool_of(1)), 1);
    }
}

