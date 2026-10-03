//! The prospect sweep, driven through the real worker against a migrated
//! database: people who commented under a post on a surface the band controls
//! become prospects with their words and the post they were under; running it
//! again changes nothing; a person who said no is not collected against; and a
//! prospect nobody has spoken to in the retention window is deleted.

use crate::common;

use anyhow::{Context, Result, ensure};
use crowdrelay_domain::{
    WorkspaceId,
    fan_prospect::{ProspectIdentityExclusionReason, ProspectIdentityKind},
};
use crowdrelay_worker::prospect_sweep::{ProspectSweep, SweepReport};
use sqlx::PgPool;
use std::time::Duration;
use time::{Duration as Span, OffsetDateTime};
use uuid::Uuid;

use super::community_relay_batch::workspace;

async fn video(pool: &PgPool, ws: WorkspaceId) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO content_sources (id, workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
         VALUES ($1,$2,'video',$3,'Technophobia', now(), now() + interval '90 days',
                 '{\"url\": \"https://youtu.be/abc123\"}'::jsonb)",
    )
    .bind(id)
    .bind(ws.into_uuid())
    .bind(format!("youtube:{}", id.simple()))
    .execute(pool)
    .await
    .context("insert content source")?;
    Ok(id)
}

async fn comment(
    pool: &PgPool,
    ws: WorkspaceId,
    source: Uuid,
    platform: &str,
    author: &str,
    body: &str,
    days_ago: i64,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO community_comments
             (id, workspace_id, platform_comment_id, parent_id, author, body, status, platform,
              content_source_id, created_at)
         VALUES ($1,$2,$3,'18088912784228243',$4,$5,'skipped',$6,$7, now() - make_interval(days => $8::int))",
    )
    .bind(id)
    .bind(ws.into_uuid())
    .bind((id.as_u128() % 100_000_000_000_000_000).to_string())
    .bind(author)
    .bind(body)
    .bind(platform)
    .bind(source)
    .bind(i32::try_from(days_ago)?)
    .execute(pool)
    .await
    .context("insert comment")?;
    Ok(id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn commenters_become_prospects_once_and_a_no_stays_a_no() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let source = video(&pool, ws).await?;
    comment(
        &pool,
        ws,
        source,
        "instagram",
        "kuba_metal",
        "Kiedy gracie Wrocław?",
        1,
    )
    .await?;
    comment(
        &pool,
        ws,
        source,
        "instagram",
        "Kuba_Metal",
        "Dajcie znać jak będzie bilet",
        0,
    )
    .await?;
    comment(
        &pool,
        ws,
        source,
        "youtube",
        "Ania Rock",
        "same words as a name",
        0,
    )
    .await?;
    comment(&pool, ws, source, "youtube", "@zine_pl", "Świetny numer", 2).await?;
    // Older than the lookback: history, not a signal.
    comment(
        &pool,
        ws,
        source,
        "instagram",
        "old_timer",
        "kiedyś tu byłem",
        45,
    )
    .await?;

    let sweep = ProspectSweep::new(pool.clone(), ws, Duration::from_secs(10));
    let now = OffsetDateTime::now_utc();
    let report = sweep.run_once(now).await?;
    ensure!(
        report
            == SweepReport {
                created: 2,
                appended: 1,
                already_known: 0,
                not_collected: 0,
                excluded_identity: 0,
                not_an_identity: 1,
                own_accounts: 0,
                own_retracted: 0,
                touched: 0,
                converted: 0,
                expired: 0,
            },
        "first pass: {report:?}"
    );
    let again = sweep.run_once(now).await?;
    ensure!(
        again
            == SweepReport {
                created: 0,
                appended: 0,
                already_known: 3,
                not_collected: 0,
                excluded_identity: 0,
                not_an_identity: 1,
                own_accounts: 0,
                own_retracted: 0,
                touched: 0,
                converted: 0,
                expired: 0,
            },
        "second pass changes nothing: {again:?}"
    );

    let (asked, confidence): (String, i16) = sqlx::query_as(
        "SELECT observation_kind, confidence_basis_points FROM fan_prospect_observations
         WHERE workspace_id = $1 AND evidence = 'Kiedy gracie Wrocław?'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        (asked.as_str(), confidence) == ("asked_about_show", 8_000),
        "a public question about a show is the strongest evidence this source gives: {asked} {confidence}"
    );
    let (evidence, url, kind): (String, Option<String>, String) = sqlx::query_as(
        "SELECT o.evidence, o.source_url, o.observation_kind
         FROM fan_prospect_observations o
         JOIN fan_prospects p ON p.workspace_id = o.workspace_id AND p.id = o.prospect_id
         WHERE p.workspace_id = $1 AND ltrim(lower(p.external_identity), '@') = 'zine_pl'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        (evidence.as_str(), url.as_deref(), kind.as_str())
            == (
                "Świetny numer",
                Some("https://youtu.be/abc123"),
                "active_under_our_post"
            ),
        "verbatim words, the post they were under, and what it was"
    );
    let prospects: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fan_prospects WHERE workspace_id = $1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    ensure!(
        prospects == 2,
        "kuba_metal (one person, two spellings) and zine_pl: {prospects}"
    );

    // A person who said no is not collected against, even when they comment again.
    sqlx::query(
        "UPDATE fan_prospects SET status = 'suppressed', status_reason = 'asked to stop'
         WHERE workspace_id = $1 AND ltrim(lower(external_identity), '@') = 'zine_pl'",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    comment(&pool, ws, source, "youtube", "zine_pl", "jeszcze jedno", 0).await?;
    let after_no = sweep.run_once(now).await?;
    ensure!(
        after_no.not_collected == 2,
        "both of zine_pl's comments, old and new: {after_no:?}"
    );

    // Retention: nobody has spoken for longer than the window, so they go.
    let far = now + Span::days(120);
    let expired = sweep.run_once(far).await?;
    ensure!(
        expired.expired == 1,
        "kuba_metal lapses, the suppression is kept: {expired:?}"
    );
    let left: Vec<(String,)> = sqlx::query_as(
        "SELECT ltrim(lower(external_identity), '@') FROM fan_prospects WHERE workspace_id = $1",
    )
    .bind(ws.into_uuid())
    .fetch_all(&pool)
    .await?;
    ensure!(left == [("zine_pl".to_owned(),)], "{left:?}");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn staff_self_and_test_identities_never_enter_the_prospect_lane() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let source = video(&pool, ws).await?;

    let exclusion = crowdrelay_infra::fan_prospect_exclusions::exclude_identity(
        &pool,
        ws.into_uuid(),
        ProspectIdentityKind::PlatformHandle,
        "instagram",
        "@Wojciech_Bator",
        ProspectIdentityExclusionReason::Staff,
        "fan100-test",
    )
    .await?
    .context("valid exclusion")?;
    ensure!(exclusion.value == "wojciech_bator", "{exclusion:?}");

    let self_comment = comment(
        &pool,
        ws,
        source,
        "instagram",
        "WOJCIECH_BATOR",
        "self test",
        0,
    )
    .await?;
    // Production-harvested owned comments enter unanswered. The helper uses
    // skipped by default because most sweep tests do not exercise the reply
    // queue, so put this row into the real lane state explicitly.
    sqlx::query(
        "UPDATE community_comments SET status='unanswered', hold_reason=NULL
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(ws.into_uuid())
    .bind(self_comment)
    .execute(&pool)
    .await?;
    comment(
        &pool,
        ws,
        source,
        "instagram",
        "kuba_metal",
        "Kiedy gracie Wrocław?",
        0,
    )
    .await?;

    let sweep = ProspectSweep::new(pool.clone(), ws, Duration::from_secs(10));
    let now = OffsetDateTime::now_utc();
    let first = sweep.run_once(now).await?;
    ensure!(
        first.created == 1 && first.excluded_identity == 1,
        "staff identity is counted as an explicit exclusion, never a prospect: {first:?}"
    );
    let staff_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_prospects
         WHERE workspace_id=$1 AND lower(external_identity)='wojciech_bator'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(staff_rows == 0, "staff identity entered prospect spine");
    let (comment_status, hold_reason): (String, Option<String>) = sqlx::query_as(
        "SELECT status, hold_reason FROM community_comments
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(ws.into_uuid())
    .bind(self_comment)
    .fetch_one(&pool)
    .await?;
    ensure!(
        comment_status == "skipped"
            && hold_reason.as_deref()
                == Some("FAN SCOUT: explicit staff/own-account/test identity exclusion"),
        "excluded owned comment must leave the reply queue: {comment_status} {hold_reason:?}"
    );

    // A later explicit test exclusion suppresses a prospect that already
    // exists, but leaves its observation history intact for audit.
    let before_observations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_prospect_observations WHERE workspace_id=$1",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    crowdrelay_infra::fan_prospect_exclusions::exclude_identity(
        &pool,
        ws.into_uuid(),
        ProspectIdentityKind::PlatformHandle,
        "instagram",
        "KUBA_METAL",
        ProspectIdentityExclusionReason::Test,
        "fan100-test",
    )
    .await?
    .context("valid existing-prospect exclusion")?;
    let (status, reason): (String, Option<String>) = sqlx::query_as(
        "SELECT status, status_reason FROM fan_prospects
         WHERE workspace_id=$1 AND lower(external_identity)='kuba_metal'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        status == "suppressed" && reason.as_deref() == Some("identity_exclusion:test"),
        "{status} {reason:?}"
    );
    let after_observations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_prospect_observations WHERE workspace_id=$1",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        after_observations == before_observations,
        "suppression keeps history; it must not invent or delete evidence"
    );

    // Exclusions are tenant-scoped: the same public handle is a normal person
    // for another act unless that act explicitly excludes it too.
    let other = workspace(&pool).await?;
    let other_source = video(&pool, other).await?;
    comment(
        &pool,
        other,
        other_source,
        "instagram",
        "wojciech_bator",
        "Kiedy gracie?",
        0,
    )
    .await?;
    let other_report = ProspectSweep::new(pool.clone(), other, Duration::from_secs(10))
        .run_once(now)
        .await?;
    ensure!(
        other_report.created == 1 && other_report.excluded_identity == 0,
        "exclusion leaked across tenants: {other_report:?}"
    );
    Ok(())
}

/// The reply lane answers a commenter with the tenant's join link; the commenter
/// clicks it and later arrives as a fan. The sweep must record the reply as a
/// touch (so the queue stops recommending an answer already given), and credit
/// the fan to the prospect only through the click -> visitor -> arrival chain.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_reply_that_carried_the_join_link_becomes_a_touch_and_then_a_fan() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let source = video(&pool, ws).await?;
    let asked = comment(
        &pool,
        ws,
        source,
        "instagram",
        "kuba_metal",
        "Kiedy gracie Wrocław?",
        3,
    )
    .await?;
    let engaged = comment(
        &pool,
        ws,
        source,
        "instagram",
        "ania_rock",
        "Świetny numer",
        3,
    )
    .await?;
    let silent = comment(&pool, ws, source, "instagram", "cichy", "Super", 3).await?;
    let _ = silent;
    let sweep = ProspectSweep::new(pool.clone(), ws, Duration::from_secs(10));
    let now = OffsetDateTime::now_utc();
    sweep.run_once(now).await?;

    // The lane replied to two of them; only kuba_metal's reply carried the link.
    let replied_at = now - Span::days(2);
    for id in [asked, engaged] {
        sqlx::query(
            "UPDATE community_comments
             SET status='replied', replied_at=$3, draft='Gramy 17.10 w Gorzowie',
                 reply_comment_id='9876543210'
             WHERE workspace_id=$1 AND id=$2",
        )
        .bind(ws.into_uuid())
        .bind(id)
        .bind(replied_at)
        .execute(&pool)
        .await?;
    }
    let link: Uuid = sqlx::query_scalar(
        "INSERT INTO smart_links (workspace_id, slug, destination_url, active,
                                  channel_source, channel_community, channel_creative)
         VALUES ($1, $2, 'https://band.example/signal', true, 'instagram', $3,
                 'owned_reply_capture')
         RETURNING id",
    )
    .bind(ws.into_uuid())
    .bind(format!("reply-capture-{}", asked.simple()))
    .bind(format!("comment:{asked}"))
    .fetch_one(&pool)
    .await?;

    let first = sweep.run_once(now).await?;
    ensure!(first.touched == 2 && first.converted == 0, "{first:?}");
    let statuses: Vec<(String, String)> = sqlx::query_as(
        "SELECT ltrim(lower(p.external_identity),'@'), p.status
         FROM fan_prospects p WHERE p.workspace_id=$1 ORDER BY 1",
    )
    .bind(ws.into_uuid())
    .fetch_all(&pool)
    .await?;
    ensure!(
        statuses
            == [
                ("ania_rock".into(), "warming".into()),
                ("cichy".into(), "observed".into()),
                ("kuba_metal".into(), "invited".into()),
            ],
        "{statuses:?}"
    );
    let again = sweep.run_once(now).await?;
    ensure!(again.touched == 0, "the same send is one touch: {again:?}");

    // The queue no longer recommends answering the two who were just answered.
    let queue = crowdrelay_infra::fan_prospects::next_actions(&pool, ws.into_uuid()).await?;
    for item in &queue {
        let handle = item
            .external_identity
            .trim_start_matches('@')
            .to_lowercase();
        let expected = match handle.as_str() {
            "kuba_metal" | "ania_rock" => "hold",
            // One generic comment is deliberately weak evidence. FAN SCOUT
            // observes it; repeated warmth or a concrete question can earn a reply.
            _ => "observe",
        };
        ensure!(
            serde_json::to_value(item.action)? == expected,
            "{handle}: {:?}",
            item.action
        );
    }

    // Before any click, no conversion. A click by a visitor who never arrives
    // credits nobody; a click then an arrival as an active fan converts.
    let stranger = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO click_events (workspace_id, smart_link_id, anonymous_visitor_id, occurred_at)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(ws.into_uuid())
    .bind(link)
    .bind(stranger)
    .bind(now - Span::days(1))
    .execute(&pool)
    .await?;
    let none = sweep.run_once(now).await?;
    ensure!(none.converted == 0, "a click alone is not a fan: {none:?}");

    let visitor = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO click_events (workspace_id, smart_link_id, anonymous_visitor_id, occurred_at)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(ws.into_uuid())
    .bind(link)
    .bind(visitor)
    .bind(now - Span::hours(20))
    .execute(&pool)
    .await?;
    let fan = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1,$2,'kuba@fan.test','active')",
    )
    .bind(fan)
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_acquisition_events (workspace_id, fan_id, anonymous_visitor_id, source, request_id, occurred_at)
         VALUES ($1,$2,$3,'public_signup','req-prospect-test',$4)",
    )
    .bind(ws.into_uuid())
    .bind(fan)
    .bind(visitor)
    .bind(now - Span::hours(19))
    .execute(&pool)
    .await?;
    let converted = sweep.run_once(now).await?;
    ensure!(converted.converted == 1, "{converted:?}");
    let (status, linked): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT status, linked_fan_id FROM fan_prospects
         WHERE workspace_id=$1 AND ltrim(lower(external_identity),'@')='kuba_metal'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        (status.as_str(), linked) == ("converted", Some(fan)),
        "{status} {linked:?}"
    );
    let trace: Uuid = sqlx::query_scalar(
        "SELECT trace_id FROM fan_prospect_touches WHERE workspace_id=$1 AND smart_link_id=$2",
    )
    .bind(ws.into_uuid())
    .bind(link)
    .fetch_one(&pool)
    .await?;
    ensure!(
        !trace.is_nil(),
        "the touch carries the trace the conversion joins"
    );
    Ok(())
}

/// The band's own accounts are not its audience. An owner who tests the reply
/// lane from a personal account was collected as a prospect, replied to twice,
/// and tripped the one-voice tripwire that halts every reply sender for a week.
/// Naming the account retracts the prospect and its touches, and the sweep stops
/// collecting it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_tenants_own_accounts_are_retracted_and_not_collected() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let source = video(&pool, ws).await?;
    comment(&pool, ws, source, "instagram", "wojciech_bator", "test", 1).await?;
    comment(
        &pool,
        ws,
        source,
        "instagram",
        "real_fan",
        "Kiedy koncert?",
        1,
    )
    .await?;
    let sweep = ProspectSweep::new(pool.clone(), ws, Duration::from_secs(10));
    let now = OffsetDateTime::now_utc();
    let first = sweep.run_once(now).await?;
    ensure!(
        first.created == 2,
        "nothing names the account yet: {first:?}"
    );

    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value)
         VALUES ($1, 'scout_own_handles', '@Wojciech_Bator, someone_else')",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fanbase_connections (workspace_id, platform, external_account_ref, credential_ref, label)
         VALUES ($1, 'instagram', '17841455886865962', 'x', 'Band Instagram (@band.official)')",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    comment(&pool, ws, source, "instagram", "band.official", "dzięki", 0).await?;

    let second = sweep.run_once(now).await?;
    ensure!(
        second.own_retracted == 1 && second.own_accounts == 2,
        "one retracted, the owner's and the brand's comments skipped: {second:?}"
    );
    let left: Vec<(String,)> = sqlx::query_as(
        "SELECT ltrim(lower(external_identity), '@') FROM fan_prospects WHERE workspace_id = $1",
    )
    .bind(ws.into_uuid())
    .fetch_all(&pool)
    .await?;
    ensure!(left == [("real_fan".to_owned(),)], "{left:?}");
    let third = sweep.run_once(now).await?;
    ensure!(third.own_retracted == 0, "idempotent: {third:?}");
    Ok(())
}
