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
/// `tokio::join!`. The pool is eight, so without a limit a single page load
/// asks for more connections than exist, takes every one of them, and every
/// other request to this API waits behind it. The section that loses the race
/// then reports its own timeout, which is how identical slowness surfaces as a
/// different broken section on each refresh.
///
/// Half the pool, so a page load cannot starve the rest of the API. The
/// remaining arms queue on `acquire()` rather than failing -- this bounds
/// concurrency, it does not drop work.
const OPS_FAN_OUT_LIMIT: usize = 4;

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
