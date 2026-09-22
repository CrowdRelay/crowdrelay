//! A promoter's reply proposing terms, against a real Postgres (P.7).
//!
//! The negotiation machinery was complete except its only input was a human
//! retyping a number from an email. This proves the sensing half: a booking
//! reply carrying its own words joins the triage queue under the target's
//! real kind, always lands needs_human — the operator already filed the
//! disposition — and carries the deterministic reader's proposed terms plus
//! the live opportunity it matches, ready for a human to confirm through
//! the ordinary terms route.

use std::time::Duration;

use crate::common;
use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{
    AutopilotBookingStateRepository, AutopilotReplyTriageRepository, RecordBookingReply,
    ReplyTargetKind, ReplyTriageResult,
};
use crowdrelay_domain::autonomy::Confidence;
use crowdrelay_domain::booking::BookingReplyDisposition;
use crowdrelay_domain::reply_triage::{HumanReviewReason, ReplyClassification};
use crowdrelay_domain::{BookingTargetId, TeamOpportunityId, WorkspaceId};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    target_id: Uuid,
    opportunity_id: TeamOpportunityId,
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
        .bind("Reply negotiation E2E")
        .execute(&pool)
        .await?;

    let city_id = Uuid::now_v7();
    sqlx::query("INSERT INTO cities (id, slug, name, country_code) VALUES ($1,$2,$3,'PL')")
        .bind(city_id)
        .bind(format!("poznan-{suffix}"))
        .bind("Poznan")
        .execute(&pool)
        .await?;

    let now = OffsetDateTime::now_utc();
    let target_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO booking_targets
            (id, workspace_id, city_id, target_kind, display_name, contact_email,
             relationship_score)
         VALUES ($1,$2,$3,'promoter','Anna Promoter','anna@promoter.example',70)",
    )
    .bind(target_id)
    .bind(workspace_id.into_uuid())
    .bind(city_id)
    .execute(&pool)
    .await?;

    // A live negotiation — submitted and replied — whose contact is the
    // promoter the reply came from. The proposal must land on this one.
    let opportunity_id = TeamOpportunityId::new();
    sqlx::query(
        r#"
        INSERT INTO team_opportunities (
            id, workspace_id, opportunity_kind, source, external_key, title, organization,
            contact_email, verified_destination, fit_basis_points, confidence_basis_points,
            currency, expected_fee_minor, estimated_cost_minor, status
        ) VALUES (
            $1,$2,'support_slot','manual',$3,'Reply E2E slot','Anna Promoter',
            'anna@promoter.example',true,9000,9000,'EUR',300000,150000,'replied'
        )
        "#,
    )
    .bind(opportunity_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("reply-{suffix}"))
    .execute(&pool)
    .await?;

    // Another tenant's open negotiation with the same contact — only the
    // workspace predicate keeps the proposal off it.
    let foreign_workspace = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(foreign_workspace.into_uuid())
        .bind(format!("{label}-foreign-{suffix}"))
        .bind("Foreign")
        .execute(&pool)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO team_opportunities (
            id, workspace_id, opportunity_kind, source, external_key, title, organization,
            contact_email, verified_destination, fit_basis_points, confidence_basis_points,
            currency, status
        ) VALUES (
            $1,$2,'support_slot','manual',$3,'Foreign slot','Anna Promoter',
            'anna@promoter.example',true,9000,9000,'EUR','replied'
        )
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(foreign_workspace.into_uuid())
    .bind(format!("reply-foreign-{suffix}"))
    .execute(&pool)
    .await?;

    let database = DatabaseConfig {
        url: database_url,
        max_connections: 4,
        connect_timeout: Duration::from_secs(3),
        ping_timeout: Duration::from_secs(2),
        operation_timeout: Duration::from_secs(10),
        lock_timeout: Duration::from_secs(1),
    };
    let repository = PostgresAutopilotRepository::new(pool.clone(), &database);
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        target_id,
        opportunity_id,
        now,
    })
}

async fn record_reply(
    fixture: &Fixture,
    text: &str,
    key: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    fixture
        .repository
        .record_booking_reply(
            fixture.workspace_id,
            RecordBookingReply {
                target_id: BookingTargetId::from_uuid(fixture.target_id),
                disposition: BookingReplyDisposition::Positive,
                occurred_at: fixture.now,
                reply_text: Some(text.to_owned()),
            },
            &IdempotencyKey::parse(key)?,
            None,
        )
        .await?;
    Ok(())
}

/// What the worker does with a booking-channel row: no classifier, always
/// needs_human — then the record path proposes the terms it read.
async fn classify_and_record(fixture: &Fixture) -> Result<(), Box<dyn std::error::Error>> {
    let replies = fixture
        .repository
        .load_replies_needing_triage(fixture.workspace_id, 10)
        .await?;
    for reply in replies {
        assert_eq!(
            reply.target_kind,
            ReplyTargetKind::BookingCounterparty,
            "a promoter reply never meets the outreach classifier"
        );
        fixture
            .repository
            .record_reply_classification(
                fixture.workspace_id,
                reply.reply_id,
                &ReplyTriageResult {
                    classification: ReplyClassification::NeedsHuman {
                        reason: HumanReviewReason::NegotiationReply,
                        confidence: Confidence::saturating_from_basis_points(10_000),
                    },
                    classified_at: OffsetDateTime::now_utc(),
                },
            )
            .await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_promoters_written_offer_lands_as_a_proposal_on_the_negotiation()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("reply-terms").await?;

    record_reply(
        &fixture,
        "We can offer €400 for the night.",
        "reply-offer-1",
    )
    .await?;

    // The reply's words joined the triage queue under the target's real kind.
    let (target_kind, previous_disposition): (String, String) = sqlx::query_as(
        "SELECT target_kind, previous_disposition FROM reply_classifications \
         WHERE workspace_id=$1 AND target_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.target_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(target_kind, "promoter");
    assert_eq!(previous_disposition, "received");

    classify_and_record(&fixture).await?;

    let (result, reason, fee, currency, opportunity): (
        String,
        String,
        Option<i64>,
        Option<String>,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT classification_result, human_review_reason, proposed_fee_minor, \
                proposed_currency, proposed_opportunity_id \
         FROM reply_classifications \
         WHERE workspace_id=$1 AND target_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.target_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(result, "needs_human");
    assert_eq!(reason, "negotiation_reply");
    assert_eq!(fee, Some(40_000));
    assert_eq!(currency.as_deref(), Some("EUR"));
    assert_eq!(
        opportunity,
        Some(fixture.opportunity_id.into_uuid()),
        "the proposal lands on the workspace's own open negotiation, not the \
         foreign tenant's row with the same contact"
    );

    // A reply with no figure files the same needs_human row and proposes
    // nothing — the human still reads, there is just nothing to confirm.
    record_reply(&fixture, "No fee this time, sorry.", "reply-offer-2").await?;
    classify_and_record(&fixture).await?;
    let (fee, currency, opportunity): (Option<i64>, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT proposed_fee_minor, proposed_currency, proposed_opportunity_id \
             FROM reply_classifications \
             WHERE workspace_id=$1 AND target_id=$2 AND reply_text LIKE 'No fee%'",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.target_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!((fee, currency.as_deref(), opportunity), (None, None, None));

    // And a reply with no text at all queues nothing.
    fixture
        .repository
        .record_booking_reply(
            fixture.workspace_id,
            RecordBookingReply {
                target_id: BookingTargetId::from_uuid(fixture.target_id),
                disposition: BookingReplyDisposition::Declined,
                occurred_at: fixture.now,
                reply_text: None,
            },
            &IdempotencyKey::parse("reply-offer-3")?,
            None,
        )
        .await?;
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM reply_classifications \
         WHERE workspace_id=$1 AND target_id=$2 AND classification_result='auto' \
           AND classified_disposition IS NULL",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.target_id)
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(queued, 0, "a disposition-only reply has nothing to read");

    fixture.pool.close().await;
    Ok(())
}
