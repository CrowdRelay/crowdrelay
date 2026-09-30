//! `POST /v1/internal/registry/sync` — the n8n executor's wake-up for the
//! scout-registry sync. Three properties keep it honest: the internal
//! boundary authenticates it like every `/v1/internal/` route, an accepted
//! call notifies BOTH sync workers over the channels they already listen
//! on, and a doubled wake is a no-change scan, not a duplicate import.

use std::time::Duration;

use axum::{
    body::{Body, to_bytes},
    http::{
        Request, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
};
use crowdrelay_api::HttpConfig;
use crowdrelay_domain::WorkspaceId;
use serde_json::Value;
use sqlx::postgres::PgListener;
use tower::ServiceExt;

use crate::{attestation_anchor, common};

async fn post(
    app: &axum::Router,
    bearer: Option<&str>,
) -> Result<(StatusCode, Value), Box<dyn std::error::Error>> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/internal/registry/sync")
        .header(CONTENT_TYPE, "application/json");
    if let Some(key) = bearer {
        request = request.header(AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = app.clone().oneshot(request.body(Body::empty())?).await?;
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await?;
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)?
    };
    Ok((status, json))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn registry_sync_wake_authenticates_and_notifies_both_workers()
-> Result<(), Box<dyn std::error::Error>> {
    let (pool, url) = common::test_pool_with_url("CROWDRELAY_TEST_DATABASE_URL").await?;
    let workspace_uuid = attestation_anchor::seed_workspace(&pool).await?;
    let app = crowdrelay_api::router(
        attestation_anchor::app_state(&pool, WorkspaceId::from_uuid(workspace_uuid))?,
        HttpConfig::new(["http://localhost:4321".to_owned()])?,
    );

    // The internal boundary is real: no bearer and a wrong bearer both
    // refuse before the handler runs.
    let (status, _) = post(&app, None).await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "unauthenticated POST");
    let (status, _) = post(&app, Some("not-the-key")).await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "wrong-key POST");

    // Listen on the workers' own channels — the notify the test asserts
    // is the exact wake the workers receive.
    let mut listener = PgListener::connect(&url).await?;
    listener
        .listen_all(["gdrive_contacts", "github_registry"])
        .await?;

    let (status, body) = post(&app, Some(attestation_anchor::COMMERCE_KEY)).await?;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["scan"], "requested");

    let mut heard = std::collections::BTreeSet::new();
    for _ in 0..2 {
        let notification = tokio::time::timeout(Duration::from_secs(5), listener.recv())
            .await
            .map_err(|_| "timed out waiting for the worker notifications")??;
        heard.insert(notification.channel().to_owned());
    }
    assert!(
        heard.contains("gdrive_contacts") && heard.contains("github_registry"),
        "one of the sync workers was not woken: {heard:?}"
    );
    Ok(())
}
