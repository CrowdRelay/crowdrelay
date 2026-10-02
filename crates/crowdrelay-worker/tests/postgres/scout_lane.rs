//! The FAN SCOUT lane's tripwires against a real schema: each breach of the
//! envelope is seen in the lane's own rows, a clean lane shows none, and the
//! senders' answer is the same read the watchdog raises.

use crate::common;

use anyhow::{Result, ensure};
use crowdrelay_domain::{
    WorkspaceId,
    fan_prospect::{ObservationKind, ProspectSource},
};
use crowdrelay_infra::{
    fan_prospects::{ObservedPerson, TouchKind, TouchReceipt, observe, record_touch},
    scout_lane::{Breach, breaches, halted},
};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::community_relay_batch::workspace;

async fn prospect(pool: &PgPool, ws: WorkspaceId, handle: &str) -> Result<Uuid> {
    let now = OffsetDateTime::now_utc();
    let outcome = observe(
        pool,
        ws.into_uuid(),
        &ObservedPerson {
            source: ProspectSource::OwnComments,
            platform: "instagram",
            platform_user_id: None,
            handle: Some(handle),
            display_identity: handle,
            display_name: None,
            profile_url: None,
            kind: ObservationKind::ActiveUnderOurPost,
            source_ref: &Uuid::now_v7().to_string(),
            source_url: None,
            observed_at: now,
            evidence: "Kiedy gracie?",
            confidence_basis_points: 3_000,
        },
    )
    .await?;
    match outcome {
        crowdrelay_infra::fan_prospects::ObserveOutcome::Created { prospect_id } => Ok(prospect_id),
        other => anyhow::bail!("{other:?}"),
    }
}

async fn link(pool: &PgPool, ws: WorkspaceId, active: bool) -> Result<Uuid> {
    Ok(sqlx::query_scalar(
        "INSERT INTO smart_links (workspace_id, slug, destination_url, active)
         VALUES ($1, $2, 'https://band.example/signal', $3) RETURNING id",
    )
    .bind(ws.into_uuid())
    .bind(format!("t-{}", Uuid::now_v7().simple()))
    .bind(active)
    .fetch_one(pool)
    .await?)
}

async fn touch(
    pool: &PgPool,
    ws: WorkspaceId,
    prospect_id: Uuid,
    kind: TouchKind,
    link_id: Option<Uuid>,
    at: OffsetDateTime,
) -> Result<()> {
    let source_ref = Uuid::now_v7().to_string();
    record_touch(
        pool,
        ws.into_uuid(),
        &TouchReceipt {
            prospect_id,
            kind,
            source: "owned_reply",
            source_ref: &source_ref,
            smart_link_id: link_id,
            touched_at: at,
        },
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_clean_lane_shows_no_breach_and_each_breach_is_seen_alone() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let now = OffsetDateTime::now_utc();

    // Clean: one engage, one invite on a live link to someone who spoke in a thread.
    let ws = workspace(&pool).await?;
    let a = prospect(&pool, ws, "kuba").await?;
    let b = prospect(&pool, ws, "ania").await?;
    let live = link(&pool, ws, true).await?;
    touch(
        &pool,
        ws,
        a,
        TouchKind::Engage,
        None,
        now - Duration::hours(30),
    )
    .await?;
    touch(
        &pool,
        ws,
        b,
        TouchKind::Invite,
        Some(live),
        now - Duration::hours(2),
    )
    .await?;
    ensure!(breaches(&pool, ws.into_uuid()).await?.is_empty());
    ensure!(halted(&pool, ws.into_uuid()).await.is_none());

    // Over rate: the same person twice inside the one-voice window.
    let ws = workspace(&pool).await?;
    let p = prospect(&pool, ws, "kuba").await?;
    touch(
        &pool,
        ws,
        p,
        TouchKind::Engage,
        None,
        now - Duration::hours(30),
    )
    .await?;
    touch(
        &pool,
        ws,
        p,
        TouchKind::Engage,
        None,
        now - Duration::hours(2),
    )
    .await?;
    ensure!(breaches(&pool, ws.into_uuid()).await? == [Breach::OverRate]);
    ensure!(halted(&pool, ws.into_uuid()).await == Some(vec![Breach::OverRate]));
    // ...and spaced beyond the window is fine.
    let ws = workspace(&pool).await?;
    let p = prospect(&pool, ws, "kuba").await?;
    touch(
        &pool,
        ws,
        p,
        TouchKind::Engage,
        None,
        now - Duration::days(5),
    )
    .await?;
    touch(
        &pool,
        ws,
        p,
        TouchKind::Engage,
        None,
        now - Duration::hours(2),
    )
    .await?;
    ensure!(breaches(&pool, ws.into_uuid()).await?.is_empty());

    // Over rate: more touches in a day than the cap, each to a different person.
    let ws = workspace(&pool).await?;
    for i in 0..=crowdrelay_infra::scout_lane::DAILY_TOUCH_CAP {
        let p = prospect(&pool, ws, &format!("fan{i}")).await?;
        touch(
            &pool,
            ws,
            p,
            TouchKind::Engage,
            None,
            now - Duration::hours(1),
        )
        .await?;
    }
    ensure!(breaches(&pool, ws.into_uuid()).await? == [Breach::OverRate]);

    // Contacted after a no: refused first, spoken to after.
    let ws = workspace(&pool).await?;
    let p = prospect(&pool, ws, "grumpy").await?;
    touch(
        &pool,
        ws,
        p,
        TouchKind::Engage,
        None,
        now - Duration::days(4),
    )
    .await?;
    sqlx::query(
        "UPDATE fan_prospects SET status='refused', status_reason='asked to stop', updated_at=$3
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(ws.into_uuid())
    .bind(p)
    .bind(now - Duration::days(2))
    .execute(&pool)
    .await?;
    ensure!(
        breaches(&pool, ws.into_uuid()).await?.is_empty(),
        "a touch BEFORE the no is history, not a breach"
    );
    touch(
        &pool,
        ws,
        p,
        TouchKind::Engage,
        None,
        now - Duration::hours(1),
    )
    .await?;
    ensure!(breaches(&pool, ws.into_uuid()).await? == [Breach::ContactedAfterNo]);

    // Invite without a route: nothing the person said in a thread the band reads.
    let ws = workspace(&pool).await?;
    let p = prospect(&pool, ws, "stranger").await?;
    let l = link(&pool, ws, true).await?;
    touch(
        &pool,
        ws,
        p,
        TouchKind::Invite,
        Some(l),
        now - Duration::hours(3),
    )
    .await?;
    sqlx::query("DELETE FROM fan_prospect_observations WHERE workspace_id=$1 AND prospect_id=$2")
        .bind(ws.into_uuid())
        .bind(p)
        .execute(&pool)
        .await?;
    ensure!(breaches(&pool, ws.into_uuid()).await? == [Breach::InviteWithoutRoute]);

    // Untracked link: the invitation's link is no longer live.
    let ws = workspace(&pool).await?;
    let p = prospect(&pool, ws, "kuba").await?;
    let dead = link(&pool, ws, true).await?;
    touch(
        &pool,
        ws,
        p,
        TouchKind::Invite,
        Some(dead),
        now - Duration::hours(3),
    )
    .await?;
    sqlx::query("UPDATE smart_links SET active=false WHERE id=$1")
        .bind(dead)
        .execute(&pool)
        .await?;
    ensure!(breaches(&pool, ws.into_uuid()).await? == [Breach::UntrackedLink]);

    // Another tenant's lane is not this tenant's.
    let other = workspace(&pool).await?;
    ensure!(breaches(&pool, other.into_uuid()).await?.is_empty());
    Ok(())
}
