//! N.6 — the roster's channels, pooled across its acts.
//!
//! What makes this worth a database rather than a unit test: the pooling
//! happens in SQL. `count(DISTINCT workspace_id)` per channel is the whole of
//! rule 2 ("one act is not a roster"), the organisation boundary is a join
//! nobody can see from the domain, and SQLx checks none of it at compile time —
//! a column that does not exist would be a production failure with no earlier
//! warning.
//!
//! Four properties are asserted here and none of them can be reached without
//! rows: two acts' arrivals through the same channel become one sample of the
//! sum; an act outside the organisation never joins it; a fan who left counts
//! in neither the numerator nor the denominator; and a signup with no visitor
//! is unattributed rather than quietly dropped.

use crate::common;

use crowdrelay_domain::roster_source_roi::{Evidence, Finding, rank_pooled_channels};
use crowdrelay_infra::roster_source_roi::pooled_channel_counts;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn two_acts_arrivals_become_one_sample() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run(&database).await
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let label = organization(pool, "north-label").await?;
    let act_a = workspace(pool, "act-a", Some(label)).await?;
    let act_b = workspace(pool, "act-b", Some(label)).await?;
    // Not on the roster. Every number below must be blind to this act, and the
    // only way to find out is to give it the same channel and the same fans.
    let outsider = workspace(pool, "outsider", None).await?;

    let reddit_a = smart_link(pool, act_a, "a-reddit", "reddit", Some("r-metal")).await?;
    let reddit_b = smart_link(pool, act_b, "b-reddit", "reddit", Some("r-metal")).await?;
    let facebook_a = smart_link(pool, act_a, "a-facebook", "facebook", None).await?;
    let reddit_out = smart_link(pool, outsider, "out-reddit", "reddit", Some("r-metal")).await?;

    // Sixteen each, both acts, same channel: the pooled sample the per-act read
    // would have refused twice.
    for index in 0..16 {
        arrival(pool, act_a, reddit_a, index, index < 12, "active", now).await?;
        arrival(pool, act_b, reddit_b, index, index < 4, "active", now).await?;
    }
    // One act's own channel, larger than the pooled one. It leads on arrivals
    // and must still never be pooled evidence.
    for index in 0..34 {
        arrival(
            pool,
            act_a,
            facebook_a,
            100 + index,
            index < 4,
            "active",
            now,
        )
        .await?;
    }
    // Churn. Netted out by construction: absent from both sides of the rate.
    for index in 0..9 {
        arrival(
            pool,
            act_a,
            reddit_a,
            200 + index,
            true,
            "unsubscribed",
            now,
        )
        .await?;
    }
    // The outsider's traffic, which the organisation must not see at all.
    for index in 0..7 {
        arrival(pool, outsider, reddit_out, 300 + index, true, "active", now).await?;
    }
    // Three people who arrived with no visitor cookie. Not "direct": unknown.
    for index in 0..3 {
        untracked_arrival(pool, act_b, 400 + index, now).await?;
    }

    let counts = pooled_channel_counts(pool, label, now).await?;

    let reddit = counts
        .samples
        .iter()
        .find(|sample| sample.source == "reddit")
        .ok_or("the pooled read lost the reddit channel entirely")?;
    assert_eq!(
        reddit.acts, 2,
        "two acts contributed and the pooled sample counted {}",
        reddit.acts
    );
    assert_eq!(
        reddit.signups, 32,
        "16 + 16 active arrivals pooled to {} — a churned fan or an outsider's fan leaked in",
        reddit.signups
    );
    assert_eq!(reddit.stayed_30d, 16, "12 + 4 stayed");
    assert_eq!(reddit.community.as_deref(), Some("r-metal"));

    let facebook = counts
        .samples
        .iter()
        .find(|sample| sample.source == "facebook")
        .ok_or("the pooled read lost the facebook channel")?;
    assert_eq!(
        facebook.acts, 1,
        "one act's channel reported {} acts, which would let it claim to be pooled",
        facebook.acts
    );
    assert_eq!(facebook.signups, 34);

    assert_eq!(
        counts.unattributed_signups, 3,
        "three arrivals with no visitor were not reported as unattributed"
    );
    assert_eq!(
        counts.acts_with_fans, 2,
        "the organisation has two acts with fans; the outsider is not one of them"
    );

    let read = rank_pooled_channels(&counts);
    let ranked_reddit = read
        .channels
        .iter()
        .find(|channel| channel.source == "reddit")
        .ok_or("the ranking lost the reddit channel")?;
    assert_eq!(ranked_reddit.evidence, Evidence::Pooled);
    let ranked_facebook = read
        .channels
        .iter()
        .find(|channel| channel.source == "facebook")
        .ok_or("the ranking lost the facebook channel")?;
    assert_eq!(
        ranked_facebook.evidence,
        Evidence::SingleAct,
        "the roster's biggest channel is one act's, and the read must say so"
    );

    // The point of the whole item: a channel the roster is not leaning on keeps
    // people better than the one it is, by a margin wider than both samples.
    match &read.finding {
        Finding::Reallocate { from, to, .. } => {
            assert_eq!(from, "facebook");
            assert_eq!(to, "reddit/r-metal");
        }
        other => return Err(format!("expected a reallocation, got {other:?}").into()),
    }
    Ok(())
}

async fn organization(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(slug)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn workspace(
    pool: &PgPool,
    slug: &str,
    organization_id: Option<Uuid>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name, organization_id) VALUES ($1, $2, $2, $3)")
        .bind(id)
        .bind(slug)
        .bind(organization_id)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn smart_link(
    pool: &PgPool,
    workspace_id: Uuid,
    slug: &str,
    source: &str,
    community: Option<&str>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO smart_links
            (id, workspace_id, slug, destination_url, channel_source, channel_community)
        VALUES ($1, $2, $3, 'https://example.test/x', $4, $5)
        "#,
    )
    .bind(id)
    .bind(workspace_id)
    .bind(slug)
    .bind(source)
    .bind(community)
    .execute(pool)
    .await?;
    Ok(id)
}

/// One fan who arrived through a link: the visitor, the click before the
/// signup, the consent, and — when `stayed` — a session inside the window.
async fn arrival(
    pool: &PgPool,
    workspace_id: Uuid,
    smart_link_id: Uuid,
    index: i32,
    stayed: bool,
    status: &str,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan_id = Uuid::now_v7();
    let visitor = Uuid::now_v7();
    let email = format!("fan{index}-{}@example.test", workspace_id.simple());
    let signed_up = now - time::Duration::days(45);

    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1, $2, $3, $4)",
    )
    .bind(fan_id)
    .bind(workspace_id)
    .bind(&email)
    .bind(status)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO fan_consents
            (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
        VALUES ($1, $2, 'marketing', true, 'privacy-v1', 'test', $3)
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(signed_up)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO fan_acquisition_events
            (workspace_id, fan_id, anonymous_visitor_id, source, request_id, occurred_at)
        VALUES ($1, $2, $3, 'signup', $4, $5)
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(visitor)
    .bind(format!("req-{index}-{}", workspace_id.simple()))
    .bind(signed_up)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO click_events
            (workspace_id, smart_link_id, anonymous_visitor_id, occurred_at)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(workspace_id)
    .bind(smart_link_id)
    .bind(visitor)
    .bind(signed_up - time::Duration::minutes(5))
    .execute(pool)
    .await?;

    if stayed {
        sqlx::query(
            r#"
            INSERT INTO fan_sessions
                (workspace_id, fan_id, session_token_hash, created_at, last_seen_at, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .bind(session_hash(fan_id))
        .bind(signed_up)
        .bind(now - time::Duration::days(1))
        .bind(now + time::Duration::days(30))
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Somebody who signed up with no visitor on the record — the shape every
/// analytics tool calls "direct traffic" and this one refuses to.
async fn untracked_arrival(
    pool: &PgPool,
    workspace_id: Uuid,
    index: i32,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan_id = Uuid::now_v7();
    let email = format!("untracked{index}-{}@example.test", workspace_id.simple());
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1, $2, $3, 'active')",
    )
    .bind(fan_id)
    .bind(workspace_id)
    .bind(&email)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO fan_acquisition_events
            (workspace_id, fan_id, source, request_id, occurred_at)
        VALUES ($1, $2, 'signup', $3, $4)
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(format!("req-untracked-{index}-{}", workspace_id.simple()))
    .bind(now - time::Duration::days(20))
    .execute(pool)
    .await?;
    Ok(())
}

/// The column is a 32-byte hash with a UNIQUE constraint; the fan's id padded
/// out is unique per fan and needs no crypto to be one.
fn session_hash(fan_id: Uuid) -> Vec<u8> {
    let mut hash = fan_id.as_bytes().to_vec();
    hash.extend_from_slice(fan_id.as_bytes());
    hash
}
