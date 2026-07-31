use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use crowdrelay_worker::outbox::{
    CROWDRELAY_EVENT_ID, CROWDRELAY_EVENT_TYPE, CROWDRELAY_EVENT_VERSION, CROWDRELAY_SIGNATURE,
    CROWDRELAY_TIMESTAMP, MapSecretProvider, OutboxWorker, OutboxWorkerConfig, SecretValue,
};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

const TEST_SECRET_REFERENCE: &str = "test/outbox-http-e2e";
const TEST_SECRET: &[u8] = b"outbox-e2e-secret-32-bytes-long!!";
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
const TEST_TIMEOUT: Duration = Duration::from_secs(12);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_OUTBOX_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn signed_http_delivery_is_exact_and_durable() -> Result<()> {
    let database_url = std::env::var("CROWDRELAY_OUTBOX_TEST_DATABASE_URL")
        .context("CROWDRELAY_OUTBOX_TEST_DATABASE_URL must target a disposable database")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .context("connect to test PostgreSQL")?;
    crowdrelay_infra::database::MIGRATOR
        .run(&pool)
        .await
        .context("apply migrations")?;

    let fixture = FixtureIds::new();
    let result = run_scenario(&pool, fixture).await;
    let cleanup_result = cleanup_fixture(&pool, fixture.workspace_id).await;
    pool.close().await;

    result.context("signed webhook scenario must succeed")?;
    cleanup_result.context("remove signed webhook test fixture")?;
    Ok(())
}

async fn run_scenario(pool: &PgPool, fixture: FixtureIds) -> Result<()> {
    let listener = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0))
        .await
        .context("bind loopback webhook receiver")?;
    let endpoint_url = format!(
        "http://{}/crowdrelay-outbox-e2e",
        listener
            .local_addr()
            .context("read webhook receiver address")?
    );

    seed_fixture(pool, fixture, &endpoint_url).await?;

    let server = tokio::spawn(capture_one_request(listener));
    let secret = SecretValue::new(TEST_SECRET.to_vec()).context("construct test HMAC secret")?;
    let provider =
        MapSecretProvider::new(HashMap::from([(TEST_SECRET_REFERENCE.to_owned(), secret)]));
    let config = OutboxWorkerConfig {
        worker_id: format!("outbox-http-e2e-{}", fixture.event_id.simple()),
        outbox_batch_size: 1,
        max_concurrent_deliveries: 1,
        database_operation_timeout: Duration::from_secs(3),
        secret_resolution_timeout: Duration::from_secs(1),
        http_connect_timeout: Duration::from_secs(1),
        lease_duration: Duration::from_secs(70),
        allow_http_endpoints: true,
        ..OutboxWorkerConfig::default()
    };
    let worker = OutboxWorker::new(pool.clone(), Arc::new(provider), config)
        .context("build outbox worker")?;

    let stats = match timeout(TEST_TIMEOUT, worker.run_once()).await {
        Ok(result) => result.context("execute one outbox cycle")?,
        Err(error) => {
            server.abort();
            return Err(error).context("outbox cycle timed out");
        }
    };
    ensure!(
        stats.outbox_claimed == 1,
        "expected one claimed outbox event"
    );
    ensure!(
        stats.deliveries_materialized == 1,
        "expected one materialized delivery"
    );
    ensure!(
        stats.deliveries_claimed == 1,
        "expected one claimed delivery"
    );
    ensure!(stats.delivered == 1, "expected one delivered webhook");
    ensure!(stats.retried == 0, "webhook must not be retried");
    ensure!(stats.dead == 0, "webhook must not be marked dead");

    let captured = await_server(server).await?;
    verify_request(&captured, fixture)?;
    verify_durable_result(pool, fixture).await
}

async fn seed_fixture(pool: &PgPool, fixture: FixtureIds, endpoint_url: &str) -> Result<()> {
    let mut transaction = pool.begin().await.context("start fixture transaction")?;
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(fixture.workspace_id)
        .bind(format!("outbox-e2e-{}", fixture.workspace_id.simple()))
        .bind("Outbox HTTP E2E")
        .execute(&mut *transaction)
        .await
        .context("insert fixture workspace")?;
    sqlx::query(
        r#"
        INSERT INTO webhook_endpoints (
            id,
            workspace_id,
            name,
            url,
            signing_secret_ref,
            timeout_ms,
            max_attempts,
            active
        )
        VALUES ($1, $2, 'loopback-receiver', $3, $4, 3000, 3, true)
        "#,
    )
    .bind(fixture.endpoint_id)
    .bind(fixture.workspace_id)
    .bind(endpoint_url)
    .bind(TEST_SECRET_REFERENCE)
    .execute(&mut *transaction)
    .await
    .context("insert fixture endpoint")?;
    sqlx::query(
        r#"
        INSERT INTO outbox_events (
            id,
            workspace_id,
            event_type,
            event_version,
            payload,
            request_id,
            available_at
        )
        VALUES (
            $1,
            $2,
            'fan.created',
            1,
            '{"fixture":"signed-delivery"}',
            'request-outbox-e2e',
            TIMESTAMPTZ '1970-01-01 00:00:00+00'
        )
        "#,
    )
    .bind(fixture.event_id)
    .bind(fixture.workspace_id)
    .execute(&mut *transaction)
    .await
    .context("insert fixture outbox event")?;
    transaction.commit().await.context("commit fixture")
}

async fn cleanup_fixture(pool: &PgPool, workspace_id: Uuid) -> Result<()> {
    let mut transaction = pool.begin().await.context("start cleanup transaction")?;
    sqlx::query("DELETE FROM outbox_events WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&mut *transaction)
        .await
        .context("delete fixture outbox events")?;
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace_id)
        .execute(&mut *transaction)
        .await
        .context("delete fixture workspace")?;
    transaction.commit().await.context("commit fixture cleanup")
}

async fn capture_one_request(listener: TcpListener) -> Result<CapturedRequest> {
    timeout(TEST_TIMEOUT, async move {
        let (mut stream, peer) = listener.accept().await.context("accept webhook request")?;
        ensure!(
            peer.ip().is_loopback(),
            "webhook receiver accepted a non-loopback peer"
        );

        let mut received = Vec::with_capacity(4096);
        let header_end = loop {
            let mut chunk = [0_u8; 2048];
            let count = stream
                .read(&mut chunk)
                .await
                .context("read webhook headers")?;
            ensure!(count > 0, "connection closed before complete headers");
            received.extend_from_slice(&chunk[..count]);
            ensure!(
                received.len() <= MAX_HEADER_BYTES + MAX_BODY_BYTES,
                "webhook request exceeds test receiver bounds"
            );

            if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n") {
                let header_end = position + 4;
                ensure!(
                    header_end <= MAX_HEADER_BYTES,
                    "webhook headers exceed test receiver bounds"
                );
                break header_end;
            }
        };

        let header_text = std::str::from_utf8(&received[..header_end - 4])
            .context("webhook headers must be UTF-8")?;
        let mut lines = header_text.split("\r\n");
        let request_line = lines
            .next()
            .context("missing HTTP request line")?
            .to_owned();
        let mut headers = HashMap::new();
        for line in lines {
            let (name, value) = line
                .split_once(':')
                .context("malformed HTTP request header")?;
            let previous =
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
            ensure!(previous.is_none(), "duplicate HTTP request header");
        }
        ensure!(
            !headers.contains_key("transfer-encoding"),
            "test receiver expects a content-length body"
        );
        let content_length = headers
            .get("content-length")
            .context("missing content-length header")?
            .parse::<usize>()
            .context("invalid content-length header")?;
        ensure!(
            content_length <= MAX_BODY_BYTES,
            "webhook body exceeds test receiver bounds"
        );

        while received.len() - header_end < content_length {
            let remaining = content_length - (received.len() - header_end);
            let mut chunk = [0_u8; 2048];
            let read_limit = remaining.min(chunk.len());
            let count = stream
                .read(&mut chunk[..read_limit])
                .await
                .context("read webhook body")?;
            ensure!(count > 0, "connection closed before complete body");
            received.extend_from_slice(&chunk[..count]);
        }
        ensure!(
            received.len() == header_end + content_length,
            "webhook request contains bytes beyond declared body"
        );

        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .context("write webhook response")?;
        stream.shutdown().await.context("close webhook response")?;

        Ok(CapturedRequest {
            request_line,
            headers,
            body: received[header_end..].to_vec(),
        })
    })
    .await
    .context("webhook receiver timed out")?
}

async fn await_server(mut server: JoinHandle<Result<CapturedRequest>>) -> Result<CapturedRequest> {
    match timeout(TEST_TIMEOUT, &mut server).await {
        Ok(joined) => joined.context("webhook receiver task panicked")?,
        Err(error) => {
            server.abort();
            Err(error).context("webhook receiver task timed out")
        }
    }
}

fn verify_request(request: &CapturedRequest, fixture: FixtureIds) -> Result<()> {
    ensure!(
        request.request_line == "POST /crowdrelay-outbox-e2e HTTP/1.1",
        "unexpected HTTP request target"
    );
    ensure!(
        header(request, "content-type")? == "application/json",
        "unexpected content type"
    );

    let expected_event_id = format!("evt_{}", fixture.event_id.simple());
    ensure!(
        header(request, CROWDRELAY_EVENT_ID)? == expected_event_id,
        "event ID header does not match persisted event"
    );
    ensure!(
        header(request, CROWDRELAY_EVENT_TYPE)? == "fan.created",
        "event type header does not match persisted event"
    );
    ensure!(
        header(request, CROWDRELAY_EVENT_VERSION)? == "1",
        "event version header does not match persisted event"
    );

    let timestamp_text = header(request, CROWDRELAY_TIMESTAMP)?;
    timestamp_text
        .parse::<i64>()
        .context("webhook timestamp header must be an integer")?;
    verify_hmac(
        timestamp_text.as_bytes(),
        &request.body,
        header(request, CROWDRELAY_SIGNATURE)?,
    )?;

    let envelope: Value =
        serde_json::from_slice(&request.body).context("parse exact webhook body")?;
    ensure!(
        envelope.get("id") == Some(&json!(expected_event_id)),
        "envelope ID does not match header"
    );
    ensure!(
        envelope.get("type") == Some(&json!("fan.created")),
        "envelope type does not match header"
    );
    ensure!(
        envelope.get("version") == Some(&json!(1)),
        "envelope version does not match header"
    );
    ensure!(
        envelope.get("workspace_id") == Some(&json!(fixture.workspace_id)),
        "envelope workspace does not match fixture"
    );
    ensure!(
        envelope.get("data") == Some(&json!({"fixture": "signed-delivery"})),
        "envelope data does not match persisted payload"
    );
    ensure!(
        envelope
            .get("occurred_at")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty()),
        "envelope occurred_at is missing"
    );
    Ok(())
}

fn verify_hmac(timestamp: &[u8], body: &[u8], signature_header: &str) -> Result<()> {
    let encoded = signature_header
        .strip_prefix("v1=")
        .context("webhook signature must use protocol v1")?;
    let signature = hex::decode(encoded).context("webhook signature must be hex")?;

    let mut verifier = Hmac::<Sha256>::new_from_slice(TEST_SECRET)
        .context("construct independent HMAC verifier")?;
    verifier.update(timestamp);
    verifier.update(b".");
    verifier.update(body);
    verifier
        .verify_slice(&signature)
        .map_err(|_| anyhow::anyhow!("webhook HMAC verification failed"))
}

async fn verify_durable_result(pool: &PgPool, fixture: FixtureIds) -> Result<()> {
    let delivery = sqlx::query_as::<_, (String, i32, Option<i16>, Option<String>, bool)>(
        r#"
        SELECT
            status,
            attempt_count,
            last_response_status,
            last_error_kind,
            delivered_at IS NOT NULL
        FROM webhook_deliveries
        WHERE workspace_id = $1
          AND outbox_event_id = $2
          AND endpoint_id = $3
        "#,
    )
    .bind(fixture.workspace_id)
    .bind(fixture.event_id)
    .bind(fixture.endpoint_id)
    .fetch_one(pool)
    .await
    .context("read durable delivery status")?;
    ensure!(delivery.0 == "delivered", "delivery status is not durable");
    ensure!(delivery.1 == 1, "delivery attempt count must be one");
    ensure!(delivery.2 == Some(204), "delivery must persist HTTP 204");
    ensure!(
        delivery.3.is_none(),
        "successful delivery has an error kind"
    );
    ensure!(
        delivery.4,
        "successful delivery has no delivered_at timestamp"
    );

    let attempt = sqlx::query_as::<_, (i32, String, Option<i16>, Option<String>)>(
        r#"
        SELECT attempt_number, outcome, response_status, error_kind
        FROM webhook_delivery_attempts
        WHERE workspace_id = $1
          AND delivery_id = (
              SELECT id
              FROM webhook_deliveries
              WHERE workspace_id = $1
                AND outbox_event_id = $2
                AND endpoint_id = $3
          )
        "#,
    )
    .bind(fixture.workspace_id)
    .bind(fixture.event_id)
    .bind(fixture.endpoint_id)
    .fetch_one(pool)
    .await
    .context("read durable delivery attempt")?;
    ensure!(attempt.0 == 1, "attempt number must be one");
    ensure!(
        attempt.1 == "delivered",
        "attempt outcome must be delivered"
    );
    ensure!(attempt.2 == Some(204), "attempt must persist HTTP 204");
    ensure!(attempt.3.is_none(), "successful attempt has an error kind");

    let outbox_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM outbox_events WHERE id = $1")
            .bind(fixture.event_id)
            .fetch_one(pool)
            .await
            .context("read durable outbox status")?;
    ensure!(
        outbox_status == "delivered",
        "outbox event was not materialized durably"
    );
    Ok(())
}

fn header<'a>(request: &'a CapturedRequest, name: &str) -> Result<&'a str> {
    request
        .headers
        .get(&name.to_ascii_lowercase())
        .map(String::as_str)
        .with_context(|| format!("missing protocol header {name}"))
}

struct CapturedRequest {
    request_line: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone, Copy)]
struct FixtureIds {
    workspace_id: Uuid,
    endpoint_id: Uuid,
    event_id: Uuid,
}

impl FixtureIds {
    fn new() -> Self {
        Self {
            workspace_id: Uuid::now_v7(),
            endpoint_id: Uuid::now_v7(),
            event_id: Uuid::now_v7(),
        }
    }
}
