//! Release tier round-trip and milestone marks against a real Postgres.
//!
//! What fails here and nowhere else: an upsert that drops `tier` under
//! COALESCE semantics (absent must keep the stored value, not reset it), a
//! snapshot row that forgets to read the new column, and a milestone-marks
//! query that returns rows for the wrong workspace.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::{
    ActionSubject, AutopilotActionPayload, AutopilotActionRepository, AutopilotContext,
    AutopilotDecisionRepository, AutopilotTeamStateRepository, DecisionCandidate,
    UpsertReleasePlan,
};
use crowdrelay_application::{IdempotencyKey, RepositoryError};
use crowdrelay_domain::{WorkspaceId, release_autopilot::ReleaseTier};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;

struct Fixture {
    repository: PostgresAutopilotRepository,
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("{label}-{suffix}"))
        .bind("Release tier E2E")
        .execute(&pool)
        .await?;
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    );
    Ok(Fixture {
        repository,
        pool,
        workspace_id,
        now: OffsetDateTime::now_utc(),
    })
}

fn idem(seed: &str) -> IdempotencyKey {
    IdempotencyKey::parse(format!("release-tier-e2e-{seed}")).expect("bounded key")
}

fn plan_cmd(source_key: &str, tier: Option<ReleaseTier>, version: i64) -> UpsertReleasePlan {
    UpsertReleasePlan {
        release_id: None,
        source_key: source_key.to_string(),
        title: format!("{source_key} title"),
        release_at: OffsetDateTime::now_utc() + time::Duration::days(60),
        listen_url: None,
        tier,
        active: true,
        assets_ready: true,
        communication_enabled: true,
        press_enabled: true,
        expected_version: version,
    }
}

#[tokio::test]
#[ignore = "needs a live postgres"]
async fn an_absent_tier_never_resets_the_bands_call() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("tier").await?;
    let idem_key = idem("tier-single");
    let created = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            plan_cmd("tier-single", Some(ReleaseTier::Single), 0),
            "admin_api_key",
            &idem_key,
            None,
        )
        .await?;
    assert!(!created.replayed);

    // A caller that does not know about tiers updates the plan — the band's
    // call must survive, not quietly reset to the default.
    let updated = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            UpsertReleasePlan {
                release_id: Some(created.release_id),
                title: "tier-single retitled".into(),
                ..plan_cmd("tier-single", None, 1)
            },
            "admin_api_key",
            &idem("tier-update"),
            None,
        )
        .await?;
    assert!(!updated.replayed);

    let plans = fixture
        .repository
        .load_release_plan_snapshots(fixture.workspace_id, fixture.now)
        .await?;
    let stored = plans
        .iter()
        .find(|p| p.release_id == created.release_id)
        .expect("the plan is listed");
    assert_eq!(stored.tier, ReleaseTier::Single);
    assert_eq!(stored.title, "tier-single retitled");

    // And a plan written without a tier lands on the honest default.
    let defaulted = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            plan_cmd("tier-default", None, 0),
            "admin_api_key",
            &idem("tier-default"),
            None,
        )
        .await?;
    let plans = fixture
        .repository
        .load_release_plan_snapshots(fixture.workspace_id, fixture.now)
        .await?;
    assert_eq!(
        plans
            .iter()
            .find(|p| p.release_id == defaulted.release_id)
            .expect("the default-tier plan is listed")
            .tier,
        ReleaseTier::Track
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a live postgres"]
async fn milestone_marks_are_workspace_scoped_and_parse_cleanly()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("marks").await?;
    let created = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            plan_cmd("marks-plan", Some(ReleaseTier::Track), 0),
            "admin_api_key",
            &idem("marks-plan"),
            None,
        )
        .await?;

    // No milestones recorded yet: the marks list is empty, never a phantom
    // completion borrowed from another workspace's plan.
    let marks = fixture
        .repository
        .load_release_milestone_marks(fixture.workspace_id, &[created.release_id])
        .await?;
    assert!(marks.is_empty());
    let foreign = fixture
        .repository
        .load_release_milestone_marks(WorkspaceId::new(), &[created.release_id])
        .await?;
    assert!(foreign.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "needs a live postgres"]
async fn an_active_release_projects_into_the_supply_chain() -> Result<(), Box<dyn std::error::Error>>
{
    // The trigger is the only writer of `release` content sources; without it
    // the Signal-first artifact chain at R-0 has no subject to evaluate. What
    // fails here and nowhere else: a projection that forgets a column, writes
    // `occurred_at` as creation time instead of the release date, or leaves
    // the source active after the band retires the plan.
    let fixture = fixture("projection").await?;
    let release_at =
        OffsetDateTime::from_unix_timestamp(fixture.now.unix_timestamp() + 30 * 86_400)?;
    let created = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            UpsertReleasePlan {
                release_id: None,
                source_key: "projection-plan".into(),
                title: "projection title".into(),
                release_at,
                listen_url: Some("https://listen.example/track".into()),
                tier: Some(ReleaseTier::Single),
                active: true,
                assets_ready: true,
                communication_enabled: true,
                press_enabled: true,
                expected_version: 0,
            },
            "admin_api_key",
            &idem("projection"),
            None,
        )
        .await?;

    let (occurred, expires, active): (OffsetDateTime, OffsetDateTime, bool) = sqlx::query_as(
        "SELECT occurred_at, expires_at, active
         FROM content_sources
         WHERE workspace_id=$1 AND source_kind='release' AND source_key=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("release:{}", created.release_id.into_uuid()))
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        occurred, release_at,
        "the chain stays dormant until release day"
    );
    assert_eq!(expires, release_at + time::Duration::days(45));
    assert!(active);
    let (meta_tier, meta_listen, meta_comm, meta_press): (
        Option<String>,
        Option<String>,
        Option<bool>,
        Option<bool>,
    ) = sqlx::query_as(
        "SELECT metadata->>'tier', metadata->>'listen_url',
                (metadata->>'communication_enabled')::boolean,
                (metadata->>'press_enabled')::boolean
         FROM content_sources
         WHERE workspace_id=$1 AND source_kind='release' AND source_key=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("release:{}", created.release_id.into_uuid()))
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(meta_tier.as_deref(), Some("single"));
    assert_eq!(meta_listen.as_deref(), Some("https://listen.example/track"));
    assert_eq!(meta_comm, Some(true));
    assert_eq!(meta_press, Some(true));

    // Retiring the plan retires the source; reviving it brings the source
    // back with the new facts, not a second row.
    fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            UpsertReleasePlan {
                release_id: Some(created.release_id),
                active: false,
                expected_version: 1,
                ..plan_cmd("projection-plan", None, 1)
            },
            "admin_api_key",
            &idem("projection-off"),
            None,
        )
        .await?;
    let dormant: bool = sqlx::query_scalar(
        "SELECT active FROM content_sources
         WHERE workspace_id=$1 AND source_key=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("release:{}", created.release_id.into_uuid()))
    .fetch_one(&fixture.pool)
    .await?;
    assert!(!dormant, "a retired plan owes the supply chain nothing");

    fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            UpsertReleasePlan {
                release_id: Some(created.release_id),
                title: "projection retitled".into(),
                tier: Some(ReleaseTier::Track),
                press_enabled: false,
                expected_version: 2,
                ..plan_cmd("projection-plan", None, 2)
            },
            "admin_api_key",
            &idem("projection-on"),
            None,
        )
        .await?;
    let (count, revived, retitled, re_tiered, re_press): (
        i64,
        bool,
        String,
        Option<String>,
        Option<bool>,
    ) = sqlx::query_as(
        "SELECT count(*)::bigint, bool_and(active),
                max(title), max(metadata->>'tier'),
                bool_and((metadata->>'press_enabled')::boolean)
         FROM content_sources
         WHERE workspace_id=$1 AND source_key=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("release:{}", created.release_id.into_uuid()))
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(count, 1, "projection stays one row per plan");
    assert!(revived);
    assert_eq!(retitled, "projection retitled");
    assert_eq!(re_tiered.as_deref(), Some("track"));
    assert_eq!(re_press, Some(false), "the plan's switches stay facts");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a live postgres"]
async fn a_flag_flip_defeats_a_queued_release_artifact() -> Result<(), Box<dyn std::error::Error>> {
    // The evaluator honors the plan's switches when it chooses the next
    // artifact; the executor honors them again against the locked row, so a
    // request queued while communication was on cannot still ship after the
    // band turns it off. What fails here and nowhere else: an execution path
    // that trusts the queue entry over the plan's newer word.
    let fixture = fixture("flagflip").await?;
    let release_at =
        OffsetDateTime::from_unix_timestamp(fixture.now.unix_timestamp() + 30 * 86_400)?;
    let created = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            UpsertReleasePlan {
                release_id: None,
                source_key: "flagflip-plan".into(),
                title: "flagflip title".into(),
                release_at,
                listen_url: Some("https://listen.example/flagflip".into()),
                tier: Some(ReleaseTier::Single),
                active: true,
                assets_ready: true,
                communication_enabled: true,
                press_enabled: true,
                expected_version: 0,
            },
            "admin_api_key",
            &idem("flagflip"),
            None,
        )
        .await?;
    let (source_id, source_version): (uuid::Uuid, i64) = sqlx::query_as(
        "SELECT id, version FROM content_sources
         WHERE workspace_id=$1 AND source_kind='release' AND source_key=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("release:{}", created.release_id.into_uuid()))
    .fetch_one(&fixture.pool)
    .await?;

    // A Signal push requested while communication was still on, queued the
    // way the content-supply evaluator writes it.
    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'content_supply','content_source',$4,'request_content_artifact',
                   9000,'auto_execute','release day signal push','{}','{}','{}',$5,$1)"#,
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}"))
    .bind(source_id)
    .bind(fixture.now)
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status,
            approved_at, approved_by, available_at)
           VALUES ($1,$2,$3,'content_supply','content.artifact.request','content_source',
                   $4,$5,$6,'queued',$7,'system:test',$7)"#,
    )
    .bind(action_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(source_id)
    .bind(format!("action:content:{source_id}:signal_push"))
    .bind(serde_json::json!({
        "kind": "request_content_artifact",
        "source_id": source_id,
        "source_version": source_version,
        "artifact": "signal_push",
        "template_key": "content.signal_push.v1",
    }))
    .bind(fixture.now)
    .execute(&fixture.pool)
    .await?;

    // The band turns communication off before the queued request runs.
    fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            UpsertReleasePlan {
                release_id: Some(created.release_id),
                communication_enabled: false,
                expected_version: 1,
                ..plan_cmd("flagflip-plan", None, 1)
            },
            "admin_api_key",
            &idem("flagflip-off"),
            None,
        )
        .await?;

    let claimed = fixture
        .repository
        .claim_due_autonomous_actions(fixture.workspace_id, 8, fixture.now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("the queued artifact request is claimable");
    let outcome = fixture
        .repository
        .execute_action(fixture.workspace_id, action, fixture.now)
        .await;
    assert!(
        matches!(outcome, Err(RepositoryError::Conflict)),
        "a flag flipped after queueing must defeat the queue entry, got {outcome:?}"
    );
    let emitted: bool = sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM outbox_events
            WHERE workspace_id=$1 AND event_type='crowdrelay.content.artifact_requested'
        )",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert!(
        !emitted,
        "no artifact request may ship against the plan's word"
    );
    Ok(())
}

/// Queues one release-milestone action the way the evaluator writes it, then
/// claims and executes it — the only path the milestone arms see in
/// production.
async fn run_release_milestone(
    fixture: &Fixture,
    release_id: crowdrelay_domain::ReleasePlanId,
    title: &str,
    release_at: OffsetDateTime,
    milestone: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision_id = uuid::Uuid::now_v7();
    let action_id = uuid::Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO autopilot_decisions
           (id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
           VALUES ($1,$2,$3,'release','release_plan',$4,'execute_release_milestone',
                   9000,'auto_execute','milestone due','{}','{}','{}',$5,$1)"#,
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("decision-{decision_id}-{milestone}"))
    .bind(release_id.into_uuid())
    .bind(fixture.now)
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO autopilot_actions
           (id, workspace_id, decision_id, context, action_kind, subject_kind,
            subject_id, idempotency_key, payload, status,
            approved_at, approved_by, available_at)
           VALUES ($1,$2,$3,'release','release.milestone.execute','release_plan',
                   $4,$5,$6,'queued',$7,'system:test',$7)"#,
    )
    .bind(action_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(release_id.into_uuid())
    .bind(format!("action:release:{release_id}:{milestone}"))
    .bind(serde_json::json!({
        "kind": "execute_release_milestone",
        "release_id": release_id.into_uuid(),
        "title": title,
        "release_at": release_at,
        "milestone": milestone,
    }))
    .bind(fixture.now)
    .execute(&fixture.pool)
    .await?;
    let claimed = fixture
        .repository
        .claim_due_autonomous_actions(fixture.workspace_id, 8, fixture.now)
        .await?;
    let action = claimed
        .iter()
        .find(|a| a.id.into_uuid() == action_id)
        .expect("the queued milestone action is claimable");
    fixture
        .repository
        .execute_action(fixture.workspace_id, action, fixture.now)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs a live postgres"]
async fn the_sustain_milestone_writes_the_r3_report_and_binds_the_release_campaign()
-> Result<(), Box<dyn std::error::Error>> {
    // The R+3 read is owed to the band whether or not the release moved
    // anything: campaign-bound arrivals sit next to the ambient window, and
    // the verdict names its own threshold. What fails here and nowhere else:
    // a report that credits ambient growth to the release, or a campaign that
    // never got bound so nothing could attribute.
    let fixture = fixture("r3report").await?;
    sqlx::query("UPDATE workspaces SET created_at = $2 WHERE id = $1")
        .bind(fixture.workspace_id.into_uuid())
        .bind(fixture.now - time::Duration::days(40))
        .execute(&fixture.pool)
        .await?;
    sqlx::query(
        "INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
         VALUES ($1, 'communication_campaigns_enabled', true, 'test')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await?;

    let release_at =
        OffsetDateTime::from_unix_timestamp(fixture.now.unix_timestamp() - 4 * 86_400)?;
    let created = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            UpsertReleasePlan {
                release_id: None,
                source_key: "r3report-plan".into(),
                title: "r3report title".into(),
                release_at,
                listen_url: Some("https://listen.example/r3report".into()),
                tier: Some(ReleaseTier::Single),
                active: true,
                assets_ready: true,
                communication_enabled: true,
                press_enabled: true,
                expected_version: 0,
            },
            "admin_api_key",
            &idem("r3report"),
            None,
        )
        .await?;

    // First milestone binds the campaign and the tracked link.
    run_release_milestone(
        &fixture,
        created.release_id,
        "r3report title",
        release_at,
        "seed_calendar",
    )
    .await?;
    let (campaign_id, link_bound): (uuid::Uuid, bool) = sqlx::query_as(
        "SELECT c.id, EXISTS(
             SELECT 1 FROM smart_links sl
             WHERE sl.workspace_id = c.workspace_id AND sl.campaign_id = c.id
               AND sl.slug LIKE 'release-%' AND sl.active
         )
         FROM campaigns c
         WHERE c.workspace_id = $1 AND c.release_plan_id = $2 AND c.active",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(created.release_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert!(
        link_bound,
        "the tracked link must bind the release campaign"
    );

    // Evidence: two baseline arrivals ten days out, one ambient arrival and
    // two campaign-bound arrivals inside the release window.
    let seed_fan = |email: &str| {
        let pool = fixture.pool.clone();
        let workspace_id = fixture.workspace_id;
        let email = email.to_string();
        async move {
            sqlx::query_scalar::<_, uuid::Uuid>(
                "INSERT INTO fans (id, workspace_id, normalized_email, status)
                 VALUES (gen_random_uuid(), $1, $2, 'active') RETURNING id",
            )
            .bind(workspace_id.into_uuid())
            .bind(&email)
            .fetch_one(&pool)
            .await
        }
    };
    let seed_acq =
        |fan: uuid::Uuid, campaign: Option<uuid::Uuid>, at: OffsetDateTime, req: &str| {
            let pool = fixture.pool.clone();
            let workspace_id = fixture.workspace_id;
            let req = req.to_string();
            async move {
                sqlx::query(
                    "INSERT INTO fan_acquisition_events
                    (workspace_id, fan_id, campaign_id, source, request_id, occurred_at)
                 VALUES ($1, $2, $3, 'public_signup', $4, $5)",
                )
                .bind(workspace_id.into_uuid())
                .bind(fan)
                .bind(campaign)
                .bind(&req)
                .bind(at)
                .execute(&pool)
                .await
            }
        };
    for idx in 0..2_i32 {
        let fan = seed_fan(&format!("baseline-{idx}@example.test")).await?;
        seed_acq(
            fan,
            None,
            fixture.now - time::Duration::days(10),
            &format!("baseline-{idx}"),
        )
        .await?;
    }
    let ambient = seed_fan("ambient@example.test").await?;
    seed_acq(
        ambient,
        None,
        fixture.now - time::Duration::days(3),
        "ambient-0",
    )
    .await?;
    for idx in 0..2_i32 {
        let fan = seed_fan(&format!("bound-{idx}@example.test")).await?;
        seed_acq(
            fan,
            Some(campaign_id),
            fixture.now - time::Duration::days(3),
            &format!("bound-{idx}"),
        )
        .await?;
    }

    // An executor registry flips ensure_executor_capability to fail-closed:
    // every emitted event kind must resolve to an advertised capability or the
    // emit returns Unavailable and the whole sustain arm rolls back. The R+3
    // report rides show.escalation, same delivery class as the T+7 report.
    sqlx::query(
        "INSERT INTO executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,'n8n-r3-test','test','test-manifest',$2,$3)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.now)
    .bind(fixture.now + time::Duration::minutes(30))
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "INSERT INTO executor_capabilities (
            workspace_id, executor_id, capability, capability_version, observed_at, expires_at
        ) VALUES ($1,'n8n-r3-test','show.escalation','1',$2,$3)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.now)
    .bind(fixture.now + time::Duration::minutes(30))
    .execute(&fixture.pool)
    .await?;

    run_release_milestone(
        &fixture,
        created.release_id,
        "r3report title",
        release_at,
        "sustain",
    )
    .await?;

    let sustain_done: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM release_milestones
         WHERE workspace_id=$1 AND release_id=$2 AND milestone='sustain')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(created.release_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert!(sustain_done, "the sustain milestone must record completion");

    let report = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT payload->'report' FROM outbox_events
         WHERE workspace_id=$1 AND event_type='crowdrelay.release.r3_report_due'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        report["observed"]["fans_acquired_via_release_campaign"],
        serde_json::json!(2),
        "bound acquisitions must be counted on the release's own numbers: {report}"
    );
    assert_eq!(
        report["inferred"]["window_acquisitions"],
        serde_json::json!(3),
        "the window counts everything that arrived, bound or not: {report}"
    );
    assert_eq!(
        report["inferred"]["verdict"],
        serde_json::json!("above_trend"),
        "3 arrivals against a 2-in-28d baseline is above trend: {report}"
    );
    let gaps = report["evidence_gaps"]
        .as_array()
        .expect("evidence_gaps is a list")
        .iter()
        .filter_map(|gap| gap.as_str())
        .collect::<Vec<_>>();
    assert!(
        gaps.contains(&"streams_not_measured"),
        "listens are never claimed without listen data: {gaps:?}"
    );
    Ok(())
}

/// §4i-2: the week holding a published show is the show's week. The collision
/// read is what the release evaluator consults before letting an
/// owned-audience milestone spend that week a second time.
#[tokio::test]
#[ignore = "needs a live postgres"]
async fn the_collision_week_names_every_live_show_and_ignores_the_rest()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("collision").await?;
    let insert = |slug: &str, title: &str, status: &str, offset: &str| {
        let pool = fixture.pool.clone();
        let workspace_id = fixture.workspace_id;
        let slug = slug.to_string();
        let title = title.to_string();
        let status = status.to_string();
        let offset = offset.to_string();
        async move {
            sqlx::query(
                "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
                 VALUES ($1, $2, $3, $4, date_trunc('week', now()) + ($5)::interval, $6,
                         CASE WHEN $6 IN ('published','completed') THEN now() ELSE NULL END)",
            )
            .bind(uuid::Uuid::now_v7())
            .bind(workspace_id.into_uuid())
            .bind(slug)
            .bind(title)
            .bind(offset)
            .bind(status)
            .execute(&pool)
            .await
        }
    };
    // The completed show sits at the week's own Monday — always at or before
    // `now` inside the week, so a finished event has an honest past starts_at.
    insert("this-monday", "Monday gig", "completed", "0 days").await?;
    insert("this-friday", "Friday gig", "published", "4 days").await?;
    insert("next-week", "Next week's gig", "published", "9 days").await?;
    insert("draft-show", "Draft gig", "draft", "1 day").await?;
    insert("cancelled-show", "Cancelled gig", "cancelled", "1 day").await?;

    // An unvalidated timezone must not break the read: the show falls back to
    // its UTC week and still collides rather than erroring the whole cycle.
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at, timezone)
         VALUES ($1, $2, 'bogus-tz', 'Weird-zone gig',
                 date_trunc('week', now()) + interval '2 days', 'published', now(), 'Mars/Olympus')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await?;

    // A different workspace's show on the same night is not this tenant's
    // collision.
    let other_workspace = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(other_workspace.into_uuid())
        .bind(format!("other-{}", other_workspace.into_uuid().simple()))
        .bind("Other")
        .execute(&fixture.pool)
        .await?;
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1, $2, 'foreign-show', 'Foreign gig', date_trunc('week', now()) + interval '3 days', 'published', now())",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(other_workspace.into_uuid())
    .execute(&fixture.pool)
    .await?;

    let shows = fixture
        .repository
        .load_colliding_show_week(fixture.workspace_id, fixture.now)
        .await?;
    let titles = shows
        .iter()
        .map(|show| show.title.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        titles,
        ["Monday gig", "Weird-zone gig", "Friday gig"],
        "both live shows this week, in order, and nothing else: {titles:?}"
    );
    Ok(())
}

/// §4i-2's central guarantee at the persistence layer: a collision-week hold
/// writes exactly one decision row and no action, and once the week clears
/// the ordinary execute key still creates the send — the hold never consumed
/// the milestone's keys.
#[tokio::test]
#[ignore = "needs a live postgres"]
async fn a_held_milestone_dedupes_in_week_and_refires_after_it_clears()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("refire").await?;
    sqlx::query(
        "INSERT INTO autopilot_policies
         (workspace_id, context, enabled, autonomy_level, max_actions_24h)
         VALUES ($1, 'release', true, 'require_approval', 10)
         ON CONFLICT (workspace_id, context) DO UPDATE
         SET enabled = true, autonomy_level = 'require_approval', max_actions_24h = 10",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await?;

    let release_id = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            UpsertReleasePlan {
                release_id: None,
                source_key: "collision-plan".into(),
                title: "Signal Lost".into(),
                release_at: fixture.now + time::Duration::days(4),
                listen_url: None,
                tier: Some(ReleaseTier::Single),
                active: true,
                assets_ready: true,
                communication_enabled: true,
                press_enabled: true,
                expected_version: 0,
            },
            "admin_api_key",
            &idem("collision-plan"),
            None,
        )
        .await?
        .release_id;
    let action = AutopilotActionPayload::ExecuteReleaseMilestone {
        release_id,
        title: "Signal Lost".to_string(),
        release_at: fixture.now + time::Duration::days(4),
        milestone: crowdrelay_domain::release_autopilot::ReleaseMilestone::FanWarmup,
    };
    let held = DecisionCandidate {
        context: AutopilotContext::Release,
        subject: ActionSubject::ReleasePlan(release_id),
        decision_kind: "hold_release_milestone_collision",
        confidence: crowdrelay_domain::autonomy::Confidence::saturating_from_basis_points(9_200),
        disposition: crowdrelay_domain::autonomy::PolicyDisposition::Deny,
        reason: "a live show this week keeps the week's attention",
        input_snapshot: serde_json::json!({
            "collision": {
                "protected_shows": [{"title": "Friday gig"}],
                "held_milestone": "fan_warmup",
            },
        }),
        policy_snapshot: serde_json::json!({}),
        action: action.clone(),
        decision_key: format!("decision:release:v1:{release_id}:fan_warmup:0:hold:2026-09-14"),
        action_idempotency_key: format!("action:release:{release_id}:fan_warmup:hold:2026-09-14"),
    };
    let trace = crowdrelay_domain::TraceContext::root(fixture.workspace_id);

    let first = fixture
        .repository
        .persist_candidate(fixture.workspace_id, &held, &trace)
        .await?;
    assert!(first.decision_created);
    assert!(!first.action_created);

    // Every later cycle in the same show week must not recount the row.
    let again = fixture
        .repository
        .persist_candidate(fixture.workspace_id, &held, &trace)
        .await?;
    assert!(
        !again.decision_created,
        "a re-evaluated hold dedupes against the row it already wrote"
    );
    let decisions = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM autopilot_decisions WHERE workspace_id=$1",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(decisions, 1);

    // The week clears: the ordinary candidate fires under its own key.
    let execute = DecisionCandidate {
        decision_kind: "execute_release_milestone",
        disposition: crowdrelay_domain::autonomy::PolicyDisposition::RequireApproval,
        reason: "release timeline has a deterministic milestone due",
        input_snapshot: serde_json::json!({}),
        decision_key: format!("decision:release:v1:{release_id}:fan_warmup:0"),
        action_idempotency_key: format!("action:release:{release_id}:fan_warmup"),
        ..held
    };
    let fired = fixture
        .repository
        .persist_candidate(fixture.workspace_id, &execute, &trace)
        .await?;
    assert!(fired.decision_created);
    assert!(
        fired.action_created,
        "the milestone re-fires after the week clears"
    );
    Ok(())
}

/// The same hold from the debt side: a milestone the collision rule held is a
/// decision the system made, and `release_milestones_missed` must not report
/// it as work the band neglected.
#[tokio::test]
#[ignore = "needs a live postgres"]
async fn a_held_milestone_is_not_growth_debt() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("debt").await?;
    sqlx::query(
        "INSERT INTO autopilot_policies
         (workspace_id, context, enabled, autonomy_level, max_actions_24h)
         VALUES ($1, 'release', true, 'require_approval', 10)
         ON CONFLICT (workspace_id, context) DO UPDATE
         SET enabled = true, autonomy_level = 'require_approval', max_actions_24h = 10",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await?;
    let release_id = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            UpsertReleasePlan {
                release_id: None,
                source_key: "held-plan".into(),
                title: "Signal Lost".into(),
                release_at: fixture.now + time::Duration::days(10),
                listen_url: None,
                tier: Some(ReleaseTier::Single),
                active: true,
                assets_ready: true,
                communication_enabled: true,
                press_enabled: true,
                expected_version: 0,
            },
            "admin_api_key",
            &idem("held-plan"),
            None,
        )
        .await?
        .release_id;

    let held = DecisionCandidate {
        context: AutopilotContext::Release,
        subject: ActionSubject::ReleasePlan(release_id),
        decision_kind: "hold_release_milestone_collision",
        confidence: crowdrelay_domain::autonomy::Confidence::saturating_from_basis_points(9_200),
        disposition: crowdrelay_domain::autonomy::PolicyDisposition::Deny,
        reason: "a live show this week keeps the week's attention",
        input_snapshot: serde_json::json!({
            "collision": {
                "protected_shows": [{"title": "Friday gig"}],
                "held_milestone": "fan_warmup",
            },
        }),
        policy_snapshot: serde_json::json!({}),
        action: AutopilotActionPayload::ExecuteReleaseMilestone {
            release_id,
            title: "Signal Lost".to_string(),
            release_at: fixture.now + time::Duration::days(10),
            milestone: crowdrelay_domain::release_autopilot::ReleaseMilestone::FanWarmup,
        },
        decision_key: format!("decision:release:v1:{release_id}:fan_warmup:0:hold:2026-09-14"),
        action_idempotency_key: format!("action:release:{release_id}:fan_warmup:hold:2026-09-14"),
    };
    let trace = crowdrelay_domain::TraceContext::root(fixture.workspace_id);
    fixture
        .repository
        .persist_candidate(fixture.workspace_id, &held, &trace)
        .await?;

    let debts = fixture
        .repository
        .load_growth_debt_observations(fixture.workspace_id, fixture.now)
        .await?;
    let missed = debts
        .iter()
        .find(|debt| {
            debt.subject
                == crowdrelay_domain::growth_debt::GrowthDebtSubject::ReleasePlan(release_id)
        })
        .expect("an active plan with unsent milestones still reports a row");
    // Ten rungs tracked (press on), none completed, one deliberately held:
    // the debt is the nine the band still owes, not ten.
    assert_eq!(missed.tracked_items, 10);
    assert_eq!(missed.outstanding_items, 9);
    Ok(())
}

/// §4i-0c: a demo is posted or it is not — a filler-tier plan owes no assets
/// gate, so `release_assets_missing` must not fire on it. The track-tier plan
/// beside it proves the gate still works for the releases it exists to guard.
#[tokio::test]
#[ignore = "needs a live postgres"]
async fn a_filler_plan_owes_no_assets_gate() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("filler-assets").await?;
    for (key, tier) in [
        ("demo-filler", Some(ReleaseTier::Filler)),
        ("real-track", Some(ReleaseTier::Track)),
    ] {
        fixture
            .repository
            .upsert_release_plan(
                fixture.workspace_id,
                UpsertReleasePlan {
                    release_id: None,
                    source_key: key.into(),
                    title: format!("{key} title"),
                    release_at: fixture.now + time::Duration::days(30),
                    listen_url: None,
                    tier,
                    active: true,
                    assets_ready: false,
                    communication_enabled: true,
                    press_enabled: true,
                    expected_version: 0,
                },
                "admin_api_key",
                &idem(key),
                None,
            )
            .await?;
    }

    let debts = fixture
        .repository
        .load_growth_debt_observations(fixture.workspace_id, fixture.now)
        .await?;
    let asset_debts: Vec<_> = debts
        .iter()
        .filter(|debt| {
            debt.kind == crowdrelay_domain::growth_debt::GrowthDebtKind::ReleaseAssetsMissing
        })
        .collect();
    assert_eq!(
        asset_debts.len(),
        1,
        "only the track-tier plan owes the assets gate"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a live postgres"]
async fn the_countdown_tags_every_likely_listener_in_one_pass()
-> Result<(), Box<dyn std::error::Error>> {
    // The countdown milestone tags its ranked listeners inside the dispatch
    // transaction — the batch insert is what this pins: every consented fan
    // lands the tag, and a second run rewrites nothing (ON CONFLICT is
    // statement-level, not per-row).
    let fixture = fixture("countdown-tags").await?;
    // Whole seconds: the milestone lock compares the payload's release_at to
    // the stored row's, and timestamptz rounds to µs — a now_utc()+days value
    // keeps ns through the JSON payload and never equals the stored row.
    let release_at =
        OffsetDateTime::from_unix_timestamp(fixture.now.unix_timestamp() + 14 * 86_400)?;
    let created = fixture
        .repository
        .upsert_release_plan(
            fixture.workspace_id,
            UpsertReleasePlan {
                release_id: None,
                source_key: "countdown-plan".into(),
                title: "countdown title".into(),
                release_at,
                listen_url: Some("https://listen.example/countdown".into()),
                tier: Some(ReleaseTier::Single),
                active: true,
                assets_ready: true,
                communication_enabled: true,
                press_enabled: true,
                expected_version: 0,
            },
            "admin_api_key",
            &idem("countdown-tags"),
            None,
        )
        .await?;

    sqlx::query(
        "INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
         VALUES ($1, 'communication_campaigns_enabled', true, 'test')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .execute(&fixture.pool)
    .await?;

    let mut fans = Vec::new();
    for idx in 0..3_i32 {
        let fan = sqlx::query_scalar::<_, uuid::Uuid>(
            "INSERT INTO fans (id, workspace_id, normalized_email, status)
             VALUES (gen_random_uuid(), $1, $2, 'active') RETURNING id",
        )
        .bind(fixture.workspace_id.into_uuid())
        .bind(format!("likely-{idx}@example.test"))
        .fetch_one(&fixture.pool)
        .await?;
        sqlx::query(
            "INSERT INTO fan_consents
                 (workspace_id, fan_id, purpose, granted, policy_version, source)
             VALUES ($1, $2, 'marketing', true, 'test-1', 'test')",
        )
        .bind(fixture.workspace_id.into_uuid())
        .bind(fan)
        .execute(&fixture.pool)
        .await?;
        fans.push(fan);
    }

    run_release_milestone(
        &fixture,
        created.release_id,
        "countdown title",
        release_at,
        "countdown",
    )
    .await?;

    let tag = format!("presave-{}", created.release_id.into_uuid());
    let tagged = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM fan_audience_tags
         WHERE workspace_id = $1 AND tag = $2 AND source = 'system'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(&tag)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        tagged,
        fans.len() as i64,
        "every ranked listener carries the countdown tag"
    );

    // And the named list still rides the outcome — the batch change wrote
    // tags, not a different artifact.
    let emitted = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM outbox_events
         WHERE workspace_id = $1
           AND event_type = 'crowdrelay.release.likely_listeners'
           AND jsonb_array_length(payload->'likely_listeners') = $2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fans.len() as i64)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(emitted, 1, "the named list rides the outcome exactly once");
    Ok(())
}
