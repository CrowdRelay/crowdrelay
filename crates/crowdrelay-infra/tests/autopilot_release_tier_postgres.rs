//! Release tier round-trip and milestone marks against a real Postgres.
//!
//! What fails here and nowhere else: an upsert that drops `tier` under
//! COALESCE semantics (absent must keep the stored value, not reset it), a
//! snapshot row that forgets to read the new column, and a milestone-marks
//! query that returns rows for the wrong workspace.

use std::time::Duration;

use crowdrelay_application::autopilot::{
    AutopilotActionRepository, AutopilotDecisionRepository, AutopilotTeamStateRepository,
    UpsertReleasePlan,
};
use crowdrelay_application::{IdempotencyKey, RepositoryError};
use crowdrelay_domain::{WorkspaceId, release_autopilot::ReleaseTier};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;

struct Fixture {
    repository: PostgresAutopilotRepository,
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
    now: OffsetDateTime,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let database_url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")
        .map_err(|error| format!("test database url must target a disposable database: {error}"))?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

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
            &idem("projection"),
            None,
        )
        .await?;

    let (occurred, expires, active): (OffsetDateTime, OffsetDateTime, bool) = sqlx::query_as(
        "SELECT occurred_at, expires_at, active
         FROM viryaos_content_sources
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
         FROM viryaos_content_sources
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
            &idem("projection-off"),
            None,
        )
        .await?;
    let dormant: bool = sqlx::query_scalar(
        "SELECT active FROM viryaos_content_sources
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
         FROM viryaos_content_sources
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
            &idem("flagflip"),
            None,
        )
        .await?;
    let (source_id, source_version): (uuid::Uuid, i64) = sqlx::query_as(
        "SELECT id, version FROM viryaos_content_sources
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
        r#"INSERT INTO viryaos_autopilot_decisions
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
        r#"INSERT INTO viryaos_autopilot_actions
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
        r#"INSERT INTO viryaos_autopilot_decisions
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
        r#"INSERT INTO viryaos_autopilot_actions
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
        "INSERT INTO viryaos_executor_instances (
            workspace_id, executor_id, version, manifest_sha, observed_at, expires_at
        ) VALUES ($1,'n8n-r3-test','test','test-manifest',$2,$3)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.now)
    .bind(fixture.now + time::Duration::minutes(30))
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_executor_capabilities (
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
        "SELECT EXISTS(SELECT 1 FROM viryaos_release_milestones
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
