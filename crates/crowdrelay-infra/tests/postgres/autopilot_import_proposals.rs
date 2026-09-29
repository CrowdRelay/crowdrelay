//! The CRM import gate against a real Postgres.
//!
//! The list route's verdicts and the approve route's transaction are the
//! same screen: what fails here and nowhere else is a verdict that reads
//! differently once it has to write, a replayed approval that inserts a
//! second target row, or an audit row that never landed.

use crate::common;
use crowdrelay_application::autopilot::{
    AutopilotOutreachImportRepository, OutreachImportSelection,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use std::time::Duration;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
}

async fn setup() -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("import-proposals-{suffix}"))
        .bind("Import Proposal Tests")
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
        pool,
        repository,
        workspace_id,
    })
}

async fn propose(
    f: &Fixture,
    kind: &str,
    name: &str,
    email: Option<&str>,
) -> Result<uuid::Uuid, sqlx::Error> {
    sqlx::query_scalar::<_, uuid::Uuid>(
        r#"INSERT INTO agent_outreach_targets
           (workspace_id, target_kind, display_name, contact_email, status)
           VALUES ($1,$2,$3,$4,'proposed')
           RETURNING id"#,
    )
    .bind(f.workspace_id.into_uuid())
    .bind(kind)
    .bind(name)
    .bind(email)
    .fetch_one(&f.pool)
    .await
}

/// Two clean proposals admit and a third — its address already a target —
/// is refused without touching the existing relationship. The audit row is
/// written, and a replayed idempotency key inserts nothing twice.
#[tokio::test]
#[ignore = "postgres"]
async fn approve_promotes_admitted_rows_once() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();

    // The already-held address: an existing target the import must never
    // overwrite.
    sqlx::query(
        "INSERT INTO outreach_targets
         (workspace_id, target_kind, display_name, contact_email,
          active, verified, accepts_outreach)
         VALUES ($1,'press','Known Zine','Taken@Zine.example',true,true,true)",
    )
    .bind(workspace)
    .execute(&f.pool)
    .await
    .expect("existing target");

    let clean_a = propose(&f, "press", "Zine A", Some("a@zine.example"))
        .await
        .expect("proposal a");
    let taken = propose(&f, "press", "Zine Taken", Some("taken@zine.example"))
        .await
        .expect("proposal taken");
    let clean_b = propose(&f, "radio", "Radio B", Some("b@radio.example"))
        .await
        .expect("proposal b");

    let key = crowdrelay_application::IdempotencyKey::parse("import-approve-0001")
        .expect("idempotency key");
    let approval = f
        .repository
        .approve_outreach_import_proposals(
            f.workspace_id,
            OutreachImportSelection::Ids(vec![clean_a, taken, clean_b]),
            &key,
            None,
        )
        .await
        .expect("approval");

    assert_eq!(approval.admitted, 2);
    assert_eq!(approval.created_target_ids.len(), 2);
    assert_eq!(
        approval.refused_by_reason.get("refuse:already_target"),
        Some(&1)
    );
    assert!(!approval.replayed);

    // The pre-existing target was not rewritten: verified stays true, and no
    // second row for the address appeared.
    let (target_rows, preexisting_verified): (i64, bool) = sqlx::query_as(
        "SELECT COUNT(*)::bigint,
                COALESCE(bool_or(verified), false)
         FROM outreach_targets
         WHERE workspace_id = $1 AND lower(contact_email) = 'taken@zine.example'",
    )
    .bind(workspace)
    .fetch_one(&f.pool)
    .await
    .expect("target count");
    assert_eq!(target_rows, 1);
    assert!(preexisting_verified);

    // Admitted rows promoted with the screen's verdict recorded.
    let promoted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM agent_outreach_targets
         WHERE workspace_id = $1 AND status = 'promoted'
           AND screening_verdict = 'admitted' AND screened_at IS NOT NULL",
    )
    .bind(workspace)
    .fetch_one(&f.pool)
    .await
    .expect("promoted count");
    assert_eq!(promoted, 2);
    let refused_row: Option<String> = sqlx::query_scalar(
        "SELECT status FROM agent_outreach_targets
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace)
    .bind(taken)
    .fetch_one(&f.pool)
    .await
    .expect("refused row");
    assert_eq!(refused_row.as_deref(), Some("proposed"));

    // The audit row landed under the idempotency key.
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM operator_actions
         WHERE workspace_id = $1 AND idempotency_key = 'import-approve-0001'
           AND action = 'approve_outreach_import_proposals'",
    )
    .bind(workspace)
    .fetch_one(&f.pool)
    .await
    .expect("audit count");
    assert_eq!(audit, 1);

    // A replay returns the stored answer and writes nothing new.
    let replay = f
        .repository
        .approve_outreach_import_proposals(
            f.workspace_id,
            OutreachImportSelection::Ids(vec![clean_a, taken, clean_b]),
            &key,
            None,
        )
        .await
        .expect("replay");
    assert!(replay.replayed);
    assert_eq!(replay.created_target_ids.len(), 0);

    let total_targets: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM outreach_targets WHERE workspace_id = $1")
            .bind(workspace)
            .fetch_one(&f.pool)
            .await
            .expect("total targets");
    assert_eq!(total_targets, 3);
}

/// The list answers each row's verdict and the batch's shape. An address
/// held as do-not-contact in the contact governor, a role mailbox and a
/// malformed address each refuse under their own reason.
#[tokio::test]
#[ignore = "postgres"]
async fn list_screens_every_proposal() {
    let f = setup().await.expect("fixture");
    let workspace = f.workspace_id.into_uuid();

    sqlx::query(
        "INSERT INTO contact_governor
         (workspace_id, normalized_contact, last_context, last_outbound_at,
          next_contact_after, do_not_contact)
         VALUES ($1,'blocked@zine.example','test',now(),now(),true)",
    )
    .bind(workspace)
    .execute(&f.pool)
    .await
    .expect("governor suppression");

    let admitted = propose(&f, "press", "Zine", Some("editor@zine.example"))
        .await
        .expect("admitted");
    propose(&f, "press", "Blocked", Some("blocked@zine.example"))
        .await
        .expect("suppressed");
    propose(&f, "press", "Role", Some("noreply@zine.example"))
        .await
        .expect("role");
    propose(&f, "press", "Broken", Some("not-an-address"))
        .await
        .expect("invalid");
    // A kind outside the importable set is not listed at all.
    propose(&f, "community", "Subreddit", Some("mod@sub.example"))
        .await
        .expect("community row");

    let page = f
        .repository
        .list_outreach_import_proposals(f.workspace_id, Some("press".to_string()), 100)
        .await
        .expect("page");

    assert_eq!(page.proposals.len(), 4);
    let verdict_of = |id: uuid::Uuid| {
        page.proposals
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.verdict.as_str())
    };
    assert_eq!(verdict_of(admitted), Some("admit"));
    assert_eq!(page.verdict_counts.get("admit"), Some(&1));
    assert_eq!(page.verdict_counts.get("refuse:do_not_contact"), Some(&1));
    assert_eq!(page.verdict_counts.get("refuse:role_address"), Some(&1));
    assert_eq!(page.verdict_counts.get("refuse:invalid_email"), Some(&1));
}

/// The all-admitted selection approves a whole kind without naming ids.
#[tokio::test]
#[ignore = "postgres"]
async fn approve_all_admitted_promotes_the_kind() {
    let f = setup().await.expect("fixture");

    propose(&f, "playlist", "Playlist A", Some("a@play.example"))
        .await
        .expect("a");
    propose(&f, "playlist", "Playlist B", Some("b@play.example"))
        .await
        .expect("b");
    propose(&f, "radio", "Radio C", Some("c@radio.example"))
        .await
        .expect("other kind");

    let key = crowdrelay_application::IdempotencyKey::parse("import-approve-0002")
        .expect("idempotency key");
    let approval = f
        .repository
        .approve_outreach_import_proposals(
            f.workspace_id,
            OutreachImportSelection::AllAdmitted {
                target_kind: "playlist".to_string(),
            },
            &key,
            None,
        )
        .await
        .expect("approval");

    assert_eq!(approval.admitted, 2);
    assert_eq!(approval.created_target_ids.len(), 2);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM agent_outreach_targets
         WHERE workspace_id = $1 AND status = 'proposed'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("remaining");
    assert_eq!(remaining, 1, "the radio row was never in the selection");
}
