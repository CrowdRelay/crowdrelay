//! The archive-promotion wave end to end against real Postgres: the growth
//! debt loader counts only the promotable cut, and an approved
//! `RunArchivePromoteWave` action executes the bounded evidence slice —
//! pending fans, confirmation intents and staged marks in one commit.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::{AutopilotActionRepository, AutopilotDecisionRepository};
use crowdrelay_domain::WorkspaceId;
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
        .bind("Archive wave E2E")
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

/// §4i-0e: a staged likely-fan backlog is the workspace's debt. Three
/// promotable rows raise one observation counting the promotable cut; a
/// fourth row that left `staged` — promoted already — must not pad it,
/// and a beacon-typed row belongs to the org review queue, not the wave.
#[tokio::test]
#[ignore = "needs a live postgres"]
async fn the_archive_backlog_counts_only_the_promotable_cut()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("archive-backlog").await?;
    for (email, kind, fan_outcome) in [
        ("ania@gmail.com", None, "staged"),
        ("bartek@wp.pl", None, "staged"),
        ("celina@outlook.com", None, "staged"),
        // Already promoted: the wave would not reach it, so the debt must
        // not count it either.
        ("danuta@gmail.com", None, "promoted"),
        // Sheet-typed beacon row: it is the org queue's problem, not the
        // fan wave's.
        ("promoter@club.pl", Some("promoter"), "staged"),
    ] {
        sqlx::query(
            r#"
            INSERT INTO drive_contacts
                (id, workspace_id, normalized_email, suggested_kind, city,
                 source_file_id, source_file_name, sources, fan_outcome)
            VALUES ($1, $2, $3, $4, $5, 'file-1', 'contacts.csv', '{gdrive}', $6)
            "#,
        )
        .bind(uuid::Uuid::now_v7())
        .bind(fixture.workspace_id.into_uuid())
        .bind(email)
        .bind(kind)
        .bind(Option::<String>::None)
        .bind(fan_outcome)
        .execute(&fixture.pool)
        .await?;
    }

    let debts = fixture
        .repository
        .load_growth_debt_observations(fixture.workspace_id, fixture.now)
        .await?;
    let backlog = debts
        .iter()
        .find(|debt| {
            debt.kind == crowdrelay_domain::growth_debt::GrowthDebtKind::ArchiveBacklogUnpromoted
        })
        .expect("three staged likely-fan rows are a backlog worth raising");
    assert_eq!(backlog.outstanding_items, 3);
    assert_eq!(backlog.tracked_items, 3);
    assert_eq!(
        backlog.subject,
        crowdrelay_domain::growth_debt::GrowthDebtSubject::Workspace(fixture.workspace_id)
    );
    Ok(())
}

/// §4i-0e end-to-end: an approved `RunArchivePromoteWave` action, claimed
/// and executed, promotes the evidence-ranked slice of the staged
/// likely-fan cut inside its own transaction — pending fans, confirmation
/// intents, staged rows marked — and touches nothing else.
#[tokio::test]
#[ignore = "needs a live postgres"]
async fn an_approved_wave_promotes_the_bounded_slice() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("archive-wave").await?;
    for (email, sources, inbound) in [
        // The inbound writer is the evidence head — promoted first.
        ("wrote-in@gmail.com", "{gdrive}", true),
        ("gmail-fan@gmail.com", "{gdrive}", false),
        ("dual@wp.pl", "{gdrive,github}", false),
    ] {
        sqlx::query(
            r#"
            INSERT INTO drive_contacts
                (id, workspace_id, normalized_email, suggested_kind, city,
                 source_file_id, source_file_name, sources, last_inbound_at)
            VALUES ($1, $2, $3, NULL, NULL, 'file-1', 'contacts.csv', $4::text[], $5)
            "#,
        )
        .bind(uuid::Uuid::now_v7())
        .bind(fixture.workspace_id.into_uuid())
        .bind(email)
        .bind(sources)
        .bind(if inbound {
            Some(fixture.now - time::Duration::days(3))
        } else {
            None
        })
        .execute(&fixture.pool)
        .await?;
    }

    // The queued action is the post-approval shape: a person saw the card
    // and said go. `available_at` defaults to the insert's now.
    let action_id: uuid::Uuid = sqlx::query_scalar(
        r#"
        WITH decision AS (
            INSERT INTO autopilot_decisions
                (id, workspace_id, decision_key, context, subject_kind, subject_id,
                 decision_kind, confidence_basis_points, disposition, reason,
                 input_snapshot, policy_snapshot, recommendation, evaluated_at, trace_id)
            VALUES ($1,$2,$3,'growth_debt','workspace',$2,
                    'growth_debt_archive_backlog',9000,'require_approval','archive backlog',
                    '{}','{}','{}',now(),$4) RETURNING id
        )
        INSERT INTO autopilot_actions
            (id, workspace_id, decision_id, context, action_kind, subject_kind,
             subject_id, idempotency_key, payload, status, action_class)
        SELECT $5, $2, id, 'growth_debt', 'archive.promotion.run', 'workspace',
               $2, $6, $7, 'queued', 'third_party'
        FROM decision
        RETURNING id
        "#,
    )
    .bind(uuid::Uuid::now_v7())
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("wave-decision-{}", uuid::Uuid::now_v7()))
    .bind(uuid::Uuid::now_v7())
    .bind(uuid::Uuid::now_v7())
    .bind(format!("wave-action-{}", uuid::Uuid::now_v7()))
    .bind(serde_json::json!({
        "kind": "run_archive_promote_wave",
        "limit": 2,
        "reason": "Zbieramy fanów w jednym miejscu — Virya Signal.",
        "staged_count": 3,
    }))
    .fetch_one(&fixture.pool)
    .await?;

    // `fixture.now` predates the action's `available_at` default — claim at
    // wall time.
    let run_at = OffsetDateTime::now_utc();
    let claimed = fixture
        .repository
        .claim_due_autonomous_actions(fixture.workspace_id, 4, run_at)
        .await?;
    let action = claimed
        .iter()
        .find(|claimed| claimed.id.into_uuid() == action_id)
        .expect("the queued wave is claimable");
    fixture
        .repository
        .execute_action(fixture.workspace_id, action, run_at)
        .await?;

    // The cap took the evidence slice: the inbound writer plus the dual-source
    // sighting; the bare gmail row stays staged for the next wave.
    let promoted: Vec<String> = sqlx::query_scalar(
        "SELECT normalized_email FROM drive_contacts
         WHERE workspace_id = $1 AND fan_outcome = 'promoted' ORDER BY normalized_email",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_all(&fixture.pool)
    .await?;
    assert_eq!(
        promoted,
        vec!["dual@wp.pl".to_owned(), "wrote-in@gmail.com".to_owned()]
    );

    let staged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM drive_contacts WHERE workspace_id = $1 AND fan_outcome = 'staged'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(staged, 1);

    // Two pending fans, two confirmation intents in the same commit.
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fans WHERE workspace_id = $1 AND status = 'pending'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(pending, 2);
    let invites: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(invites, 2);
    Ok(())
}
