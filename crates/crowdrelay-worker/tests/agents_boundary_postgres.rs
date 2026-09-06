//! The CrowdRelay → crowdrelay-agents boundary, exercised at runtime.
//!
//! Everything else that guards this boundary is static: the capability-token
//! parity gate reads both languages' source, and the unit tests on either side
//! check their own half. Neither can see the boundary itself. A token that
//! both sides agree on in source can still be rejected by the running service;
//! an HTTP error can still be recorded as a business success; a hung
//! dependency can still leave a row claiming an action completed.
//!
//! This suite runs the real thing:
//!
//! * a disposable PostgreSQL, migrated by CrowdRelay's own migrator,
//! * the real `crowdrelay-agents` Fastify service, on a real port,
//! * CrowdRelay's real HTTP clients and real state transitions,
//! * tokens minted by [`derive_agent_token_with_capability`] — the function
//!   the worker ships, not a reimplementation that would only prove the test
//!   agrees with itself.
//!
//! # What is doubled, and why
//!
//! A *successful* join is the one outcome that cannot be produced without
//! subscribing a real Reddit account to a real subreddit. Tests F and G need
//! that success to prove there is exactly one of it, so they point CrowdRelay
//! at a local listener that stands in for the external side effect and counts
//! what it receives. Everything on the CrowdRelay side of that listener — the
//! claim, the HTTP client, the state machine, the durable row — is real.
//!
//! Tests B, C and the capability-scope test use the real agents service, and
//! none of them reaches Reddit: they are all resolved at the auth boundary or
//! by the service's own "no credentials stored" rejection.
//!
//! # Running it
//!
//! `just test-agents-boundary` starts the disposable database and the real
//! agents service, exports the three variables below, and runs this target.
//! Without them the tests are skipped rather than failed — a bare CrowdRelay
//! checkout has no sibling agents repository.
//!
//! * `CROWDRELAY_AGENTS_TEST_DATABASE_URL` — disposable database
//! * `CROWDRELAY_AGENTS_TEST_URL` — base URL of the running agents service
//! * `CROWDRELAY_AGENTS_TEST_AUTH_KEY` — that service's
//!   `AGENT_SERVICE_AUTH_KEY`

use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_worker::{
    community_join_executor::CommunityJoinExecutorWorker,
    discovery::{AgentCapability, derive_agent_token, derive_agent_token_with_capability},
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The three variables the suite needs, or `None` when it should skip.
struct Env {
    database_url: String,
    agents_url: String,
    auth_key: String,
}

impl Env {
    fn load() -> Option<Self> {
        Some(Self {
            database_url: std::env::var("CROWDRELAY_AGENTS_TEST_DATABASE_URL").ok()?,
            agents_url: std::env::var("CROWDRELAY_AGENTS_TEST_URL")
                .ok()?
                .trim_end_matches('/')
                .to_owned(),
            auth_key: std::env::var("CROWDRELAY_AGENTS_TEST_AUTH_KEY").ok()?,
        })
    }
}

/// Connects and migrates. The agents service migrates its own tables on
/// startup against the same database, so both halves of the schema are real.
async fn connect(env: &Env) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&env.database_url)
        .await
        .context("connect to the disposable test database")?;
    crowdrelay_infra::database::MIGRATOR
        .run(&pool)
        .await
        .context("apply CrowdRelay migrations")?;
    Ok(pool)
}

/// `discovery_places.workspace_id` is a real foreign key, so a tenant has to
/// exist before it can own anything. Every other fixture row cascades from
/// this one.
async fn seed_workspace(pool: &PgPool, workspace: Uuid) -> Result<()> {
    sqlx::query(
        "INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'agents boundary suite') \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(workspace)
    .bind(format!("agents-boundary-{}", workspace.simple()))
    .execute(pool)
    .await
    .context("seed the test workspace")?;
    Ok(())
}

/// A subreddit the executor is eligible to claim: active, big enough, and
/// not yet joined.
async fn seed_joinable_place(pool: &PgPool, workspace: Uuid, name: &str) -> Result<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        r"INSERT INTO discovery_places
              (workspace_id, place_kind, platform, name, url, member_count,
               status, membership_state)
          VALUES ($1, 'subreddit', 'reddit', $2, $3, 5000, 'active', 'not_joined')
          RETURNING id",
    )
    .bind(workspace)
    .bind(name)
    .bind(format!("https://reddit.com/{name}/{}", Uuid::now_v7()))
    .fetch_one(pool)
    .await
    .context("seed a joinable discovery place")?;
    Ok(id)
}

/// `(membership_state, membership_note)` for one place, read back through the
/// workspace the way every production query does.
async fn membership(
    pool: &PgPool,
    workspace: Uuid,
    place: Uuid,
) -> Result<(String, Option<String>)> {
    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT membership_state, membership_note FROM discovery_places \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace)
    .bind(place)
    .fetch_one(pool)
    .await
    .context("read back the membership state")?;
    Ok(row)
}

/// Removes the tenant; everything the suite wrote cascades from it.
async fn cleanup(pool: &PgPool, workspaces: &[Uuid]) -> Result<()> {
    for workspace in workspaces {
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(workspace)
            .execute(pool)
            .await
            .context("remove the test fixture")?;
    }
    Ok(())
}

fn executor(
    pool: &PgPool,
    workspace: Uuid,
    agents_url: &str,
    auth_key: &str,
) -> Result<CommunityJoinExecutorWorker> {
    CommunityJoinExecutorWorker::new(
        pool.clone(),
        WorkspaceId::from_uuid(workspace),
        agents_url.to_owned(),
        Some(auth_key.to_owned()),
        true, // auto_join — the path under test
    )
    .context("build the community join executor")
}

// ---------------------------------------------------------------------------
// Local stand-ins for the external side effect
// ---------------------------------------------------------------------------

/// A listener that counts requests and answers each with a fixed response.
///
/// This is the *external side effect* — a real Reddit subscription — and
/// nothing else. The claim, the token, the HTTP client, the state machine and
/// the durable row on the CrowdRelay side of it are all production code.
struct CountingAgents {
    url: String,
    seen: Arc<AtomicUsize>,
    handle: JoinHandle<()>,
}

impl CountingAgents {
    async fn start(response: &'static str) -> Result<Self> {
        let listener = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0))
            .await
            .context("bind the counting stand-in")?;
        let addr: SocketAddr = listener.local_addr().context("read its address")?;
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut scratch = [0_u8; 4096];
                // One read is enough to let the client finish sending; the
                // body is not inspected, only counted.
                let _ = stream.read(&mut scratch).await;
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        Ok(Self {
            url: format!("http://{addr}"),
            seen,
            handle,
        })
    }

    fn requests(&self) -> usize {
        self.seen.load(Ordering::SeqCst)
    }
}

impl Drop for CountingAgents {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A listener that accepts the connection and never answers. The client's own
/// timeout is the only thing that ends the request.
struct HangingAgents {
    url: String,
    handle: JoinHandle<()>,
}

impl HangingAgents {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0))
            .await
            .context("bind the hanging stand-in")?;
        let addr: SocketAddr = listener.local_addr().context("read its address")?;
        let handle = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                // Held, not dropped: dropping the socket would close the
                // connection and produce a transport error instead of the
                // silence this test is about.
                held.push(stream);
            }
        });
        Ok(Self {
            url: format!("http://{addr}"),
            handle,
        })
    }
}

impl Drop for HangingAgents {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// An address nothing is listening on — the dependency is simply gone.
async fn unreachable_agents() -> Result<String> {
    let listener = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0))
        .await
        .context("bind to find a free port")?;
    let addr = listener.local_addr().context("read the free port")?;
    drop(listener);
    Ok(format!("http://{addr}"))
}

const JOIN_OK: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                       Content-Length: 22\r\nConnection: close\r\n\r\n\
                       {\"status\":\"joined\"}\r\n\r\n";

// ---------------------------------------------------------------------------
// 1. Capability scope — a read token cannot mutate
// ---------------------------------------------------------------------------

/// A token scoped to `read` must not open a `credentials` or `social_publish`
/// route on the running service.
///
/// The flag that used to make this fail was `allowLegacyTokens`, which
/// defaulted to true and granted every capability to any workspace-bound
/// token. Nothing in either repository's unit tests could see it, because the
/// scoped token was never the token that got in.
#[tokio::test]
#[ignore = "requires a disposable database and a running crowdrelay-agents service"]
async fn a_read_token_cannot_write_credentials_or_publish() -> Result<()> {
    let Some(env) = Env::load() else {
        eprintln!("skipped: agents boundary variables are not set");
        return Ok(());
    };
    let workspace = Uuid::now_v7();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("build the probe client")?;
    let read = derive_agent_token_with_capability(&env.auth_key, workspace, AgentCapability::Read);

    // The read token opens the read route. Without this the rejections below
    // would also pass if the service were rejecting everything.
    let status = client
        .get(format!("{}/reddit/status", env.agents_url))
        .header("Authorization", format!("Bearer {read}"))
        .header("X-Workspace-Id", workspace.to_string())
        .send()
        .await
        .context("GET /reddit/status with a read token")?
        .status();
    ensure!(
        status.is_success(),
        "a correctly scoped read token must open the read route, got HTTP {status}"
    );

    // The legacy token is the one that matters. It is workspace-bound but
    // carries no capability, and the service used to accept it by default and
    // grant it everything — so any drift that made a scoped check miss landed
    // here and authenticated anyway, silently. A scoped `read` token failing a
    // `credentials` route proves nothing about that, because a read token was
    // never the token that got in.
    let legacy = derive_agent_token(&env.auth_key, workspace);

    for (route, body) in [
        (
            "/reddit/credentials",
            serde_json::json!({"reddit_username": "probe", "reddit_password": "probe-password"}),
        ),
        ("/reddit/join", serde_json::json!({"subreddit": "probe"})),
        ("/reddit/scrape", serde_json::json!({"queries": ["probe"]})),
    ] {
        for (label, token) in [("read-scoped", &read), ("legacy-unscoped", &legacy)] {
            let status = client
                .post(format!("{}{route}", env.agents_url))
                .header("Authorization", format!("Bearer {token}"))
                .header("X-Workspace-Id", workspace.to_string())
                .json(&body)
                .send()
                .await
                .with_context(|| format!("POST {route} with a {label} token"))?
                .status();
            ensure!(
                status.as_u16() == 401,
                "a {label} token must be rejected at {route} with 401, got HTTP \
                 {status}. A non-401 means the request passed authorization — for \
                 the legacy token that means AGENT_SERVICE_ALLOW_LEGACY_TOKENS is \
                 enabled and capability scoping is not being enforced at all"
            );
        }
    }

    // And a correctly scoped token gets past authorization on the same route,
    // so the rejection above is about the capability and not about the route.
    let publish = derive_agent_token_with_capability(
        &env.auth_key,
        workspace,
        AgentCapability::SocialPublish,
    );
    let status = client
        .post(format!("{}/reddit/join", env.agents_url))
        .header("Authorization", format!("Bearer {publish}"))
        .header("X-Workspace-Id", workspace.to_string())
        .json(&serde_json::json!({"subreddit": "probe"}))
        .send()
        .await
        .context("POST /reddit/join with a social_publish token")?
        .status();
    ensure!(
        status.as_u16() != 401,
        "a social_publish token must pass authorization at /reddit/join; \
         got 401, so the rejection above proved nothing about capability"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// B. Wrong tenant
// ---------------------------------------------------------------------------

/// A valid token for workspace A, sent with workspace B's identity, is
/// rejected by the running service — and B's rows are untouched.
///
/// Driven over HTTP rather than through the HMAC helper: the helper agreeing
/// with itself says nothing about what the service accepts.
#[tokio::test]
#[ignore = "requires a disposable database and a running crowdrelay-agents service"]
async fn b_a_token_for_one_workspace_is_rejected_for_another() -> Result<()> {
    let Some(env) = Env::load() else {
        eprintln!("skipped: agents boundary variables are not set");
        return Ok(());
    };
    let pool = connect(&env).await?;
    let workspace_a = Uuid::now_v7();
    let workspace_b = Uuid::now_v7();
    let result = wrong_tenant_scenario(&pool, &env, workspace_a, workspace_b).await;
    let cleaned = cleanup(&pool, &[workspace_a, workspace_b]).await;
    pool.close().await;
    result?;
    cleaned
}

async fn wrong_tenant_scenario(
    pool: &PgPool,
    env: &Env,
    workspace_a: Uuid,
    workspace_b: Uuid,
) -> Result<()> {
    seed_workspace(pool, workspace_a).await?;
    seed_workspace(pool, workspace_b).await?;
    let place_b = seed_joinable_place(pool, workspace_b, "r/tenant-b").await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("build the probe client")?;

    // Every capability, so this is a statement about tenant binding rather
    // than about one route.
    for capability in [
        AgentCapability::Read,
        AgentCapability::Dispatch,
        AgentCapability::Credentials,
        AgentCapability::SocialPublish,
    ] {
        let token = derive_agent_token_with_capability(&env.auth_key, workspace_a, capability);
        for (method, route) in [
            ("GET", "/reddit/status"),
            ("POST", "/reddit/join"),
            ("POST", "/reddit/credentials"),
        ] {
            let request = if method == "GET" {
                client.get(format!("{}{route}", env.agents_url))
            } else {
                client
                    .post(format!("{}{route}", env.agents_url))
                    .json(&serde_json::json!({"subreddit": "probe"}))
            };
            let status = request
                .header("Authorization", format!("Bearer {token}"))
                // A's token, B's identity.
                .header("X-Workspace-Id", workspace_b.to_string())
                .send()
                .await
                .with_context(|| format!("{method} {route} with a cross-tenant token"))?
                .status();
            ensure!(
                status.as_u16() == 401,
                "a token minted for workspace A must be rejected when presented \
                 as workspace B at {route} ({capability:?}), got HTTP {status}"
            );
        }
    }

    // No business mutation, and no credential was stored under B's identity.
    let (state, _) = membership(pool, workspace_b, place_b).await?;
    ensure!(
        state == "not_joined",
        "the rejected cross-tenant request must not move workspace B's \
         membership state, found {state}"
    );
    let credentials: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_service_credentials WHERE workspace_id = $1",
    )
    .bind(workspace_b)
    .fetch_one(pool)
    .await
    .context("count credentials stored for workspace B")?;
    ensure!(
        credentials == 0,
        "the rejected cross-tenant POST /reddit/credentials must not store a \
         credential for workspace B, found {credentials}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// C. A well-formed HTTP response can still be a domain rejection
// ---------------------------------------------------------------------------

/// The real agents service answers a join for a workspace with no stored
/// Reddit credentials with a valid HTTP response that means "no". CrowdRelay
/// must record that as a rejection, not as a join.
#[tokio::test]
#[ignore = "requires a disposable database and a running crowdrelay-agents service"]
async fn c_a_domain_rejection_is_not_recorded_as_a_join() -> Result<()> {
    let Some(env) = Env::load() else {
        eprintln!("skipped: agents boundary variables are not set");
        return Ok(());
    };
    let pool = connect(&env).await?;
    let workspace = Uuid::now_v7();
    let result = domain_rejection_scenario(&pool, &env, workspace).await;
    let cleaned = cleanup(&pool, &[workspace]).await;
    pool.close().await;
    result?;
    cleaned
}

async fn domain_rejection_scenario(pool: &PgPool, env: &Env, workspace: Uuid) -> Result<()> {
    seed_workspace(pool, workspace).await?;
    let place = seed_joinable_place(pool, workspace, "r/domainrejection").await?;
    let worker = executor(pool, workspace, &env.agents_url, &env.auth_key)?;

    let joined = worker.run_once().await.context("run one join cycle")?;
    ensure!(
        joined == 0,
        "a domain rejection must not be counted as a processed join, got {joined}"
    );

    let (state, note) = membership(pool, workspace, place).await?;
    ensure!(
        state == "rejected",
        "a domain rejection must leave the place `rejected`, found {state}"
    );
    // Not merely "not joined": the recorded reason has to be the rejection
    // that happened, so an operator reading the row learns what the service
    // said rather than that something went wrong.
    let note = note.unwrap_or_default();
    ensure!(
        note.contains("agents") && note.contains("503"),
        "the recorded note must carry the agent service's own rejection, found: {note}"
    );
    ensure!(
        note.contains("credentials"),
        "the note must name the actual cause reported by the service, found: {note}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// D. A hung dependency
// ---------------------------------------------------------------------------

/// A dependency that accepts the connection and never answers must never
/// produce a join, and retrying must not multiply the attempt into two
/// durable effects.
///
/// Slow on purpose: the executor's own 60-second request timeout is the thing
/// under test, and shortening it would test a different client than the one
/// that ships.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable database; takes over a minute by design"]
async fn d_a_hung_dependency_never_becomes_a_join() -> Result<()> {
    let Some(env) = Env::load() else {
        eprintln!("skipped: agents boundary variables are not set");
        return Ok(());
    };
    let pool = connect(&env).await?;
    let workspace = Uuid::now_v7();
    let result = hung_dependency_scenario(&pool, &env, workspace).await;
    let cleaned = cleanup(&pool, &[workspace]).await;
    pool.close().await;
    result?;
    cleaned
}

async fn hung_dependency_scenario(pool: &PgPool, env: &Env, workspace: Uuid) -> Result<()> {
    let hanging = HangingAgents::start().await?;
    seed_workspace(pool, workspace).await?;
    let place = seed_joinable_place(pool, workspace, "r/hung").await?;
    let worker = executor(pool, workspace, &hanging.url, &env.auth_key)?;

    let joined = worker.run_once().await.context("run one join cycle")?;
    ensure!(
        joined == 0,
        "a request that never completed must not count as a join, got {joined}"
    );
    let (state, note) = membership(pool, workspace, place).await?;
    ensure!(
        state != "joined" && state != "joining",
        "a timed-out join must not be left as `{state}` — neither a success \
         nor a claim nobody will ever release"
    );
    ensure!(
        state == "rejected",
        "the state machine records a timed-out join as `rejected`, found {state}"
    );
    ensure!(
        note.as_deref().is_some_and(|n| n.contains("http")),
        "the note must say the request failed in transport, found: {note:?}"
    );

    // Retry: the place is out of the eligible set, so a second cycle cannot
    // dispatch a second attempt at the same subreddit.
    let joined_again = worker.run_once().await.context("run a second join cycle")?;
    ensure!(
        joined_again == 0,
        "retrying after a timeout must not join, got {joined_again}"
    );
    let (state_after, _) = membership(pool, workspace, place).await?;
    ensure!(
        state_after == "rejected",
        "the retry must not change the recorded outcome, found {state_after}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// F. Duplicate cycles, one durable effect
// ---------------------------------------------------------------------------

/// Two cycles over the same eligible place produce exactly one dispatch and
/// exactly one joined row.
///
/// The stand-in counts requests because "no double publish" is a claim about
/// what left the process, not only about what the database ended up with.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable database"]
async fn f_a_repeated_cycle_produces_one_durable_join() -> Result<()> {
    let Some(env) = Env::load() else {
        eprintln!("skipped: agents boundary variables are not set");
        return Ok(());
    };
    let pool = connect(&env).await?;
    let workspace = Uuid::now_v7();
    let result = duplicate_cycle_scenario(&pool, &env, workspace).await;
    let cleaned = cleanup(&pool, &[workspace]).await;
    pool.close().await;
    result?;
    cleaned
}

async fn duplicate_cycle_scenario(pool: &PgPool, env: &Env, workspace: Uuid) -> Result<()> {
    let agents = CountingAgents::start(JOIN_OK).await?;
    seed_workspace(pool, workspace).await?;
    let place = seed_joinable_place(pool, workspace, "r/idempotent").await?;
    let worker = executor(pool, workspace, &agents.url, &env.auth_key)?;

    let first = worker.run_once().await.context("first cycle")?;
    ensure!(first == 1, "the first cycle must join once, got {first}");

    let second = worker.run_once().await.context("second cycle")?;
    ensure!(
        second == 0,
        "the second cycle must find nothing eligible, got {second}"
    );

    ensure!(
        agents.requests() == 1,
        "exactly one join must have left the process, the stand-in saw {}",
        agents.requests()
    );

    let joined: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM discovery_places \
         WHERE workspace_id = $1 AND membership_state = 'joined'",
    )
    .bind(workspace)
    .fetch_one(pool)
    .await
    .context("count joined places")?;
    ensure!(
        joined == 1,
        "exactly one durable join must exist, found {joined}"
    );

    // Identity is stable: the same row, not a second one written alongside it.
    let (state, _) = membership(pool, workspace, place).await?;
    ensure!(
        state == "joined",
        "the joined row must be the place that was claimed, found {state}"
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM discovery_places \
         WHERE workspace_id = $1 AND id = $2 \
           AND membership_changed_by = 'community-join-executor' \
           AND membership_changed_at IS NOT NULL",
    )
    .bind(workspace)
    .bind(place)
    .fetch_one(pool)
    .await
    .context("check the audit fields")?;
    ensure!(
        audited == 1,
        "the durable row must name what changed it and when, found {audited}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// G. One tenant's dependency is down
// ---------------------------------------------------------------------------

/// Tenant B's agents service is gone. Tenant A keeps working, B degrades in a
/// way its own row records, and neither tenant's state reaches the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable database"]
async fn g_one_tenant_losing_agents_does_not_stop_the_other() -> Result<()> {
    let Some(env) = Env::load() else {
        eprintln!("skipped: agents boundary variables are not set");
        return Ok(());
    };
    let pool = connect(&env).await?;
    let workspace_a = Uuid::now_v7();
    let workspace_b = Uuid::now_v7();
    let result = partial_outage_scenario(&pool, &env, workspace_a, workspace_b).await;
    let cleaned = cleanup(&pool, &[workspace_a, workspace_b]).await;
    pool.close().await;
    result?;
    cleaned
}

async fn partial_outage_scenario(
    pool: &PgPool,
    env: &Env,
    workspace_a: Uuid,
    workspace_b: Uuid,
) -> Result<()> {
    let healthy = CountingAgents::start(JOIN_OK).await?;
    let gone = unreachable_agents().await?;

    seed_workspace(pool, workspace_a).await?;
    seed_workspace(pool, workspace_b).await?;
    let place_a = seed_joinable_place(pool, workspace_a, "r/healthy-tenant").await?;
    let place_b = seed_joinable_place(pool, workspace_b, "r/degraded-tenant").await?;

    let worker_a = executor(pool, workspace_a, &healthy.url, &env.auth_key)?;
    let worker_b = executor(pool, workspace_b, &gone, &env.auth_key)?;

    // Concurrently, because "B's failure does not take the process down" is
    // only interesting while A is mid-flight.
    let (result_a, result_b) = tokio::join!(worker_a.run_once(), worker_b.run_once());
    let joined_a = result_a.context("tenant A's cycle must not fail")?;
    let joined_b = result_b.context("tenant B's cycle must return, not panic or abort")?;

    ensure!(joined_a == 1, "tenant A must keep joining, got {joined_a}");
    ensure!(
        joined_b == 0,
        "tenant B must not report a join while its dependency is gone, got {joined_b}"
    );

    let (state_a, _) = membership(pool, workspace_a, place_a).await?;
    ensure!(
        state_a == "joined",
        "tenant A's place must be joined, found {state_a}"
    );

    let (state_b, note_b) = membership(pool, workspace_b, place_b).await?;
    ensure!(
        state_b == "rejected",
        "tenant B must degrade explicitly rather than silently, found {state_b}"
    );
    ensure!(
        note_b.as_deref().is_some_and(|n| !n.is_empty()),
        "tenant B's degradation must be readable from the row itself"
    );

    // No contamination in either direction: each workspace sees only its own
    // rows, and the counting stand-in received nothing from B.
    let rows_a: i64 =
        sqlx::query_scalar("SELECT count(*) FROM discovery_places WHERE workspace_id = $1")
            .bind(workspace_a)
            .fetch_one(pool)
            .await
            .context("count tenant A's places")?;
    let joined_rows_b: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM discovery_places \
         WHERE workspace_id = $1 AND membership_state = 'joined'",
    )
    .bind(workspace_b)
    .fetch_one(pool)
    .await
    .context("count tenant B's joined places")?;
    ensure!(
        rows_a == 1 && joined_rows_b == 0,
        "tenant isolation broke: A has {rows_a} places and B has {joined_rows_b} joins"
    );
    ensure!(
        healthy.requests() == 1,
        "only tenant A may reach the healthy service, it saw {} requests",
        healthy.requests()
    );

    // The process is still usable afterwards — a failed tenant must not have
    // poisoned anything shared.
    let place_a2 = seed_joinable_place(pool, workspace_a, "r/healthy-tenant-again").await?;
    let joined_again = worker_a
        .run_once()
        .await
        .context("tenant A must still work after tenant B failed")?;
    ensure!(
        joined_again == 1,
        "tenant A must keep working after tenant B's outage, got {joined_again}"
    );
    let (state_a2, _) = membership(pool, workspace_a, place_a2).await?;
    ensure!(
        state_a2 == "joined",
        "tenant A's later place must join, found {state_a2}"
    );
    Ok(())
}
