//! K against a real schema: the staggered windows, retained-versus-merely-qualified
//! referrals, the Latarnik restriction, tenant isolation, and the evidence floor
//! on what a reader is told.

use crate::common;

use anyhow::{Result, ensure};
use crowdrelay_domain::{
    WorkspaceId,
    latarnik::{FanEvidence, RoleStatus},
    viral_coefficient::{Withheld, read},
};
use crowdrelay_infra::{
    latarnik_roles::{record_candidate, transition_role},
    viral_coefficient::k_counts,
};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::community_relay_batch::workspace;

async fn fan(pool: &PgPool, ws: WorkspaceId, email: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status, created_at)
         VALUES ($1,$2,$3,'active', now() - interval '200 days')",
    )
    .bind(id)
    .bind(ws.into_uuid())
    .bind(email)
    .execute(pool)
    .await?;
    Ok(id)
}

/// A meaningful action `days_ago`: an event interest, the cheapest first-party row.
async fn acted(pool: &PgPool, ws: WorkspaceId, fan_id: Uuid, days_ago: i32) -> Result<()> {
    let event = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,'Show', now() + interval '30 days','published', now())",
    )
    .bind(event)
    .bind(ws.into_uuid())
    .bind(format!("show-{}", event.simple()))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO event_interests (workspace_id, event_id, fan_id, created_at)
         VALUES ($1,$2,$3, now() - make_interval(days => $4))",
    )
    .bind(ws.into_uuid())
    .bind(event)
    .bind(fan_id)
    .bind(days_ago)
    .execute(pool)
    .await?;
    Ok(())
}

async fn referral(
    pool: &PgPool,
    ws: WorkspaceId,
    referrer: Uuid,
    referred: Uuid,
    qualified_days_ago: i32,
    status: &str,
) -> Result<()> {
    let code: Uuid = sqlx::query_scalar(
        "INSERT INTO referral_codes (workspace_id, fan_id, code)
         VALUES ($1,$2, encode(gen_random_bytes(18),'hex'))
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(ws.into_uuid())
    .bind(referrer)
    .fetch_optional(pool)
    .await?
    .unwrap_or_default();
    let code = if code.is_nil() {
        sqlx::query_scalar(
            "SELECT id FROM referral_codes WHERE workspace_id=$1 AND fan_id=$2 AND active LIMIT 1",
        )
        .bind(ws.into_uuid())
        .bind(referrer)
        .fetch_one(pool)
        .await?
    } else {
        code
    };
    sqlx::query(
        "INSERT INTO referral_attributions
             (workspace_id, referrer_fan_id, referred_fan_id, referral_code_id, accepted_at,
              status, qualified_at, rejected_at, reversed_at)
         VALUES ($1,$2,$3,$4, now() - make_interval(days => $5), $6,
                 CASE WHEN $6 IN ('qualified', 'reversed') THEN now() - make_interval(days => $5) END,
                 CASE WHEN $6 = 'rejected' THEN now() - make_interval(days => $5) END,
                 CASE WHEN $6 = 'reversed' THEN now() - make_interval(days => 1) END)",
    )
    .bind(ws.into_uuid())
    .bind(referrer)
    .bind(referred)
    .bind(code)
    .bind(qualified_days_ago)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn k_counts_only_qualified_referrals_that_were_retained_in_staggered_windows() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let now = OffsetDateTime::now_utc();

    // Cohort: four fans who did something 60-90 days ago (the floor is four).
    let mut cohort = Vec::new();
    for i in 0..4 {
        let f = fan(&pool, ws, &format!("c{i}@fan.test")).await?;
        acted(&pool, ws, f, 75).await?;
        cohort.push(f);
    }
    // A fan who was active only recently is not in the cohort, and what they
    // refer does not count.
    let newcomer = fan(&pool, ws, "new@fan.test").await?;
    acted(&pool, ws, newcomer, 5).await?;

    // c0 brought two people in the referral window: one is still active (retained),
    // one never did anything again. c1 brought one that qualified but was rejected
    // later (not counted), c2 one that qualified too early (outside the window).
    let (kept, lapsed, rejected, early, late) = (
        fan(&pool, ws, "kept@fan.test").await?,
        fan(&pool, ws, "lapsed@fan.test").await?,
        fan(&pool, ws, "rejected@fan.test").await?,
        fan(&pool, ws, "early@fan.test").await?,
        fan(&pool, ws, "late@fan.test").await?,
    );
    acted(&pool, ws, kept, 3).await?;
    acted(&pool, ws, early, 3).await?;
    acted(&pool, ws, late, 3).await?;
    referral(&pool, ws, cohort[0], kept, 45, "qualified").await?;
    referral(&pool, ws, cohort[0], lapsed, 40, "qualified").await?;
    referral(&pool, ws, cohort[1], rejected, 40, "rejected").await?;
    // Qualified, then reversed (chargeback, fraud, unsubscribed-and-refunded): it
    // keeps its qualification timestamp but is no longer a referral that counts.
    let reversed = fan(&pool, ws, "reversed@fan.test").await?;
    acted(&pool, ws, reversed, 3).await?;
    referral(&pool, ws, cohort[1], reversed, 42, "reversed").await?;
    referral(&pool, ws, cohort[2], early, 70, "qualified").await?; // before the window
    referral(&pool, ws, cohort[3], late, 10, "qualified").await?; // too recent to be retained
    referral(
        &pool,
        ws,
        newcomer,
        fan(&pool, ws, "x@fan.test").await?,
        45,
        "qualified",
    )
    .await?;

    let series = k_counts(&pool, ws.into_uuid(), now).await?;
    ensure!(series.fans.cohort == 4, "{:?}", series.fans);
    ensure!(
        series.fans.qualified_referrals == 2 && series.fans.retained_referrals == 1,
        "only the two qualified referrals inside the window; one of them retained: {:?}",
        series.fans
    );
    let k = read(series.fans);
    ensure!(
        k.k_milli == Some(250),
        "1 retained over a cohort of 4: {k:?}"
    );

    // No Latarnik yet: an empty cohort is withheld, never reported as K = 0.
    ensure!(series.latarnik.cohort == 0);
    let l = read(series.latarnik);
    ensure!(l.k_milli.is_none() && l.withheld == Some(Withheld::CohortBelowFloor));

    // Another tenant sees none of it.
    let other = workspace(&pool).await?;
    let theirs = k_counts(&pool, other.into_uuid(), now).await?;
    ensure!(theirs.fans.cohort == 0 && theirs.fans.qualified_referrals == 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_latarnik_series_is_the_cohort_who_held_the_role_and_what_they_brought() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let w = ws.into_uuid();
    let now = OffsetDateTime::now_utc();
    let evidence = FanEvidence {
        account_open: true,
        consented: true,
        tenure_days: 200,
        active_now: true,
        active_before: true,
        distinct_actions_90d: 3,
        attended_show_90d: true,
        has_purchased: false,
        qualified_referrals: 0,
        suppressed_in_any_role: false,
        already_asked: false,
    };
    // Kuba holds an active role since 70 days ago; Ania's is only a candidate.
    let kuba = fan(&pool, ws, "kuba@fan.test").await?;
    let ania = fan(&pool, ws, "ania@fan.test").await?;
    let backdated = now - Duration::days(70);
    let role = record_candidate(&pool, w, "kuba@fan.test", &evidence, backdated)
        .await?
        .ok_or_else(|| anyhow::anyhow!("role"))?;
    transition_role(&pool, w, role, RoleStatus::Invited, None, backdated).await?;
    transition_role(&pool, w, role, RoleStatus::Active, None, backdated).await?;
    record_candidate(&pool, w, "ania@fan.test", &evidence, backdated).await?;
    let (brought, ignored) = (
        fan(&pool, ws, "brought@fan.test").await?,
        fan(&pool, ws, "ignored@fan.test").await?,
    );
    acted(&pool, ws, brought, 2).await?;
    acted(&pool, ws, ignored, 2).await?;
    referral(&pool, ws, kuba, brought, 45, "qualified").await?;
    // Ania never held the role, so her referral is not the role's doing.
    referral(&pool, ws, ania, ignored, 45, "qualified").await?;

    let series = k_counts(&pool, w, now).await?;
    ensure!(series.latarnik.cohort == 1, "{:?}", series.latarnik);
    ensure!(
        series.latarnik.qualified_referrals == 1 && series.latarnik.retained_referrals == 1,
        "{:?}",
        series.latarnik
    );
    // One Latarnik is below the floor: the count is shown, K is withheld.
    let reading = read(series.latarnik);
    ensure!(reading.k_milli.is_none() && reading.counts.retained_referrals == 1);
    Ok(())
}
