//! Negotiating terms against a real Postgres.
//!
//! The domain is unit-tested and the arithmetic is not what fails here. What
//! fails here is the wiring: whether the ladder computed at open survives an
//! improved offer, whether a settled conversation stays settled, and whether
//! the floor still holds at execution — hours after the decision, when an
//! operator may have recorded something new.
//!
//! The last one is the only test in the file that would let the band play for
//! nothing if it were deleted.

use std::time::Duration;

use crowdrelay_application::autopilot::{
    AutopilotActionPayload, AutopilotActionRepository, AutopilotDecisionRepository,
    AutopilotTeamStateRepository, ClaimedAutopilotAction, PromoterPosition,
    RecordTeamOpportunityTerms,
};
use crowdrelay_application::{IdempotencyKey, RepositoryError};
use crowdrelay_domain::{
    AutopilotActionId, EventId, TeamOpportunityId, WorkspaceId,
    negotiation::{FloorBasis, TermsState},
};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::postgres::PgPoolOptions;
use time::OffsetDateTime;
use uuid::Uuid;

struct Fixture {
    pool: sqlx::PgPool,
    repository: PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    opportunity_id: TeamOpportunityId,
    now: OffsetDateTime,
}

async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let database_url =
        std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").map_err(|error| {
            format!(
                "CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL must target a disposable database: {error}"
            )
        })?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;

    let workspace_id = WorkspaceId::new();
    let suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("{label}-{suffix}"))
        .bind("Terms E2E")
        .execute(&pool)
        .await?;

    let now = OffsetDateTime::now_utc();
    // A show sixty days out, already applied for and replied to: the state a
    // negotiation actually happens in.
    let opportunity_id = TeamOpportunityId::new();
    sqlx::query(
        r#"
        INSERT INTO viryaos_team_opportunities (
            id, workspace_id, opportunity_kind, source, external_key, title, organization,
            verified_destination, fit_basis_points, confidence_basis_points, currency,
            expected_fee_minor, estimated_cost_minor, event_starts_at, status
        ) VALUES (
            $1,$2,'support_slot','manual',$3,'Terms E2E slot','A promoter',
            true,9000,9000,'PLN',300000,150000,$4,'replied'
        )
        "#,
    )
    .bind(opportunity_id.into_uuid())
    .bind(workspace_id.into_uuid())
    .bind(format!("terms-{suffix}"))
    .bind(now + time::Duration::days(60))
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
        opportunity_id,
        now,
    })
}

async fn record(
    fixture: &Fixture,
    position: PromoterPosition,
    key: &str,
) -> Result<(), RepositoryError> {
    fixture
        .repository
        .record_team_opportunity_terms(
            fixture.workspace_id,
            RecordTeamOpportunityTerms {
                opportunity_id: fixture.opportunity_id,
                position,
                currency: "PLN".to_owned(),
                responds_by: fixture.now + time::Duration::days(7),
            },
            &IdempotencyKey::parse(key).expect("valid key"),
            None,
        )
        .await
        .map(|_| ())
}

async fn ladder(fixture: &Fixture) -> Result<(i64, i64, i64, String), Box<dyn std::error::Error>> {
    Ok(sqlx::query_as::<_, (i64, i64, i64, String)>(
        "SELECT walk_away_minor, target_minor, opening_ask_minor, state
         FROM viryaos_team_opportunity_terms WHERE workspace_id=$1 AND opportunity_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.opportunity_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_ladder_is_computed_once_and_survives_a_better_offer()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = fixture("terms-ladder").await?;
    record(
        &fixture,
        PromoterPosition::Offer { fee_minor: 100_000 },
        "terms-open",
    )
    .await?;
    let opened = ladder(&fixture).await?;
    assert_eq!(opened.3, "proposed");
    assert!(opened.0 > 0, "a costed trip has a floor above zero");
    assert!(
        opened.1 >= opened.0 && opened.2 >= opened.1,
        "the ladder climbs"
    );

    // The promoter improves their offer. The state goes back to `proposed` so
    // the agent looks again, and the numbers the last counter was argued from
    // do not move under it.
    sqlx::query(
        "UPDATE viryaos_team_opportunity_terms SET state='countered', countered_fee_minor=$3, \
         counter_rounds=1 WHERE workspace_id=$1 AND opportunity_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.opportunity_id.into_uuid())
    .bind(opened.2)
    .execute(&fixture.pool)
    .await?;
    record(
        &fixture,
        PromoterPosition::Offer { fee_minor: 200_000 },
        "terms-improved",
    )
    .await?;
    let improved = ladder(&fixture).await?;
    assert_eq!(
        (improved.0, improved.1, improved.2),
        (opened.0, opened.1, opened.2),
        "the ladder is frozen at open; a better offer is not a new conversation"
    );
    assert_eq!(improved.3, "proposed");
    let rounds = sqlx::query_scalar::<_, i32>(
        "SELECT counter_rounds FROM viryaos_team_opportunity_terms \
         WHERE workspace_id=$1 AND opportunity_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.opportunity_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        rounds, 1,
        "nudging an offer up by a złoty must not buy another ask"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_withdrawal_settles_it_and_nothing_reopens_it() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = fixture("terms-withdrawn").await?;
    record(
        &fixture,
        PromoterPosition::Offer { fee_minor: 100_000 },
        "terms-open",
    )
    .await?;
    record(&fixture, PromoterPosition::Withdrawn, "terms-withdrawn").await?;
    let settled = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT state, settled_reason FROM viryaos_team_opportunity_terms \
         WHERE workspace_id=$1 AND opportunity_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.opportunity_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        settled,
        ("declined".to_owned(), Some("promoter_withdrew".to_owned()))
    );

    // Another offer does not quietly restart it. Reopening is an operator
    // deliberately starting a new conversation, and it is theirs to say so.
    assert!(matches!(
        record(
            &fixture,
            PromoterPosition::Offer { fee_minor: 900_000 },
            "terms-reopen",
        )
        .await,
        Err(RepositoryError::Conflict)
    ));

    // And a settled negotiation is never read into a cycle again.
    assert!(
        fixture
            .repository
            .load_live_opportunity_terms(fixture.workspace_id, fixture.now)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_floor_still_holds_when_the_move_is_finally_sent()
-> Result<(), Box<dyn std::error::Error>> {
    // The one test here whose absence would let the band play for nothing.
    // Hours pass between drafting an acceptance and a human approving it.
    let fixture = fixture("terms-floor").await?;
    record(
        &fixture,
        PromoterPosition::Offer { fee_minor: 400_000 },
        "terms-open",
    )
    .await?;
    let opened = ladder(&fixture).await?;

    let below_floor = opened.0.saturating_sub(1);
    let payload = AutopilotActionPayload::AcceptLiveOpportunityTerms {
        opportunity_id: fixture.opportunity_id,
        fee_minor: below_floor,
        currency: "PLN".to_owned(),
    };
    let action_id = queue_action(&fixture, &payload, "below-floor").await?;
    assert!(
        matches!(
            fixture
                .repository
                .execute_action(
                    fixture.workspace_id,
                    &ClaimedAutopilotAction {
                        id: AutopilotActionId::from_uuid(action_id),
                        payload,
                        attempt_number: 1,
                    },
                    fixture.now,
                )
                .await,
            Err(RepositoryError::Conflict)
        ),
        "an acceptance below the floor is refused at execution, not only at decision"
    );
    assert_eq!(ladder(&fixture).await?.3, "proposed", "and nothing settled");
    retire(&fixture, action_id).await?;

    // A counter in the wrong currency is a different offer, not a rounding
    // difference.
    let wrong_currency = AutopilotActionPayload::CounterLiveOpportunityTerms {
        opportunity_id: fixture.opportunity_id,
        ask_minor: opened.2,
        currency: "EUR".to_owned(),
        round: 1,
    };
    let action_id = queue_action(&fixture, &wrong_currency, "wrong-currency").await?;
    assert!(matches!(
        fixture
            .repository
            .execute_action(
                fixture.workspace_id,
                &ClaimedAutopilotAction {
                    id: AutopilotActionId::from_uuid(action_id),
                    payload: wrong_currency,
                    attempt_number: 1,
                },
                fixture.now,
            )
            .await,
        Err(RepositoryError::Conflict)
    ));
    retire(&fixture, action_id).await?;

    // The counter the agent actually drafted goes through, leaves the
    // negotiation waiting rather than finished, and counts exactly one ask.
    let counter = AutopilotActionPayload::CounterLiveOpportunityTerms {
        opportunity_id: fixture.opportunity_id,
        ask_minor: opened.2,
        currency: "PLN".to_owned(),
        round: 1,
    };
    let action_id = queue_action(&fixture, &counter, "counter").await?;
    fixture
        .repository
        .execute_action(
            fixture.workspace_id,
            &ClaimedAutopilotAction {
                id: AutopilotActionId::from_uuid(action_id),
                payload: counter,
                attempt_number: 1,
            },
            fixture.now,
        )
        .await?;
    let after = sqlx::query_as::<_, (String, Option<i64>, i32)>(
        "SELECT state, countered_fee_minor, counter_rounds FROM viryaos_team_opportunity_terms \
         WHERE workspace_id=$1 AND opportunity_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.opportunity_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(after, ("countered".to_owned(), Some(opened.2), 1));
    assert_eq!(
        TermsState::parse(&after.0),
        Some(TermsState::Countered),
        "countered is the agent waiting, not the agent finished"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM outbox_events
             WHERE workspace_id=$1 AND event_type='crowdrelay.opportunity.terms_countered'"
        )
        .bind(fixture.workspace_id.into_uuid())
        .fetch_one(&fixture.pool)
        .await?,
        1
    );
    Ok(())
}

/// Takes an action out of flight so the next one on the same subject may be
/// queued. Only the in-flight uniqueness index cares, and it is right to: one
/// live move per opportunity at a time is exactly the rule.
async fn retire(fixture: &Fixture, action_id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "UPDATE viryaos_autopilot_actions SET status='failed', finished_at=now() WHERE id=$1",
    )
    .bind(action_id)
    .execute(&fixture.pool)
    .await?;
    Ok(())
}

async fn queue_action(
    fixture: &Fixture,
    payload: &AutopilotActionPayload,
    label: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let decision_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_decisions (
            id, workspace_id, decision_key, context, subject_kind, subject_id,
            decision_kind, confidence_basis_points, disposition, reason,
            input_snapshot, policy_snapshot, recommendation
        , trace_id)
        VALUES ($1,$2,$3,'live_opportunity','team_opportunity',$4,'counter_live_opportunity_terms',
                9000,'require_approval','test','{}'::jsonb,'{}'::jsonb,$5,gen_random_uuid())
        "#,
    )
    .bind(decision_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("decision:live-terms:{label}:{decision_id}"))
    .bind(fixture.opportunity_id.into_uuid())
    .bind(serde_json::to_value(payload)?)
    .execute(&fixture.pool)
    .await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO viryaos_autopilot_actions (
            id, workspace_id, decision_id, context, action_kind, subject_kind, subject_id,
            idempotency_key, payload, status, action_class, attempt_count, started_at
        )
        VALUES ($1,$2,$3,'live_opportunity','opportunity.terms.counter','team_opportunity',$4,$5,
                $6,'processing','third_party',1,now())
        "#,
    )
    .bind(action_id)
    .bind(fixture.workspace_id.into_uuid())
    .bind(decision_id)
    .bind(fixture.opportunity_id.into_uuid())
    .bind(format!("action:live-terms:{label}:{action_id}"))
    .bind(serde_json::to_value(payload)?)
    .execute(&fixture.pool)
    .await?;
    Ok(action_id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_negotiation_reads_the_show_through_the_same_statement_the_evaluator_uses()
-> Result<(), Box<dyn std::error::Error>> {
    // Two ways of costing one trip is how a negotiation floor and an economics
    // verdict come to disagree about the same show.
    let fixture = fixture("terms-read").await?;
    record(
        &fixture,
        PromoterPosition::Offer { fee_minor: 100_000 },
        "terms-open",
    )
    .await?;
    let live = fixture
        .repository
        .load_live_opportunity_terms(fixture.workspace_id, fixture.now)
        .await?;
    let snapshot = live.first().ok_or("the negotiation is live")?;
    assert_eq!(snapshot.terms.opportunity_id, fixture.opportunity_id);
    assert_eq!(snapshot.currency, "PLN");
    assert_eq!(snapshot.opportunity.expected_fee_minor, 300_000);
    assert!(
        snapshot.opportunity.already_applied,
        "a negotiation only happens after something was sent"
    );
    // The apply read must not see it: those are two halves of the pipeline and
    // a batch of replied conversations may not crowd out actionable new offers.
    let apply = fixture
        .repository
        .load_live_opportunity_snapshots(fixture.workspace_id, fixture.now)
        .await?;
    assert!(
        apply
            .iter()
            .all(|entry| entry.opportunity_id != fixture.opportunity_id)
    );
    let _ = EventId::new();
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_floor_cites_the_counterpartys_precedent_and_the_market()
-> Result<(), Box<dyn std::error::Error>> {
    // §4h-9: the floor is a `max` over the costed trip, the counterparty's own
    // precedent and the market evidence at the rooms they book — and the row
    // says which of them bound, so the drafted counter can cite it.
    let fixture = fixture("terms-floor-basis").await?;
    sqlx::query(
        "UPDATE viryaos_team_opportunities SET contact_email='booker@promoter.example', \
         organization='Promoter Co' WHERE workspace_id=$1 AND id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.opportunity_id.into_uuid())
    .execute(&fixture.pool)
    .await?;

    // The precedent: an earlier conversation with the same contact email
    // closed on the counter we sent — COALESCE prefers it over the offer as
    // made, because the counter is what the deal actually closed at.
    let prior_opportunity = TeamOpportunityId::new();
    sqlx::query(
        r#"
        INSERT INTO viryaos_team_opportunities (
            id, workspace_id, opportunity_kind, source, external_key, title, organization,
            contact_email, verified_destination, fit_basis_points, confidence_basis_points,
            currency, expected_fee_minor, estimated_cost_minor, event_starts_at, status
        ) VALUES (
            $1,$2,'support_slot','manual',$3,'Last years slot','Promoter Co',
            'booker@promoter.example',true,9000,9000,'PLN',300000,120000,$4,'won'
        )
        "#,
    )
    .bind(prior_opportunity.into_uuid())
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("prior-{}", Uuid::now_v7().simple()))
    .bind(fixture.now - time::Duration::days(200))
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO viryaos_team_opportunity_terms (
            workspace_id, opportunity_id, state, currency, offered_fee_minor,
            walk_away_minor, target_minor, opening_ask_minor, countered_fee_minor,
            responds_by, settled_at
        ) VALUES ($1,$2,'accepted','PLN',280000,250000,290000,320000,300000,$3,$3)
        "#,
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(prior_opportunity.into_uuid())
    .bind(fixture.now - time::Duration::days(100))
    .execute(&fixture.pool)
    .await?;

    // The costed floor on this show is 150_000 (bare cost — no policy row is
    // configured). The deal closed at the 280_000 offer that was accepted;
    // the 300_000 on the row was our own unanswered counter, which was never
    // a deal and must not become the precedent.
    record(
        &fixture,
        PromoterPosition::Offer { fee_minor: 100_000 },
        "floor-prior",
    )
    .await?;
    let row = sqlx::query_as::<_, (i64, String, Option<i64>, Option<i64>)>(
        "SELECT walk_away_minor, floor_basis, prior_fee_minor, market_floor_minor \
         FROM viryaos_team_opportunity_terms WHERE workspace_id=$1 AND opportunity_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.opportunity_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        row,
        (
            280_000,
            "counterparty_history".to_owned(),
            Some(280_000),
            None
        ),
        "a promoter who agreed to 280 last time does not re-open below it"
    );

    // Stage two: the workspace's own booking graph ties the counterparty to a
    // room — a promoter target on the same contact email, linked through the
    // edge table — and three workspaces' pooled terms at that room clear a
    // band whose p25 sits above the precedent.
    let city_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO cities (id, slug, name, country_code) \
         VALUES ($1, 'wroclaw', 'Wrocław', 'PL') \
         ON CONFLICT (country_code, slug) DO UPDATE SET name = EXCLUDED.name",
    )
    .bind(city_id)
    .execute(&fixture.pool)
    .await?;
    let city_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM cities WHERE country_code = 'PL' AND slug = 'wroclaw'",
    )
    .fetch_one(&fixture.pool)
    .await?;
    let venue_id = Uuid::now_v7();
    // The test database is shared and venues are unique per (city, name), so
    // the room gets its own name per run rather than colliding on 'Klub Ucho'.
    let venue_name = format!("Klub Ucho {}", Uuid::now_v7().simple());
    sqlx::query(
        "INSERT INTO place_venues (id, city_id, name_key, display_name) \
         VALUES ($1, $2, place_venue_key($3), $3)",
    )
    .bind(venue_id)
    .bind(city_id)
    .bind(&venue_name)
    .execute(&fixture.pool)
    .await?;
    let target_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO viryaos_booking_targets \
         (id, workspace_id, city_id, target_kind, display_name, contact_email) \
         VALUES ($3,$1,$2,'promoter','Promoter Co','booker@promoter.example')",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(city_id)
    .bind(target_id)
    .execute(&fixture.pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_booking_target_venues (workspace_id, target_id, venue_id) \
         VALUES ($1,$2,$3)",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(target_id)
    .bind(venue_id)
    .execute(&fixture.pool)
    .await?;
    let night_id = Uuid::now_v7();
    sqlx::query("INSERT INTO place_events (id, venue_id, event_date) VALUES ($1,$2,$3)")
        .bind(night_id)
        .bind(venue_id)
        .bind(fixture.now.date() - time::Duration::days(30))
        .execute(&fixture.pool)
        .await?;
    // Three distinct workspaces — the k-anonymity floor. p25 over
    // [380_000, 400_000, 420_000] is 380_000.
    for (index, amount) in [380_000_i64, 400_000, 420_000].into_iter().enumerate() {
        let contributor = Uuid::now_v7();
        sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
            .bind(contributor)
            .bind(format!(
                "terms-contributor-{index}-{}",
                contributor.simple()
            ))
            .bind("Contributor")
            .execute(&fixture.pool)
            .await?;
        sqlx::query(
            "INSERT INTO place_event_contributions \
             (place_event_id, workspace_id, kind, value) VALUES ($1,$2,'terms',$3)",
        )
        .bind(night_id)
        .bind(contributor)
        .bind(serde_json::json!({"amount_minor": amount, "currency": "PLN"}))
        .execute(&fixture.pool)
        .await?;
    }

    // A fresh opportunity from the same counterparty: the market floor is
    // true for the room they work, so it — not the precedent, not the cost —
    // is what the ladder opens on, and the row says so.
    let next_opportunity = TeamOpportunityId::new();
    sqlx::query(
        r#"
        INSERT INTO viryaos_team_opportunities (
            id, workspace_id, opportunity_kind, source, external_key, title, organization,
            contact_email, verified_destination, fit_basis_points, confidence_basis_points,
            currency, expected_fee_minor, estimated_cost_minor, event_starts_at, status
        ) VALUES (
            $1,$2,'support_slot','manual',$3,'This years slot','Promoter Co',
            'booker@promoter.example',true,9000,9000,'PLN',300000,150000,$4,'replied'
        )
        "#,
    )
    .bind(next_opportunity.into_uuid())
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!("next-{}", Uuid::now_v7().simple()))
    .bind(fixture.now + time::Duration::days(60))
    .execute(&fixture.pool)
    .await?;
    fixture
        .repository
        .record_team_opportunity_terms(
            fixture.workspace_id,
            RecordTeamOpportunityTerms {
                opportunity_id: next_opportunity,
                position: PromoterPosition::Offer { fee_minor: 100_000 },
                currency: "PLN".to_owned(),
                responds_by: fixture.now + time::Duration::days(7),
            },
            &IdempotencyKey::parse("floor-market").expect("valid key"),
            None,
        )
        .await?;
    let row = sqlx::query_as::<_, (i64, String, Option<i64>, Option<i64>)>(
        "SELECT walk_away_minor, floor_basis, prior_fee_minor, market_floor_minor \
         FROM viryaos_team_opportunity_terms WHERE workspace_id=$1 AND opportunity_id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(next_opportunity.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(
        row,
        (380_000, "market".to_owned(), Some(280_000), Some(380_000)),
        "the lowest band that holds at every room they work binds the floor"
    );

    // And the read path hands the cycle the same citation the row froze.
    let live = fixture
        .repository
        .load_live_opportunity_terms(fixture.workspace_id, fixture.now)
        .await?;
    let snapshot = live
        .iter()
        .find(|entry| entry.terms.opportunity_id == next_opportunity)
        .ok_or("the negotiation is live")?;
    assert_eq!(snapshot.terms.ladder.floor_basis, FloorBasis::Market);
    assert_eq!(snapshot.terms.ladder.walk_away_minor, 380_000);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn terminal_progress_writes_the_reason_and_refuses_to_close_silently()
-> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_application::autopilot::{
        RecordTeamOpportunityProgress, TeamOpportunityProgress,
    };

    // The scout's status_reason write rides a bind the stricter transition
    // guards took over — this test exists because a bind nobody referenced
    // compiled clean and would have failed on the first real close.
    let fixture = fixture("terms-progress").await?;

    // Lost without a reason refuses: a refusal teaches the pipeline only if it
    // says why.
    let silent = fixture
        .repository
        .record_team_opportunity_progress(
            fixture.workspace_id,
            RecordTeamOpportunityProgress {
                opportunity_id: fixture.opportunity_id,
                progress: TeamOpportunityProgress::Lost,
                occurred_at: fixture.now,
                reason: None,
            },
            &IdempotencyKey::parse("progress-lost-silent").expect("valid key"),
            None,
        )
        .await;
    assert!(silent.is_err(), "a lost with no reason must not write");

    // Won from 'replied' carries the reason into status_reason.
    fixture
        .repository
        .record_team_opportunity_progress(
            fixture.workspace_id,
            RecordTeamOpportunityProgress {
                opportunity_id: fixture.opportunity_id,
                progress: TeamOpportunityProgress::Won,
                occurred_at: fixture.now,
                reason: Some("signed for the October date".to_owned()),
            },
            &IdempotencyKey::parse("progress-won").expect("valid key"),
            None,
        )
        .await?;
    let (status, reason): (String, Option<String>) = sqlx::query_as(
        "SELECT status, status_reason FROM viryaos_team_opportunities \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fixture.opportunity_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(status, "won");
    assert_eq!(reason.as_deref(), Some("signed for the October date"));

    // A won row cannot be re-closed or dismissed: the terminal guard holds.
    let reclose = fixture
        .repository
        .record_team_opportunity_progress(
            fixture.workspace_id,
            RecordTeamOpportunityProgress {
                opportunity_id: fixture.opportunity_id,
                progress: TeamOpportunityProgress::Lost,
                occurred_at: fixture.now,
                reason: Some("changed their mind".to_owned()),
            },
            &IdempotencyKey::parse("progress-reclose").expect("valid key"),
            None,
        )
        .await;
    assert!(matches!(reclose, Err(RepositoryError::Conflict)));

    // A never-sent row dismisses with a reason — pre-send cleanup is allowed.
    let fresh_id = TeamOpportunityId::new();
    sqlx::query(
        r#"
        INSERT INTO viryaos_team_opportunities (
            id, workspace_id, opportunity_kind, source, external_key, title, organization,
            verified_destination, fit_basis_points, confidence_basis_points, currency,
            expected_fee_minor, estimated_cost_minor, event_starts_at, status
        ) VALUES (
            $1,$2,'festival','manual',$3,'Dismissable','A festival',
            true,5000,5000,'PLN',100000,80000,$4,'new'
        )
        "#,
    )
    .bind(fresh_id.into_uuid())
    .bind(fixture.workspace_id.into_uuid())
    .bind(format!(
        "dismiss-{}",
        fixture.workspace_id.into_uuid().simple()
    ))
    .bind(fixture.now + time::Duration::days(90))
    .execute(&fixture.pool)
    .await?;
    fixture
        .repository
        .record_team_opportunity_progress(
            fixture.workspace_id,
            RecordTeamOpportunityProgress {
                opportunity_id: fresh_id,
                progress: TeamOpportunityProgress::Dismissed,
                occurred_at: fixture.now,
                reason: Some("scout duplicate".to_owned()),
            },
            &IdempotencyKey::parse("progress-dismiss").expect("valid key"),
            None,
        )
        .await?;
    let (status, reason): (String, Option<String>) = sqlx::query_as(
        "SELECT status, status_reason FROM viryaos_team_opportunities \
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(fixture.workspace_id.into_uuid())
    .bind(fresh_id.into_uuid())
    .fetch_one(&fixture.pool)
    .await?;
    assert_eq!(status, "dismissed");
    assert_eq!(reason.as_deref(), Some("scout duplicate"));
    Ok(())
}
