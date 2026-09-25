//! Peer observation sweep, end to end against a canned feed.
//!
//! The HTTP half is a one-shot TCP listener on 127.0.0.1 serving a fixed
//! Atom document — the suite must not depend on YouTube being reachable.
//! This proves the full path: peer lookup -> fetch -> parse -> dated facts
//! -> dedup on the second sweep.

use std::time::Duration;

use crate::common;
use crowdrelay_domain::{WorkspaceId, content_engine::PeerStatus};
use crowdrelay_infra::content_engine::PostgresContentEngineRepository;
use crowdrelay_worker::peer_observation::PeerObservationWorker;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Canned Atom feed with dates relative to today — the sweep drops facts
/// older than 120 days, so fixed dates would silently expire the fixture.
fn canned_feed() -> String {
    let rfc3339 = &time::format_description::well_known::Rfc3339;
    let recent = (time::OffsetDateTime::now_utc() - time::Duration::days(6))
        .format(rfc3339)
        .expect("rfc3339");
    let older = (time::OffsetDateTime::now_utc() - time::Duration::days(13))
        .format(rfc3339)
        .expect("rfc3339");
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/atom+xml\r\nconnection: close\r\n\r\n\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<feed xmlns:yt=\"http://www.youtube.com/xml/schemas/2015\"\n\
      xmlns:media=\"http://search.yahoo.com/mrss/\"\n\
      xmlns=\"http://www.w3.org/2005/Atom\">\n\
  <entry>\n\
    <yt:videoId>vid00000001</yt:videoId>\n\
    <title>Playthrough of the new single</title>\n\
    <link rel=\"alternate\" href=\"https://peer.test/v/1\"/>\n\
    <published>{recent}</published>\n\
    <media:group><media:community><media:statistics views=\"1200000\"/></media:community></media:group>\n\
  </entry>\n\
  <entry>\n\
    <yt:videoId>vid00000002</yt:videoId>\n\
    <title>Studio diary day 2</title>\n\
    <link rel=\"alternate\" href=\"https://peer.test/v/2\"/>\n\
    <published>{older}</published>\n\
    <media:group><media:community><media:statistics views=\"300000\"/></media:community></media:group>\n\
  </entry>\n\
</feed>"
    )
}

/// Answers every connection with the canned feed, forever, until the test
/// drops the join handle.
async fn serve_canned_feed(listener: TcpListener) {
    while let Ok((mut socket, _)) = listener.accept().await {
        let mut buf = [0_u8; 4096];
        let _ = socket.read(&mut buf).await;
        let _ = socket.write_all(canned_feed().as_bytes()).await;
    }
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn sweep_records_dated_facts_once() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("peer-obs-{}", workspace_id.into_uuid().simple()))
        .bind("Peer Observation Test")
        .execute(&pool)
        .await?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let feed_addr = listener.local_addr()?;
    // The worker refuses loopback/link-local targets (handles are operator
    // data), so the canned server is reached through a fake hostname the
    // client DNS-overrides at the listener — the guard stays live.
    let feed_url = format!("http://feed.test:{}/feed", feed_addr.port());
    tokio::spawn(serve_canned_feed(listener));

    // The peer the sweep will read — confirmed, with the canned feed as its
    // rss handle and the `format` watch tag. Peer writes go through the
    // operator-ledger paths now; a fixture seeds the row directly.
    let peer_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO peers (
             id, workspace_id, name, handles, tier, watch_for, why,
             proposed_by, status, confirmed_at
         ) VALUES ($1, $2, $3, $4, 'near_peer', '{format}', 'sweep fixture',
                   'operator', 'confirmed', now())
         RETURNING id",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("Feed Peer {}", workspace_id.into_uuid().simple()))
    .bind(json!({"rss": feed_url}))
    .fetch_one(&pool)
    .await?;

    let client = reqwest::Client::builder()
        .resolve("feed.test", feed_addr)
        .timeout(Duration::from_secs(5))
        .build()?;
    let worker = PeerObservationWorker::with_client(
        pool.clone(),
        workspace_id,
        Duration::from_secs(3600),
        client,
    );

    let recorded = worker.sweep().await?;
    assert_eq!(recorded, 2, "both dated feed entries should land");

    let tail: Vec<(
        String,
        String,
        String,
        uuid::Uuid,
        Option<String>,
        serde_json::Value,
    )> = sqlx::query_as(
        "SELECT fact, platform, kind, peer_id, url, metrics FROM peer_observations
         WHERE workspace_id = $1 ORDER BY observed_at DESC, id DESC LIMIT 10",
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].0, "Playthrough of the new single");
    assert_eq!(tail[0].1, "rss");
    assert_eq!(tail[0].2, "post");
    assert_eq!(tail[0].3, peer_id);
    assert_eq!(
        tail[0].4.as_deref(),
        Some("https://peer.test/v/1"),
        "the evidence link rides with the fact"
    );
    assert_eq!(tail[0].5["views"], json!(1_200_000));

    // A resweep records nothing new — the dedup index, not luck.
    let again = worker.sweep().await?;
    assert_eq!(again, 0, "the same facts must not be recorded twice");
    Ok(())
}

/// The sweep opens with the proposal pass: a peer act sharing the listing's
/// genres lands `proposed` — and is *not* observed that same sweep, because
/// only a confirmed peer is ever read. The operator's confirm is what turns
/// a candidate into a feed.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn sweep_proposes_before_it_observes() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let unique = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("peer-sweep-{unique}"))
        .bind("Peer Sweep Test")
        .execute(&pool)
        .await?;
    // The listing's genre is the intersection the pass reads; the act name
    // and genre are run-unique because the peer-act graph is global.
    let genre = format!("sweep-{unique}");
    sqlx::query(
        "INSERT INTO band_listings (workspace_id, act_name, genre_tags) \
         VALUES ($1, $2, $3)",
    )
    .bind(workspace_id.into_uuid())
    .bind("The Sweep Band")
    .bind(vec![genre.clone()])
    .execute(&pool)
    .await?;
    let act_id = uuid::Uuid::now_v7();
    let act_name = format!("Sweep Match {unique}");
    sqlx::query("INSERT INTO place_peer_acts (id, name_key, display_name) VALUES ($1, $2, $3)")
        .bind(act_id)
        .bind(act_name.to_lowercase())
        .bind(&act_name)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO place_peer_act_genres
            (peer_act_id, genre_tag, provenance, source_ref)
         VALUES ($1, $2, 'researched', 'test-suite')",
    )
    .bind(act_id)
    .bind(&genre)
    .execute(&pool)
    .await?;

    let worker = PeerObservationWorker::with_client(
        pool.clone(),
        workspace_id,
        Duration::from_secs(3600),
        reqwest::Client::new(),
    );
    let recorded = worker.sweep().await?;
    assert_eq!(recorded, 0, "a proposal is never observed the same sweep");

    let repository = PostgresContentEngineRepository::new(pool.clone());
    let proposals = repository
        .list_peers(workspace_id, Some(PeerStatus::Proposed))
        .await?;
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].name, act_name);
    assert_eq!(proposals[0].proposed_by, "peer-act-graph");
    assert_eq!(
        proposals[0].handles,
        json!({}),
        "the scanner never invents handles"
    );
    Ok(())
}
