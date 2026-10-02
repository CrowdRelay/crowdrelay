//! The Latarnik sweep and role against a real schema.
//!
//! What only the database proves: the evidence loader's SQL reads the right
//! first-party rows (consent, tenure, sessions, event interests, do-not-contact
//! elsewhere) and agrees with the canonical activation function; a candidate is
//! one person-keyed role however many times the fan is detected; the status
//! machine is enforced by the writer and by the schema; the list says a person
//! is also a fan without saying who; and erasing the fan erases the role.
//!
//! The deep path (stayed active across two windows *and* was in the room or
//! bought) is covered by the evaluator's unit tests; building a ticket sale or
//! a QR campaign here would test those fixtures, not the sweep.

use crate::common;

use std::time::Duration;

use anyhow::{Result, ensure};
use crowdrelay_domain::{
    WorkspaceId,
    latarnik::{FanEvidence, RoleStatus},
};
use crowdrelay_infra::latarnik_roles::{
    LatarnikError, list_roles, load_fan_evidence, record_candidate, transition_role,
};
use crowdrelay_worker::latarnik_sweep::{LatarnikSweep, SweepReport};
use sqlx::PgPool;
use time::{Duration as Span, OffsetDateTime};
use uuid::Uuid;

use super::community_relay_batch::workspace;

async fn fan(
    pool: &PgPool,
    ws: WorkspaceId,
    email: &str,
    tenure_days: i32,
    consent: bool,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status, created_at)
         VALUES ($1,$2,$3,'active', now() - make_interval(days => $4))",
    )
    .bind(id)
    .bind(ws.into_uuid())
    .bind(email)
    .bind(tenure_days)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
         VALUES ($1,$2,'marketing',$3,'v1','test', now() - make_interval(days => $4))",
    )
    .bind(ws.into_uuid())
    .bind(id)
    .bind(consent)
    .bind(tenure_days)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn interest(pool: &PgPool, ws: WorkspaceId, fan_id: Uuid, days_ago: i32) -> Result<()> {
    let event = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,'Show', now() + interval '30 days', 'published', now())",
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

async fn session(pool: &PgPool, ws: WorkspaceId, fan_id: Uuid) -> Result<()> {
    sqlx::query(
        "INSERT INTO fan_sessions (workspace_id, fan_id, session_token_hash, created_at, expires_at, last_seen_at)
         VALUES ($1,$2,gen_random_bytes(32), now() - interval '5 days', now() + interval '30 days',
                 now() - interval '2 days')",
    )
    .bind(ws.into_uuid())
    .bind(fan_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_loader_reads_what_a_fan_did_and_the_sweep_asks_nobody() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let now = OffsetDateTime::now_utc();

    // Engaged: stayed (interest 45d ago and 5d ago) and opened Signal.
    let engaged = fan(&pool, ws, "engaged@fan.test", 90, true).await?;
    interest(&pool, ws, engaged, 45).await?;
    interest(&pool, ws, engaged, 5).await?;
    session(&pool, ws, engaged).await?;
    // Same behaviour, but never consented, and one who opted out of contact elsewhere.
    let no_consent = fan(&pool, ws, "private@fan.test", 90, false).await?;
    interest(&pool, ws, no_consent, 5).await?;
    session(&pool, ws, no_consent).await?;
    let dnc = fan(&pool, ws, "dnc@fan.test", 90, true).await?;
    interest(&pool, ws, dnc, 5).await?;
    session(&pool, ws, dnc).await?;
    sqlx::query(
        "INSERT INTO outreach_targets
             (workspace_id, target_kind, display_name, contact_email, active, verified,
              accepts_outreach, do_not_contact)
         VALUES ($1,'press','Zine','dnc@fan.test',true,true,true,true)",
    )
    .bind(ws.into_uuid())
    .execute(&pool)
    .await?;
    // A fan of three days who has done nothing yet.
    let _new = fan(&pool, ws, "new@fan.test", 3, true).await?;

    let observed = load_fan_evidence(&pool, ws.into_uuid(), now, 100).await?;
    let by_email = |email: &str| {
        observed
            .iter()
            .find(|f| f.email == email)
            .map(|f| f.evidence)
            .ok_or_else(|| anyhow::anyhow!("no evidence for {email}"))
    };
    let e = by_email("engaged@fan.test")?;
    ensure!(
        e.consented
            && e.active_now
            && e.active_before
            && e.distinct_actions_90d == 2
            && !e.attended_show_90d
            && !e.has_purchased
            && e.qualified_referrals == 0
            && !e.suppressed_in_any_role
            && !e.already_asked
            && (89..=91).contains(&e.tenure_days),
        "{e:?}"
    );
    ensure!(!by_email("private@fan.test")?.consented);
    ensure!(by_email("dnc@fan.test")?.suppressed_in_any_role);
    let n = by_email("new@fan.test")?;
    ensure!(
        !n.active_now && n.distinct_actions_90d == 0 && n.tenure_days <= 4,
        "{n:?}"
    );

    // The sweep: the engaged fan is real but light (no room, no purchase), so it
    // is counted, not given a role; nobody was asked anything.
    let sweep = LatarnikSweep::new(pool.clone(), ws, Duration::from_secs(10));
    let report = sweep.run_once(now).await?;
    ensure!(
        report
            == SweepReport {
                fans_read: 4,
                candidates_recorded: 0,
                light_ask_ready: 1,
                referral_opportunities_recorded: 1,
                missions_offered: 0,
                missions_completed: 0,
                missions_expired: 0,
            },
        "{report:?}"
    );
    let roles: i64 =
        sqlx::query_scalar("SELECT count(*) FROM latarnik_roles WHERE workspace_id=$1")
            .bind(ws.into_uuid())
            .fetch_one(&pool)
            .await?;
    ensure!(roles == 0, "a light ask is not a role");
    let opportunities: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fan_advocacy_opportunities
         WHERE workspace_id=$1 AND kind='personal_referral' AND status='ready'",
    )
    .bind(ws.into_uuid())
    .fetch_one(&pool)
    .await?;
    ensure!(
        opportunities == 1,
        "the light ask becomes one durable opportunity"
    );

    // A second pass cannot create another ask or upgrade the same person into
    // a role behind the first plan's back.
    let second = sweep.run_once(now + Span::hours(1)).await?;
    ensure!(second.referral_opportunities_recorded == 0, "{second:?}");
    let observed_again =
        load_fan_evidence(&pool, ws.into_uuid(), now + Span::hours(1), 100).await?;
    ensure!(
        observed_again
            .iter()
            .any(|f| f.email == "engaged@fan.test" && f.evidence.already_asked)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_candidate_is_one_person_keyed_role_and_the_machine_is_enforced() -> Result<()> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let now = OffsetDateTime::now_utc();
    let fan_id = fan(&pool, ws, "kuba@fan.test", 90, true).await?;
    let evidence = FanEvidence {
        account_open: true,
        consented: true,
        tenure_days: 90,
        active_now: true,
        active_before: true,
        distinct_actions_90d: 3,
        attended_show_90d: true,
        has_purchased: false,
        qualified_referrals: 0,
        suppressed_in_any_role: false,
        already_asked: false,
    };
    let role = record_candidate(&pool, ws.into_uuid(), "kuba@fan.test", &evidence, now)
        .await?
        .ok_or_else(|| anyhow::anyhow!("first detection creates the role"))?;
    ensure!(
        record_candidate(&pool, ws.into_uuid(), "kuba@fan.test", &evidence, now)
            .await?
            .is_none(),
        "detected twice is still one role"
    );
    let persons: i64 = sqlx::query_scalar("SELECT count(*) FROM persons WHERE workspace_id=$1")
        .bind(ws.into_uuid())
        .fetch_one(&pool)
        .await?;
    ensure!(persons == 1);

    // The loader now sees the fan as already asked: one ask per person.
    let observed = load_fan_evidence(&pool, ws.into_uuid(), now, 10).await?;
    ensure!(
        observed
            .iter()
            .any(|f| f.email == "kuba@fan.test" && f.evidence.already_asked)
    );

    // Visible overlap without identity.
    let listed = list_roles(&pool, ws.into_uuid(), 10).await?;
    ensure!(listed.len() == 1 && listed[0].is_also_a_fan && listed[0].status == "candidate");
    ensure!(listed[0].evidence["attended_show_90d"] == true);
    ensure!(
        !listed[0].evidence.to_string().contains("kuba@"),
        "no address in the evidence"
    );

    // Nobody becomes active without being invited; an ended role needs a reason
    // and has no exits.
    let illegal = transition_role(&pool, ws.into_uuid(), role, RoleStatus::Active, None, now).await;
    ensure!(
        matches!(illegal, Err(LatarnikError::IllegalMove { .. })),
        "{illegal:?}"
    );
    transition_role(&pool, ws.into_uuid(), role, RoleStatus::Invited, None, now).await?;
    transition_role(
        &pool,
        ws.into_uuid(),
        role,
        RoleStatus::Active,
        None,
        now + Span::days(1),
    )
    .await?;
    let no_reason =
        transition_role(&pool, ws.into_uuid(), role, RoleStatus::Revoked, None, now).await;
    ensure!(
        matches!(no_reason, Err(LatarnikError::ReasonRequired)),
        "{no_reason:?}"
    );
    transition_role(
        &pool,
        ws.into_uuid(),
        role,
        RoleStatus::Revoked,
        Some("asked to step back"),
        now + Span::days(2),
    )
    .await?;
    let reopened =
        transition_role(&pool, ws.into_uuid(), role, RoleStatus::Active, None, now).await;
    ensure!(
        matches!(reopened, Err(LatarnikError::IllegalMove { .. })),
        "a revoked role has no exits"
    );
    // The schema refuses a role that skips its facts, whatever the writer does.
    let skipped = sqlx::query(
        "UPDATE latarnik_roles SET status='active', invited_at=NULL WHERE workspace_id=$1 AND id=$2",
    )
    .bind(ws.into_uuid())
    .bind(role)
    .execute(&pool)
    .await;
    ensure!(
        skipped.is_err(),
        "active without an invitation is refused by the schema"
    );

    // The person holds fan and role at once, and erasing the fan erases the role.
    sqlx::query(
        "DELETE FROM persons WHERE workspace_id = $1 AND id IN (
             SELECT pi.person_id FROM person_identities pi
             JOIN fans f ON f.workspace_id = pi.workspace_id AND f.normalized_email = pi.value
             WHERE pi.workspace_id = $1 AND pi.kind = 'email' AND f.id = $2)",
    )
    .bind(ws.into_uuid())
    .bind(fan_id)
    .execute(&pool)
    .await?;
    ensure!(list_roles(&pool, ws.into_uuid(), 10).await?.is_empty());
    Ok(())
}

async fn signed_in(pool: &PgPool, ws: WorkspaceId, fan_id: Uuid, token: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO fan_sessions (workspace_id, fan_id, session_token_hash, created_at, expires_at, last_seen_at)
         VALUES ($1,$2,digest($3,'sha256'), now() - interval '1 day', now() + interval '30 days', now())",
    )
    .bind(ws.into_uuid())
    .bind(fan_id)
    .bind(token)
    .execute(pool)
    .await?;
    Ok(())
}

/// The invitation is delivered in the fan's own Signal session: nothing is sent,
/// the fan sees the ask when they look, and answers through the same status
/// machine the operator uses. A candidate is invisible to them; a decline is
/// terminal (one ask is one ask); nobody else's session reaches this role.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_fan_sees_the_invitation_in_their_own_session_and_answers_through_the_machine()
-> Result<()> {
    use crowdrelay_infra::latarnik_roles::{MyAnswer, MyRoleState, answer_my_role, my_role};
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let w = ws.into_uuid();
    let now = OffsetDateTime::now_utc();
    // Session token hashes are unique across the whole database, so a rerun on
    // the same database must not reuse them.
    let kuba_token = format!("kuba-{}", Uuid::now_v7());
    let ania_token = format!("ania-{}", Uuid::now_v7());
    let kuba = fan(&pool, ws, "kuba@fan.test", 90, true).await?;
    let ania = fan(&pool, ws, "ania@fan.test", 90, true).await?;
    signed_in(&pool, ws, kuba, &kuba_token).await?;
    signed_in(&pool, ws, ania, &ania_token).await?;
    let evidence = FanEvidence {
        account_open: true,
        consented: true,
        tenure_days: 90,
        active_now: true,
        active_before: true,
        distinct_actions_90d: 3,
        attended_show_90d: true,
        has_purchased: false,
        qualified_referrals: 0,
        suppressed_in_any_role: false,
        already_asked: false,
    };

    // No session, no answer.
    ensure!(my_role(&pool, w, "nobody").await?.is_none());
    // A fan with no role sees nothing.
    ensure!(my_role(&pool, w, &ania_token).await?.map(|r| r.0) == Some(MyRoleState::None));

    // The band's internal candidate is not shown: nothing has been asked.
    let role = record_candidate(&pool, w, "kuba@fan.test", &evidence, now)
        .await?
        .ok_or_else(|| anyhow::anyhow!("candidate"))?;
    ensure!(my_role(&pool, w, &kuba_token).await?.map(|r| r.0) == Some(MyRoleState::None));
    let early = answer_my_role(&pool, w, &kuba_token, MyAnswer::Accept, now).await;
    ensure!(matches!(early, Err(LatarnikError::NotFound)), "{early:?}");

    // The operator asks; the fan sees it — and only that fan.
    transition_role(&pool, w, role, RoleStatus::Invited, None, now).await?;
    ensure!(my_role(&pool, w, &kuba_token).await?.map(|r| r.0) == Some(MyRoleState::Invited));
    ensure!(my_role(&pool, w, &ania_token).await?.map(|r| r.0) == Some(MyRoleState::None));
    let wrong = answer_my_role(&pool, w, &kuba_token, MyAnswer::Pause, now).await;
    ensure!(
        matches!(wrong, Err(LatarnikError::IllegalMove { .. })),
        "an answer must fit: {wrong:?}"
    );

    // Accepting makes them active with the one capability slice 2 grants; the
    // same person now visibly holds fan and Latarnik roles at once.
    ensure!(
        answer_my_role(&pool, w, &kuba_token, MyAnswer::Accept, now).await? == RoleStatus::Active
    );
    let listed = list_roles(&pool, w, 10).await?;
    ensure!(listed[0].status == "active" && listed[0].is_also_a_fan);
    let caps: serde_json::Value = sqlx::query_scalar(
        "SELECT capabilities FROM latarnik_roles WHERE workspace_id=$1 AND id=$2",
    )
    .bind(w)
    .bind(role)
    .fetch_one(&pool)
    .await?;
    ensure!(caps == serde_json::json!(["referral_link"]), "{caps}");
    ensure!(
        answer_my_role(&pool, w, &kuba_token, MyAnswer::Pause, now).await? == RoleStatus::Paused
    );
    ensure!(
        answer_my_role(&pool, w, &kuba_token, MyAnswer::Resume, now).await? == RoleStatus::Active
    );

    // Leaving is terminal and recorded; the band never asks again.
    ensure!(
        answer_my_role(&pool, w, &kuba_token, MyAnswer::Leave, now).await? == RoleStatus::Revoked
    );
    ensure!(my_role(&pool, w, &kuba_token).await?.map(|r| r.0) == Some(MyRoleState::Ended));
    let reopened = answer_my_role(&pool, w, &kuba_token, MyAnswer::Accept, now).await;
    ensure!(
        matches!(reopened, Err(LatarnikError::IllegalMove { .. })),
        "{reopened:?}"
    );
    let observed = load_fan_evidence(&pool, w, now, 10).await?;
    ensure!(
        observed
            .iter()
            .any(|f| f.email == "kuba@fan.test" && f.evidence.already_asked),
        "an ended role is still an ask that was made"
    );

    // A decline is the same: terminal, with its reason on the row.
    let ania_role = record_candidate(&pool, w, "ania@fan.test", &evidence, now)
        .await?
        .ok_or_else(|| anyhow::anyhow!("candidate"))?;
    transition_role(&pool, w, ania_role, RoleStatus::Invited, None, now).await?;
    ensure!(
        answer_my_role(&pool, w, &ania_token, MyAnswer::Decline, now).await? == RoleStatus::Revoked
    );
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT status_reason FROM latarnik_roles WHERE workspace_id=$1 AND id=$2",
    )
    .bind(w)
    .bind(ania_role)
    .fetch_one(&pool)
    .await?;
    ensure!(
        reason.as_deref() == Some("declined_by_person"),
        "{reason:?}"
    );
    Ok(())
}

async fn make_active_latarnik(
    pool: &PgPool,
    ws: WorkspaceId,
    email: &str,
    token: &str,
    locale: &str,
    now: OffsetDateTime,
) -> Result<(Uuid, Uuid)> {
    use crowdrelay_infra::latarnik_roles::{MyAnswer, answer_my_role};
    let w = ws.into_uuid();
    let fan_id = fan(pool, ws, email, 90, true).await?;
    sqlx::query("UPDATE fans SET locale = $3 WHERE workspace_id=$1 AND id=$2")
        .bind(w)
        .bind(fan_id)
        .bind(locale)
        .execute(pool)
        .await?;
    signed_in(pool, ws, fan_id, token).await?;
    let evidence = FanEvidence {
        account_open: true,
        consented: true,
        tenure_days: 90,
        active_now: true,
        active_before: true,
        distinct_actions_90d: 3,
        attended_show_90d: true,
        has_purchased: false,
        qualified_referrals: 0,
        suppressed_in_any_role: false,
        already_asked: false,
    };
    let role = record_candidate(pool, w, email, &evidence, now)
        .await?
        .ok_or_else(|| anyhow::anyhow!("candidate"))?;
    transition_role(pool, w, role, RoleStatus::Invited, None, now).await?;
    answer_my_role(pool, w, token, MyAnswer::Accept, now).await?;
    Ok((fan_id, role))
}

/// A mission is one real thing, offered inside the Latarnik's own session, one at
/// a time, and completed only by someone it brought — never by a tap.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_mission_is_offered_once_tapped_in_private_and_completed_only_by_a_referral() -> Result<()>
{
    use crowdrelay_infra::latarnik_missions::{
        MissionAnswer, answer_my_mission, my_open_mission, settle,
    };
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let w = ws.into_uuid();
    let now = OffsetDateTime::now_utc();
    let kuba_token = format!("mission-kuba-{}", Uuid::now_v7());
    let ania_token = format!("mission-ania-{}", Uuid::now_v7());
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value)
         VALUES ($1, 'member_site_base_url', 'https://band.example/')",
    )
    .bind(w)
    .execute(&pool)
    .await?;

    let (kuba, kuba_role) =
        make_active_latarnik(&pool, ws, "kuba@fan.test", &kuba_token, "pl-PL", now).await?;
    let (_ania, _ania_role) =
        make_active_latarnik(&pool, ws, "ania@fan.test", &ania_token, "en-GB", now).await?;

    // Accepting gave each their own referral code and the one capability.
    let codes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM referral_codes WHERE workspace_id=$1 AND active")
            .bind(w)
            .fetch_one(&pool)
            .await?;
    ensure!(codes == 2, "{codes}");

    // Nothing real to ask yet: no show, no release. No mission, nothing invented.
    let sweep = LatarnikSweep::new(pool.clone(), ws, Duration::from_secs(10));
    ensure!(sweep.run_once(now).await?.missions_offered == 0);
    ensure!(my_open_mission(&pool, w, &kuba_token, now).await?.is_none());

    // A published show in Kuba's city and a fresh release.
    let city: Uuid = sqlx::query_scalar("SELECT id FROM cities LIMIT 1")
        .fetch_one(&pool)
        .await?;
    sqlx::query("INSERT INTO fan_city_interests (workspace_id, fan_id, city_id) VALUES ($1,$2,$3)")
        .bind(w)
        .bind(kuba)
        .bind(city)
        .execute(&pool)
        .await?;
    let show = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, city_id, slug, title, starts_at, status, published_at)
         VALUES ($1,$2,$3,'gorzow','Gorzów show', now() + interval '12 days','published', now())",
    )
    .bind(show)
    .bind(w)
    .bind(city)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO content_sources (workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
         VALUES ($1,'video','youtube:abc','Technophobia', now() - interval '3 days', now() + interval '60 days', '{}'::jsonb)",
    )
    .bind(w)
    .execute(&pool)
    .await?;

    let offered = sweep.run_once(now).await?;
    ensure!(offered.missions_offered == 2, "{offered:?}");
    ensure!(
        sweep.run_once(now).await?.missions_offered == 0,
        "one open mission each"
    );

    let mine = my_open_mission(&pool, w, &kuba_token, now)
        .await?
        .ok_or_else(|| anyhow::anyhow!("mission"))?;
    ensure!(
        mine.kind == "show_one_person" && mine.status == "offered",
        "{mine:?}"
    );
    ensure!(
        mine.prompt.contains("jedną osobę"),
        "Polish for a pl-PL fan: {}",
        mine.prompt
    );
    ensure!(
        mine.share_text.contains("https://band.example/r/")
            && mine.share_text.contains("?event=gorzow&lang=pl")
            && !mine.share_text.contains("//r/"),
        "their own contextual link on the stored site root: {}",
        mine.share_text
    );
    let hers = my_open_mission(&pool, w, &ania_token, now)
        .await?
        .ok_or_else(|| anyhow::anyhow!("mission"))?;
    ensure!(
        hers.kind == "release_one_person" && hers.prompt.contains("one person"),
        "{hers:?}"
    );
    ensure!(hers.id != mine.id);

    // Nobody can answer someone else's mission, and a stranger learns nothing.
    ensure!(!answer_my_mission(&pool, w, &ania_token, mine.id, MissionAnswer::Tap, now).await?);
    ensure!(!answer_my_mission(&pool, w, "nobody", mine.id, MissionAnswer::Tap, now).await?);

    // A tap is recorded and completes nothing.
    ensure!(answer_my_mission(&pool, w, &kuba_token, mine.id, MissionAnswer::Tap, now).await?);
    let (done, _) = settle(&pool, w, now + Span::hours(1)).await?;
    ensure!(done == 0, "a tap earns nothing");

    // Someone Kuba brought arrives through his code after the tap: completed.
    let friend = fan(&pool, ws, "friend@fan.test", 1, true).await?;
    let code_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM referral_codes WHERE workspace_id=$1 AND fan_id=$2 AND active",
    )
    .bind(w)
    .bind(kuba)
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO referral_attributions
             (workspace_id, referrer_fan_id, referred_fan_id, referral_code_id, accepted_at,
              status, qualified_at)
         VALUES ($1,$2,$3,$4,$5,'qualified',$5)",
    )
    .bind(w)
    .bind(kuba)
    .bind(friend)
    .bind(code_id)
    .bind(now + Span::hours(2))
    .execute(&pool)
    .await?;
    let (done, _) = settle(&pool, w, now + Span::hours(3)).await?;
    ensure!(done == 1, "{done}");
    let status: String =
        sqlx::query_scalar("SELECT status FROM latarnik_missions WHERE workspace_id=$1 AND id=$2")
            .bind(w)
            .bind(mine.id)
            .fetch_one(&pool)
            .await?;
    ensure!(status == "completed");
    ensure!(my_open_mission(&pool, w, &kuba_token, now).await?.is_none());

    // Ania's untouched mission runs out its time and frees her slot; the
    // cooldown from its offer then still holds her back from a new one.
    let (_, expired) = settle(&pool, w, now + Span::days(11)).await?;
    ensure!(expired == 1, "{expired}");
    ensure!(
        sweep.run_once(now + Span::days(11)).await?.missions_offered == 0,
        "inside the cooldown from the last offer, and Kuba's too"
    );
    let _ = kuba_role;
    Ok(())
}
