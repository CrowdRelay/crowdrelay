//! 4G.4 — approving a gig proposal writes one letter to everybody who books
//! the room, or writes none of it.
//!
//! The property worth a database test is the one a unit test cannot reach: the
//! contact governor runs inside the dispatch transaction, so a promoter who is
//! blocked has to roll back the whole outreach rather than leaving the band
//! having written to two of three people about one night. Those three book the
//! same city and talk to each other, and a partial send is worse than none.
//!
//! Also driven here because SQLx checks nothing at compile time: the approval
//! writes a decision and an action by hand, and a column that does not exist
//! would be a production failure with no warning anywhere earlier.

use std::time::Duration;

use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::{
    AutopilotActionRepository, AutopilotMeasurementKind, AutopilotMeasurementRepository,
    ClaimedAutopilotMeasurement, assess_measurement_effect,
};
use crowdrelay_domain::ids::{AutopilotActionId, AutopilotMeasurementId};
use crowdrelay_domain::{CityId, WorkspaceId};
use crowdrelay_infra::gig_outreach::{GigOutreachError, GigOutreachOutcome, approve_gig_proposal};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use sqlx::{Connection, PgConnection, PgPool, postgres::PgPoolOptions};
use time::OffsetDateTime;
use uuid::Uuid;

struct DisposableDatabase {
    pool: PgPool,
    url: String,
    admin_url: String,
    name: String,
}

impl DisposableDatabase {
    async fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let base = std::env::var("CROWDRELAY_TEST_DATABASE_URL")
            .map_err(|_| "CROWDRELAY_TEST_DATABASE_URL must target a disposable database")?;
        let name = format!("crowdrelay_gigout_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').ok_or("database url has no path")?;
        let url = format!("{head}/{name}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&url)
            .await?;
        crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            url,
            admin_url: base,
            name,
        })
    }

    async fn drop_database(self) {
        let Self {
            pool,
            admin_url,
            name,
            ..
        } = self;
        pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&admin_url).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&mut admin)
                .await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn one_approval_writes_to_the_whole_room_or_to_nobody()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run(&database.pool, &database.url).await;
    database.drop_database().await;
    result
}

async fn run(pool: &PgPool, url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool).await?;
    let wroclaw = city(pool, "wroclaw").await?;
    for index in 0..60 {
        reachable_fan(pool, act, wroclaw, &format!("fan{index}@example.com")).await?;
    }
    played_show(pool, act, wroclaw, "Klub X", "show-1", 40).await?;
    // Two promoters who book the same room. The whole point of the recipient
    // set: they talk to each other.
    promoter(pool, act, wroclaw, "Anna", "anna@example.com", 70).await?;
    promoter(pool, act, wroclaw, "Bogdan", "bogdan@example.com", 60).await?;

    // ── The approval ────────────────────────────────────────────────────────
    let key = IdempotencyKey::parse("gig-approve-1").expect("valid key");
    let outcome = approve_gig_proposal(pool, act, wroclaw, &key, now, None).await?;
    let (action_id, recipients, opening_line) = match outcome {
        GigOutreachOutcome::Queued {
            action_id,
            recipients,
            opening_line,
            ..
        } => (action_id, recipients, opening_line),
        other => return Err(format!("expected a queued outreach, got {other:?}").into()),
    };
    assert_eq!(
        recipients.len(),
        2,
        "both promoters who book the room must be on one letter, got {recipients:?}"
    );
    assert!(
        !opening_line.is_empty() && opening_line.ends_with('.'),
        "the letter opens with the proposal's reason: {opening_line:?}"
    );

    // One action, not one per promoter. This is the §12-6 rule, and it is the
    // difference between everybody hearing and two of three hearing.
    let (kind, status, payload) = sqlx::query_as::<_, (String, String, serde_json::Value)>(
        "SELECT action_kind, status, payload FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(kind, "gig.outreach.request");
    // Approving the proposal is the approval. A second approve screen is how a
    // person stops reading approvals.
    assert_eq!(status, "queued");
    assert_eq!(
        payload["recipients"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or_default(),
        2
    );
    // O.1: the letter the promoter will read is in the payload, whole. Before
    // this the executor composed it after the approval, so the band approved an
    // opening line and a stranger received five paragraphs nobody had seen.
    let subject = payload["draft"]["subject"].as_str().unwrap_or_default();
    let body = payload["draft"]["body"].as_str().unwrap_or_default();
    assert!(
        subject.contains("Klub X"),
        "the approved subject does not name the room: {subject:?}"
    );
    assert!(
        body.contains(&opening_line),
        "the letter does not open with the line the band approved"
    );
    // O.6: the fixture's room is in Poland, so the whole letter is Polish —
    // frame, evidence and sign-off together. An English greeting here would
    // mean the frame and the sentences had come apart.
    assert!(
        body.starts_with("Cześć,") && body.contains("Pozdrawiamy,"),
        "the payload carries a fragment rather than a whole letter: {body:?}"
    );
    assert!(
        !body.contains("Hi,") && !body.contains("Best,"),
        "an English frame leaked into a letter to a Polish room: {body:?}"
    );
    assert!(
        !body.contains("Virya, a modern metal band from Wroclaw"),
        "the letter still carries the executor's hardcoded description of one tenant"
    );

    assert!(
        !payload["reasons"]
            .as_array()
            .expect("reasons travel with the action")
            .is_empty(),
        "the reasons the band approved on did not reach the letter"
    );
    let total_actions = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions WHERE workspace_id = $1",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(total_actions, 1, "one room, one action");

    // Every action carries a decision, or the trace cannot explain the send.
    let decision_kind = sqlx::query_scalar::<_, String>(
        "SELECT decision.decision_kind FROM viryaos_autopilot_actions AS action
         JOIN viryaos_autopilot_decisions AS decision ON decision.id = action.decision_id
         WHERE action.workspace_id = $1 AND action.id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(decision_kind, "gig.proposal.approved");

    // ── The two minutes between "approve" and "sent" (O.2) ──────────────────
    // Until this existed, the two were one instant: the action was queued with
    // `available_at = now`, a worker claimed it within a second, and the only
    // cancel path refused anything past `awaiting_approval`. An operator who
    // saw the mistake immediately had nothing to click.
    let available_at = sqlx::query_scalar::<_, OffsetDateTime>(
        "SELECT available_at FROM viryaos_autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert!(
        available_at > now,
        "the letter was claimable the instant it was approved: {available_at} <= {now}"
    );

    // ── The same click twice ────────────────────────────────────────────────
    let replay = approve_gig_proposal(pool, act, wroclaw, &key, now, None).await?;
    match replay {
        GigOutreachOutcome::Replayed {
            action_id: replayed,
            ..
        } => assert_eq!(replayed, action_id, "a retried click made a second letter"),
        other => return Err(format!("expected a replay, got {other:?}").into()),
    }

    // ── One blocked promoter stops the whole letter ─────────────────────────
    // Bogdan is under a do-not-contact. The governor refuses him, and the
    // transaction takes Anna's reservation and the outbox row down with it.
    block_contact(pool, act, "bogdan@example.com", now).await?;
    // A registry that advertises something else. Gating only applies once an
    // executor exists at all — with an empty registry nothing is gated, which
    // is deliberate and is why this seeds one before asserting the park.
    advertise(pool, act, "booking.outreach", now).await?;

    // With a registry that cannot send this letter, a *new* approval is
    // refused rather than queued. The alternative is what production would
    // have done: accept the band's yes, park the action, and cancel it a day
    // later by a sweep nothing shows them.
    let refused_key = IdempotencyKey::parse("gig-approve-blocked").expect("valid key");
    match approve_gig_proposal(pool, act, wroclaw, &refused_key, now, None).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("nothing can send this letter yet"),
            "the refusal did not name the missing sender: {sentence}"
        ),
        other => {
            return Err(
                format!("an approval was taken with nothing able to send it: {other:?}").into(),
            );
        }
    }
    // And nothing was written for it — a refused approval leaves no action to
    // cancel later.
    let actions_now = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_autopilot_actions WHERE workspace_id = $1",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(actions_now, 1, "a refused approval still queued an action");

    let parked = repository(pool, url)
        .claim_due_autonomous_actions(WorkspaceId::from_uuid(act), 8, now)
        .await?;
    assert!(
        !parked
            .iter()
            .any(|candidate| candidate.id.into_uuid() == action_id),
        "the outreach was claimed with no executor advertising gig.outreach"
    );
    advertise(pool, act, "gig.outreach", now).await?;
    // A parked action returns to the queue five minutes later rather than
    // burning an attempt, so the claim that follows the switch-on has to be
    // past that window — the park is a delay, not a rejection.
    let after_park = now + time::Duration::minutes(6);
    let repository = repository(pool, url);
    let claimed = repository
        .claim_due_autonomous_actions(WorkspaceId::from_uuid(act), 8, after_park)
        .await?;
    let action = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == action_id)
        .ok_or("the queued outreach was not claimable")?;
    let executed = repository
        .execute_action(WorkspaceId::from_uuid(act), action, after_park)
        .await;
    assert!(
        executed.is_err(),
        "a blocked promoter did not stop the outreach"
    );
    let reserved = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_contact_governor
         WHERE workspace_id = $1 AND last_action_id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        reserved, 0,
        "the promoter the governor admitted kept a reservation from a letter nobody received"
    );
    let emitted = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM viryaos_autopilot_action_emissions
         WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(emitted, 0, "a partial letter reached the outbox");

    // ── A city that no longer proposes refuses rather than sending ──────────
    sqlx::query(
        "INSERT INTO events
            (workspace_id, city_id, slug, title, venue, starts_at, status, published_at)
         VALUES ($1, $2, 'already-booked', 'Booked', 'Klub X',
                 now() + interval '30 days', 'published', now())",
    )
    .bind(act)
    .bind(wroclaw)
    .execute(pool)
    .await?;
    let second_key = IdempotencyKey::parse("gig-approve-2").expect("valid key");
    match approve_gig_proposal(pool, act, wroclaw, &second_key, now, None).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("already a show"),
            "the refusal did not say what changed: {sentence}"
        ),
        other => {
            return Err(format!("a booked city was approved anyway: {other:?}").into());
        }
    }

    // A city nobody has an audience in is not on the board at all.
    match approve_gig_proposal(pool, act, Uuid::now_v7(), &second_key, now, None).await {
        Err(GigOutreachError::NotFound) => {}
        other => return Err(format!("an unknown city was not refused: {other:?}").into()),
    }

    two_promoters_with_one_name_both_receive_the_letter(pool).await?;
    a_second_approval_for_a_city_already_written_to_is_a_sentence(pool).await?;
    a_reply_belongs_to_the_letter_that_preceded_it(pool).await?;

    // ── The window is usable (O.2) ──────────────────────────────────────────
    // A fresh approval, immediately called back. `processing` deliberately
    // cannot be: a letter that is going out is gone, and a cancel that
    // pretended otherwise would be a lie with a promoter on the other end.
    unblock_contact(pool, act, "bogdan@example.com").await?;
    let callback_key = IdempotencyKey::parse("gig-approve-callback").expect("valid key");
    let queued = approve_gig_proposal(pool, act, wroclaw, &callback_key, now, None).await;
    if let Ok(GigOutreachOutcome::Queued { action_id, .. }) = queued {
        let called_back = sqlx::query_scalar::<_, String>(
            r#"
            UPDATE viryaos_autopilot_actions
            SET status = 'cancelled', finished_at = now()
            WHERE workspace_id = $1 AND id = $2
              AND (
                    status = 'awaiting_approval'
                 OR (status = 'queued' AND attempt_count = 0 AND available_at > now())
              )
            RETURNING status
            "#,
        )
        .bind(act)
        .bind(action_id)
        .fetch_optional(pool)
        .await?;
        assert_eq!(
            called_back.as_deref(),
            Some("cancelled"),
            "an approved letter still inside its hold window could not be called back"
        );
    }

    Ok(())
}

/// Approving a city whose letter is still in flight is answered, not crashed.
///
/// The action ledger has a partial unique index over the unfinished states, so
/// the second write collides. Left to the database, the band's second click
/// returned a unique-violation — a 503 that says the system is broken when the
/// truth is that they already said yes.
async fn a_second_approval_for_a_city_already_written_to_is_a_sentence(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool).await?;
    let lublin = city_in(pool, "lublin", "PL", 51.25, 22.57).await?;
    for index in 0..60 {
        reachable_fan(pool, act, lublin, &format!("lublin{index}@example.com")).await?;
    }
    played_show(pool, act, lublin, "Klub L", "lublin-show", 30).await?;
    promoter(pool, act, lublin, "Ewa", "ewa@example.com", 70).await?;

    let first = IdempotencyKey::parse("gig-lublin-1").expect("valid key");
    let GigOutreachOutcome::Queued { .. } =
        approve_gig_proposal(pool, act, lublin, &first, now, None).await?
    else {
        return Err("the first Lublin approval did not queue".into());
    };

    // A different key, so this is a genuinely new approval rather than a replay.
    let second = IdempotencyKey::parse("gig-lublin-2").expect("valid key");
    match approve_gig_proposal(pool, act, lublin, &second, now, None).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("already approved this city"),
            "the second approval did not explain itself: {sentence}"
        ),
        other => {
            return Err(format!("a second letter to one city was accepted: {other:?}").into());
        }
    }

    Ok(())
}

/// One reply, one letter: the most recent outbound touch before it.
///
/// A promoter written to twice in a week — a booking approach and a gig
/// proposal's letter — used to answer both, because each measurement asked
/// only "was there an inbound inside my seven days". One reply became two
/// successes, and the reason tally believed twice as much evidence existed as
/// there was.
async fn a_reply_belongs_to_the_letter_that_preceded_it(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let act = workspace(pool).await?;
    let city_id = city_in(pool, "reply-owner", "PL", 51.75, 19.46).await?;
    promoter(pool, act, city_id, "Dorota", "dorota@example.com", 70).await?;
    let target = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM viryaos_booking_targets
         WHERE workspace_id = $1 AND contact_email = 'dorota@example.com'",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;

    let first_letter = OffsetDateTime::now_utc() - time::Duration::days(5);
    let second_letter = first_letter + time::Duration::days(2);
    let reply_at = second_letter + time::Duration::hours(6);
    for (phase, direction, at, key) in [
        ("initial", "outbound", first_letter, "letter-one"),
        ("initial", "outbound", second_letter, "letter-two"),
        ("initial", "inbound", reply_at, "the-reply"),
    ] {
        sqlx::query(
            "INSERT INTO viryaos_booking_interactions
                (workspace_id, target_id, direction, phase, source_key, occurred_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(act)
        .bind(target)
        .bind(direction)
        .bind(phase)
        .bind(key)
        .bind(at)
        .execute(pool)
        .await?;
    }

    // The observation is the same query the worker runs, asked once per letter.
    let observed = |sent_at: OffsetDateTime| async move {
        sqlx::query_scalar::<_, f64>(
            r#"
            SELECT CASE WHEN EXISTS (
                SELECT 1 FROM viryaos_booking_interactions AS reply
                WHERE reply.workspace_id=$1 AND reply.target_id=$2
                  AND reply.direction='inbound'
                  AND reply.occurred_at >= $3
                  AND reply.occurred_at < $3 + INTERVAL '7 days'
                  AND NOT EXISTS (
                      SELECT 1 FROM viryaos_booking_interactions AS newer
                      WHERE newer.workspace_id=reply.workspace_id
                        AND newer.target_id=reply.target_id
                        AND newer.direction='outbound'
                        AND newer.occurred_at > $3
                        AND newer.occurred_at <= reply.occurred_at
                  )
            ) THEN 1.0::double precision ELSE 0.0::double precision END
            "#,
        )
        .bind(act)
        .bind(target)
        .bind(sent_at)
        .fetch_one(pool)
        .await
    };
    assert!(
        (observed(second_letter).await? - 1.0).abs() < f64::EPSILON,
        "the letter the promoter actually answered did not get the reply"
    );
    assert!(
        observed(first_letter).await?.abs() < f64::EPSILON,
        "an older letter still claimed a reply that came after a newer one"
    );

    Ok(())
}

/// Two people who book the same city can share a display name, and both are
/// recipients.
///
/// The booking list is unique on the contact address, so the name is not an
/// identity. Resolving the letter by name addressed the highest-ranked
/// namesake twice and never wrote to the other, while the console showed both
/// — which quietly breaks the promise that everybody who books the room hears.
async fn two_promoters_with_one_name_both_receive_the_letter(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool).await?;
    let poznan = city_in(pool, "poznan-namesakes", "PL", 52.4, 16.9).await?;
    for index in 0..60 {
        reachable_fan(
            pool,
            act,
            poznan,
            &format!("namesake-fan{index}@example.com"),
        )
        .await?;
    }
    played_show(pool, act, poznan, "Klub Y", "namesake-show", 30).await?;
    // Two different people, one name, two addresses — which is exactly what
    // the booking list's own uniqueness allows.
    promoter(pool, act, poznan, "Anna", "anna.one@example.com", 70).await?;
    promoter(pool, act, poznan, "Anna", "anna.two@example.com", 60).await?;

    let key = IdempotencyKey::parse("gig-namesakes").expect("valid key");
    let GigOutreachOutcome::Queued {
        action_id,
        recipients,
        ..
    } = approve_gig_proposal(pool, act, poznan, &key, now, None).await?
    else {
        return Err("the namesake proposal did not queue".into());
    };
    assert_eq!(
        recipients.len(),
        2,
        "a promoter sharing a name with another was dropped: {recipients:?}"
    );

    let addressed = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT payload -> 'recipients' FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    let ids: Vec<String> = addressed
        .as_array()
        .ok_or("the recipient set is not an array")?
        .iter()
        .filter_map(|entry| entry.get("target_id").and_then(|id| id.as_str()))
        .map(ToOwned::to_owned)
        .collect();
    assert_eq!(ids.len(), 2, "two recipients, two rows: {ids:?}");
    assert_ne!(ids[0], ids[1], "one promoter was addressed twice");

    Ok(())
}

/// 4G.5 — a settled proposal has its reasons scored.
///
/// The whole reason `Reason` is structured is that "which kind of evidence
/// predicts a booking" is a question the system can answer about itself. This
/// drives the chain the measurements travel: approve → the letter goes out →
/// a promoter answers inside the window → a show lands on the calendar — and
/// then asks the tally whether it saw what happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_settled_proposal_has_its_reasons_scored() -> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_infra::gig_planning::proposal_track_record;

    let database = DisposableDatabase::create().await?;
    let pool = &database.pool;
    let result = async {
        let now = OffsetDateTime::now_utc();
        let act = workspace(pool).await?;
        let wroclaw = city(pool, "wroclaw").await?;
        for index in 0..60 {
            reachable_fan(pool, act, wroclaw, &format!("fan{index}@example.com")).await?;
        }
        played_show(pool, act, wroclaw, "Klub X", "show-1", 40).await?;
        promoter(pool, act, wroclaw, "Anna", "anna@example.com", 70).await?;
        promoter(pool, act, wroclaw, "Bogdan", "bogdan@example.com", 60).await?;
        let anna = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM viryaos_booking_targets
             WHERE workspace_id = $1 AND contact_email = 'anna@example.com'",
        )
        .bind(act)
        .fetch_one(pool)
        .await?;
        let bogdan = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM viryaos_booking_targets
             WHERE workspace_id = $1 AND contact_email = 'bogdan@example.com'",
        )
        .bind(act)
        .fetch_one(pool)
        .await?;

        let key = IdempotencyKey::parse("gig-score-1").expect("valid key");
        let GigOutreachOutcome::Queued { action_id, .. } =
            approve_gig_proposal(pool, act, wroclaw, &key, now, None).await?
        else {
            return Err("the proposal did not queue".into());
        };

        // Before the letter runs there is no track record to read — an
        // unexecuted approval is a promise, not an outcome.
        let record = proposal_track_record(pool, act).await?;
        let proposal = record
            .proposals
            .iter()
            .find(|entry| entry.city == "wroclaw")
            .ok_or("the approved proposal is absent from its own track record")?;
        assert!(
            !proposal.is_settled(),
            "a letter that never left must not score"
        );
        assert!(
            record.by_reason.is_empty(),
            "an unsettled proposal must not move the tallies"
        );

        // The letter leaves and Anna answers inside the week — the rows the
        // measurement pipeline itself writes, driven the way it writes them.
        // The ledger enforces the real transition chain, so the fixture walks
        // it rather than teleporting the action to succeeded.
        for status in ["processing", "succeeded"] {
            sqlx::query(
                "UPDATE viryaos_autopilot_actions
                 SET status = $3, finished_at = $4
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(act)
            .bind(action_id)
            .bind(status)
            .bind(now)
            .execute(pool)
            .await?;
        }
        // One measurement per recipient — the letter went to two promoters and
        // each gets their own reply window. Under the old
        // (action, measurement_kind) uniqueness the second row could not exist;
        // under the old outcome index the second completion raised instead of
        // settling. Both recipients are driven through the repository's real
        // completion so a regression in either key fails here.
        let repository = PostgresAutopilotRepository::new(
            pool.clone(),
            &DatabaseConfig {
                url: database.url.clone(),
                max_connections: 4,
                connect_timeout: Duration::from_secs(3),
                ping_timeout: Duration::from_secs(2),
                operation_timeout: Duration::from_secs(10),
                lock_timeout: Duration::from_secs(1),
            },
        );
        for subject in [anna, bogdan] {
            let measurement_id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO viryaos_autopilot_measurements
                    (id, workspace_id, action_id, measurement_kind, subject_id,
                     action_finished_at, baseline_value, due_at, status,
                     available_at, started_at)
                 VALUES ($1, $2, $3, 'booking_reply_7d', $4, $5, 0,
                         $5 + interval '7 days', 'processing', $5, $5)",
            )
            .bind(measurement_id)
            .bind(act)
            .bind(action_id)
            .bind(subject)
            .bind(now)
            .execute(pool)
            .await?;
            let measurement = ClaimedAutopilotMeasurement {
                id: AutopilotMeasurementId::from(measurement_id),
                action_id: AutopilotActionId::from(action_id),
                kind: AutopilotMeasurementKind::BookingReply7d,
                subject_id: subject,
                baseline_value: 0.0,
                action_finished_at: now,
                attempt_number: 1,
            };
            let effect = assess_measurement_effect(&measurement, 1.0)
                .ok_or("a reply the worker could not classify")?;
            AutopilotMeasurementRepository::complete_measurement(
                &repository,
                WorkspaceId::from_uuid(act),
                &measurement,
                1.0,
                effect,
                now + time::Duration::days(3),
            )
            .await
            .map_err(|error| format!("the second recipient's outcome did not settle: {error}"))?;
        }

        // And the band gets the show — the outcome the proposal was for.
        sqlx::query(
            "INSERT INTO events
                (workspace_id, city_id, slug, title, venue, starts_at,
                 status, published_at)
             VALUES ($1, $2, 'the-gig', 'The Gig', 'Klub X',
                     now() + interval '45 days', 'published', now())",
        )
        .bind(act)
        .bind(wroclaw)
        .execute(pool)
        .await?;

        let record = proposal_track_record(pool, act).await?;
        let proposal = record
            .proposals
            .iter()
            .find(|entry| entry.city == "wroclaw")
            .ok_or("the settled proposal vanished from the track record")?;
        assert!(proposal.is_settled(), "a finished window did not settle");
        assert_eq!(
            proposal.recipients, 2,
            "the letter went to two promoters and the record must say so"
        );
        assert_eq!(proposal.replies, 2, "both replies did not score");
        assert!(
            proposal.show_booked,
            "the show the letter produced did not score"
        );
        assert!(
            !proposal.reasons.is_empty(),
            "the scored proposal lost the reasons it was approved on"
        );

        // Every reason the proposal carried is now a tally with a reply and a
        // show against it — which is the answer "did the evidence hold".
        for reason in &proposal.reasons {
            let kind = serde_json::to_value(reason)
                .expect("reason encodes")
                .get("kind")
                .and_then(|value| value.as_str())
                .expect("a reason always has a kind")
                .to_owned();
            let score = record
                .by_reason
                .iter()
                .find(|score| score.kind == kind)
                .unwrap_or_else(|| panic!("reason {kind} was carried but never scored"));
            assert_eq!(score.proposals, 1);
            assert_eq!(score.replies, 1, "reason {kind} did not see the reply");
            assert_eq!(score.shows, 1, "reason {kind} did not see the show");
        }

        // The named room is the stronger signal, and it is reported apart from
        // "a show in that city": the letter asked for Klub X and Klub X is
        // what happened.
        assert!(
            proposal.show_booked_at_venue,
            "a show at the room the letter named did not register as one"
        );

        // A show created outside the attribution window is not this
        // proposal's. Unbounded, every city the band ever plays eventually
        // marks every proposal ever made for it a success, and a tally where
        // every reason works is one nobody can act on.
        let late_city = city_in(pool, "late-city", "PL", 54.35, 18.65).await?;
        let late_key = IdempotencyKey::parse("gig-score-late").expect("valid key");
        for index in 0..60 {
            reachable_fan(pool, act, late_city, &format!("late{index}@example.com")).await?;
        }
        played_show(pool, act, late_city, "Klub Z", "late-show", 30).await?;
        promoter(pool, act, late_city, "Celina", "celina@example.com", 70).await?;
        let GigOutreachOutcome::Queued {
            action_id: late_action,
            ..
        } = approve_gig_proposal(pool, act, late_city, &late_key, now, None).await?
        else {
            return Err("the late-city proposal did not queue".into());
        };
        // Approved a year ago, from the ledger's point of view.
        sqlx::query(
            "UPDATE viryaos_autopilot_decisions SET evaluated_at = now() - interval '365 days'
             WHERE workspace_id = $1
               AND id = (SELECT decision_id FROM viryaos_autopilot_actions
                         WHERE workspace_id = $1 AND id = $2)",
        )
        .bind(act)
        .bind(late_action)
        .execute(pool)
        .await?;
        sqlx::query(
            "INSERT INTO events
                (workspace_id, city_id, slug, title, venue, starts_at,
                 status, published_at)
             VALUES ($1, $2, 'much-later', 'Much Later', 'Klub Z',
                     now() + interval '20 days', 'published', now())",
        )
        .bind(act)
        .bind(late_city)
        .execute(pool)
        .await?;
        let record = proposal_track_record(pool, act).await?;
        let late = record
            .proposals
            .iter()
            .find(|entry| entry.city == "late-city")
            .ok_or("the year-old proposal vanished from the track record")?;
        assert!(
            !late.show_booked,
            "a show booked a year after the approval was credited to it"
        );

        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    database.drop_database().await;
    result
}

/// N.10 — the band fixes the first line, the fix is the letter, and the fix
/// is on the ledger.
///
/// The gig approvals gain the generic approve-with-edit machinery (2.9): the
/// opening line is the band's voice, so a correction is what the promoter
/// reads — recorded field-by-field so the voice signal (§4d-3.2) can measure
/// how wrong the machine was. What the gate refuses writes nothing at all,
/// because a typo must not be able to burn the click's idempotency key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_fixed_opening_line_is_the_letter_and_the_ledger()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run_revision(&database.pool).await;
    database.drop_database().await;
    result
}

async fn run_revision(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;

    let now = OffsetDateTime::now_utc();
    let act = workspace(pool).await?;
    let gdansk = city(pool, "gdansk-rev").await?;
    for index in 0..60 {
        reachable_fan(pool, act, gdansk, &format!("rev{index}@example.com")).await?;
    }
    played_show(pool, act, gdansk, "Klub G", "rev-show", 30).await?;
    promoter(pool, act, gdansk, "Feliks", "feliks@example.com", 70).await?;

    // ── A locked field refuses the approval, and spends nothing ──────────
    // `venue` is what the letter points at, not a word a human reads — the
    // edit box is not a back door into the proposal's facts.
    let key = IdempotencyKey::parse("gig-rev-1").expect("valid key");
    let venue_edit = BTreeMap::from([("venue".to_owned(), "Anywhere Else".to_owned())]);
    match approve_gig_proposal(pool, act, gdansk, &key, now, Some(&venue_edit)).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("`venue` cannot be revised"),
            "the refusal did not name the locked field: {sentence}"
        ),
        other => return Err(format!("a venue edit was taken: {other:?}").into()),
    }
    // An emptied line is a reject wearing an edit's clothes, and an empty map
    // changes nothing — both refuse in their own words, and neither spends
    // the key.
    let emptied_edit = BTreeMap::from([("opening_line".to_owned(), "   ".to_owned())]);
    match approve_gig_proposal(pool, act, gdansk, &key, now, Some(&emptied_edit)).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("cannot be emptied"),
            "an emptied line was not refused in its own words: {sentence}"
        ),
        other => return Err(format!("an emptied edit was taken: {other:?}").into()),
    }
    let empty_edit = BTreeMap::new();
    match approve_gig_proposal(pool, act, gdansk, &key, now, Some(&empty_edit)).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("identical to the draft"),
            "an empty revision was not refused: {sentence}"
        ),
        other => return Err(format!("an empty revision was taken: {other:?}").into()),
    }
    let (actions, decisions, audits) = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT (SELECT COUNT(*) FROM viryaos_autopilot_actions WHERE workspace_id = $1),
                (SELECT COUNT(*) FROM viryaos_autopilot_decisions WHERE workspace_id = $1),
                (SELECT COUNT(*) FROM operator_actions WHERE workspace_id = $1)",
    )
    .bind(act)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        (actions, decisions, audits),
        (0, 0, 0),
        "a refused revision wrote something"
    );

    // ── The same key, now carrying a real edit ────────────────────────────
    // The refusal spent nothing, so this is still the click the band made.
    let edit = BTreeMap::from([(
        "opening_line".to_owned(),
        "Gdansk, we keep missing you — Klub G on a Friday fixes that.".to_owned(),
    )]);
    let GigOutreachOutcome::Queued {
        action_id,
        opening_line,
        ..
    } = approve_gig_proposal(pool, act, gdansk, &key, now, Some(&edit)).await?
    else {
        return Err("the unburnt key did not queue the fixed letter".into());
    };
    assert_eq!(
        opening_line, "Gdansk, we keep missing you — Klub G on a Friday fixes that.",
        "the outcome did not return the operator's words"
    );

    // The payload the action carries is what the executor renders — verbatim.
    let stored_line = sqlx::query_scalar::<_, String>(
        "SELECT payload ->> 'opening_line' FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        stored_line, opening_line,
        "the queued letter is not the fix"
    );

    // One ledger row per edited field, hung off the approval's own audit
    // row: the machine's words, the band's words, and how far they moved.
    let rows = sqlx::query_as::<_, (String, String, String, String, i32, serde_json::Value)>(
        "SELECT audit.action, revision.field, revision.before_text,
                revision.after_text, revision.distance_chars, audit.details
         FROM viryaos_draft_revisions AS revision
         JOIN operator_actions AS audit ON audit.id = revision.operation_id
         WHERE revision.workspace_id = $1 AND revision.action_id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_all(pool)
    .await?;
    let [(action, field, before, after, distance, details)] = rows.as_slice() else {
        return Err(format!("one field edited, one ledger row expected: {rows:?}").into());
    };
    assert_eq!(action, "approve_gig_proposal");
    assert_eq!(field, "opening_line");
    assert!(
        before.ends_with('.'),
        "the machine's line was not kept: {before}"
    );
    assert_eq!(after, &opening_line);
    assert!(*distance > 0, "the edit recorded no distance");
    assert_eq!(
        details["revision_fields"],
        serde_json::json!(["opening_line"])
    );

    // ── A replayed key does not re-apply anything ─────────────────────────
    let other_edit = BTreeMap::from([(
        "opening_line".to_owned(),
        "a different fix entirely".to_owned(),
    )]);
    match approve_gig_proposal(pool, act, gdansk, &key, now, Some(&other_edit)).await? {
        GigOutreachOutcome::Replayed {
            action_id: replayed,
            ..
        } => assert_eq!(replayed, action_id),
        other => return Err(format!("a replayed key queued again: {other:?}").into()),
    }
    let still = sqlx::query_scalar::<_, String>(
        "SELECT payload ->> 'opening_line' FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(still, opening_line, "the replay rewrote the stored letter");

    // ── A no-op is not an edit, and the evidence is not editable ──────────
    // The no-op must be the machine's own line for the fresh city, recomputed
    // the way the approval recomputes it — reach is geographic, so a second
    // seeded city changes the count a guessed sentence would miss.
    let sopot = city(pool, "sopot-rev").await?;
    for index in 0..60 {
        reachable_fan(pool, act, sopot, &format!("sopot{index}@example.com")).await?;
    }
    played_show(pool, act, sopot, "Klub G", "sopot-show", 30).await?;
    promoter(pool, act, sopot, "Feliks", "feliks-sopot@example.com", 70).await?;
    let noop_key = IdempotencyKey::parse("gig-rev-noop").expect("valid key");
    let opportunities = crowdrelay_infra::gig_planning::city_opportunities(pool, act, now).await?;
    let sopot_opportunity = opportunities
        .iter()
        .find(|candidate| candidate.city_id == CityId::from_uuid(sopot))
        .expect("sopot is on the board");
    let intent = crowdrelay_infra::gig_planning::stated_intent(
        &crowdrelay_infra::tenant_settings::TenantSettingsRepository::new(pool.clone()),
        act,
    )
    .await?;
    let sopot_line = crowdrelay_domain::gig_plan::plan_gig(sopot_opportunity, intent)
        .expect("sopot still proposes");
    // The stored line is the Polish one, because the room is Polish. Editing
    // it to the same words is the no-op this refuses.
    let sopot_line = crowdrelay_domain::gig_letter::opening_line(
        &sopot_line,
        crowdrelay_domain::gig_letter::LetterLanguage::Polish,
    );
    let noop_edit = BTreeMap::from([("opening_line".to_owned(), sopot_line)]);
    match approve_gig_proposal(pool, act, sopot, &noop_key, now, Some(&noop_edit)).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("identical to the draft"),
            "the no-op was not refused in the draft's words: {sentence}"
        ),
        other => return Err(format!("a no-op edit was taken: {other:?}").into()),
    }
    // `reasons` is what the machine counted — disagreeing with a number is a
    // refusal, not a rewrite. The same key is still unspent after both.
    let reasons_edit = BTreeMap::from([("reasons".to_owned(), "the room loves us".to_owned())]);
    match approve_gig_proposal(pool, act, sopot, &noop_key, now, Some(&reasons_edit)).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("`reasons` cannot be revised"),
            "the evidence was editable: {sentence}"
        ),
        other => return Err(format!("a reasons edit was taken: {other:?}").into()),
    }
    let GigOutreachOutcome::Queued { .. } =
        approve_gig_proposal(pool, act, sopot, &noop_key, now, Some(&edit)).await?
    else {
        return Err("twice-refused edits still burnt the key".into());
    };

    Ok(())
}

fn repository(pool: &PgPool, url: &str) -> PostgresAutopilotRepository {
    PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: url.to_owned(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    )
}

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Test Act')")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .execute(pool)
        .await?;
    Ok(id)
}

async fn city(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    city_in(pool, slug, "PL", 51.1, 17.0).await
}

async fn city_in(
    pool: &PgPool,
    slug: &str,
    country_code: &str,
    latitude: f64,
    longitude: f64,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, $1, $2, $3, $4)
         ON CONFLICT (country_code, slug)
         DO UPDATE SET latitude = $3, longitude = $4",
    )
    .bind(slug)
    .bind(country_code)
    .bind(latitude)
    .bind(longitude)
    .execute(pool)
    .await?;
    Ok(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM cities WHERE country_code = $2 AND slug = $1",
        )
        .bind(slug)
        .bind(country_code)
        .fetch_one(pool)
        .await?,
    )
}

/// Active, consented, opted into nearby gigs, inside the radius they chose.
/// All four, or the reachability gate counts nothing and the test proves
/// nothing.
async fn reachable_fan(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    email: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO fans (workspace_id, normalized_email, status)
         VALUES ($1, $2, 'active') RETURNING id",
    )
    .bind(workspace_id)
    .bind(email)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents
            (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
         VALUES ($1, $2, 'marketing', true, 'v1', 'signup', now())",
    )
    .bind(workspace_id)
    .bind(fan)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_location_preferences
            (workspace_id, fan_id, city_id, radius_km, nearby_gigs_enabled)
         VALUES ($1, $2, $3, 50, true)",
    )
    .bind(workspace_id)
    .bind(fan)
    .bind(city_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_city_interests (workspace_id, fan_id, city_id)
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(workspace_id)
    .bind(fan)
    .bind(city_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn played_show(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    venue: &str,
    slug: &str,
    days_ago: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO events (workspace_id, city_id, slug, title, venue, starts_at, status)
         VALUES ($1, $2, $3, $3, $4, now() - ($5 || ' days')::interval, 'completed')",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(slug)
    .bind(venue)
    .bind(days_ago.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

async fn promoter(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    name: &str,
    email: &str,
    score: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_booking_targets
            (workspace_id, city_id, target_kind, display_name, contact_email,
             relationship_score, capacity)
         VALUES ($1, $2, 'promoter', $3, $4, $5, 300)",
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(name)
    .bind(email)
    .bind(score)
    .execute(pool)
    .await?;
    Ok(())
}

/// An executor advertising one capability, the way the worker's heartbeat does.
async fn advertise(
    pool: &PgPool,
    workspace_id: Uuid,
    capability: &str,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_executor_instances
            (workspace_id, executor_id, version, manifest_sha, observed_at, expires_at)
         VALUES ($1,'n8n-gig-test','1','sha',$2,$3)
         ON CONFLICT DO NOTHING",
    )
    .bind(workspace_id)
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO viryaos_executor_capabilities
            (workspace_id, executor_id, capability, capability_version, observed_at, expires_at)
         VALUES ($1,'n8n-gig-test',$2,'1',$3,$4)",
    )
    .bind(workspace_id)
    .bind(capability)
    .bind(now)
    .bind(now + time::Duration::minutes(30))
    .execute(pool)
    .await?;
    Ok(())
}

/// A do-not-contact on one address, the way an operator sets one.
/// Lifts the do-not-contact so a later approval in the same test can reach the
/// whole room again.
async fn unblock_contact(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        // `next_contact_after >= last_outbound_at` is a CHECK, so the window
        // is cleared by moving both backwards rather than only one.
        "UPDATE viryaos_contact_governor
         SET do_not_contact = false,
             last_outbound_at = now() - INTERVAL '30 days',
             next_contact_after = now() - INTERVAL '29 days'
         WHERE workspace_id = $1 AND normalized_contact = $2",
    )
    .bind(workspace_id)
    .bind(email)
    .execute(pool)
    .await?;
    Ok(())
}

async fn block_contact(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO viryaos_contact_governor
            (workspace_id, normalized_contact, last_context, last_outbound_at,
             next_contact_after, do_not_contact)
         VALUES ($1, $2, 'manual', $3, $3, true)",
    )
    .bind(workspace_id)
    .bind(email)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}
