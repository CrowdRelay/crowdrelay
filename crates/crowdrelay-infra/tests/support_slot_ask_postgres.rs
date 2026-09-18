//! The support-slot ask (§12-6, N.5) — one open slot, one letter to the act
//! that already holds the room.
//!
//! N.10 extends the same approval: the roster operator may fix the letter's
//! `opening_line` at approval time, reviewed by the generic revision machinery
//! before anything is queued — a refused edit refuses the whole approval and
//! writes nothing, an accepted one lands in the payload and leaves a
//! `viryaos_draft_revisions` row hung off the approval's audit row.

use std::time::Duration;

use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::config::DatabaseConfig;
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
        let name = format!("crowdrelay_peeracts_{}", Uuid::now_v7().simple());
        let mut admin = PgConnection::connect(&base).await?;
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut admin)
            .await?;
        drop(admin);
        let (head, _) = base.rsplit_once('/').ok_or("database url has no path")?;
        let url = format!("{head}/{name}");
        let pool = PgPoolOptions::new()
            .max_connections(4)
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

/// §N.5 — one open slot produces one approved ask to the act that already
/// has the room.
///
/// The planner stopped at naming the cheapest gig in the system; this drives
/// the write half. What the database has to prove: the letter leaves from the
/// *headliner's* workspace (the promoter relationship and the night are
/// theirs), the slot and the pairing arithmetic are recomputed rather than
/// trusted, one show produces one in-flight ask, a replayed key answers from
/// the ledger, and every expired shape of the offer — cancelled, past, draft,
/// filled — is the same refusal. And at the end of it the outbox carries the
/// ask's own template, because a letter rendered as a booking proposal
/// announces a night that is already held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn one_open_slot_produces_one_ask_to_the_room_that_offered_it()
-> Result<(), Box<dyn std::error::Error>> {
    let database = DisposableDatabase::create().await?;
    let result = run_support_slot_ask(&database.pool, &database.url).await;
    database.drop_database().await;
    result
}

async fn run_support_slot_ask(pool: &PgPool, url: &str) -> Result<(), Box<dyn std::error::Error>> {
    use crowdrelay_application::IdempotencyKey;
    use crowdrelay_application::autopilot::AutopilotActionRepository;
    use crowdrelay_infra::autopilot::PostgresAutopilotRepository;
    use crowdrelay_infra::gig_outreach::{
        GigOutreachError, SupportSlotAskOutcome, approve_support_slot_ask as approve_ask,
    };

    let now = OffsetDateTime::now_utc();
    let org = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO organizations (slug, name) VALUES ('label-n5', 'Label N5') RETURNING id",
    )
    .fetch_one(pool)
    .await?;
    let member = |name: &str| {
        let pool = pool.clone();
        let name = name.to_owned();
        async move {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO workspaces (id, slug, name, organization_id)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(format!("ws-{name}"))
            .bind(&name)
            .bind(org)
            .execute(&pool)
            .await?;
            Ok::<Uuid, Box<dyn std::error::Error>>(id)
        }
    };
    let head = member("headliner").await?;
    let support = member("support").await?;
    // The refusal cast — each a headliner whose offer has expired a different
    // way, plus the supports that measure wrong.
    let head_cancelled = member("head-cancelled").await?;
    let head_filled = member("head-filled").await?;
    let head_past = member("head-past").await?;
    let head_draft = member("head-draft").await?;
    let head_silent = member("head-silent").await?;
    let head_alone = member("head-alone").await?;
    let silent_support = member("silent-support").await?;
    let echo_support = member("echo-support").await?;

    // A workspace on a different roster entirely — same request, no member.
    let foreign_org = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO organizations (slug, name) VALUES ('other-label', 'Other') RETURNING id",
    )
    .fetch_one(pool)
    .await?;
    let foreign_head = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO workspaces (id, slug, name, organization_id)
         VALUES ($1, 'ws-foreign-head', 'foreign-head', $2)",
    )
    .bind(foreign_head)
    .bind(foreign_org)
    .execute(pool)
    .await?;

    let city = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ('wroclaw-n5', 'wroclaw-n5', 'PL', 51.1, 17.0) RETURNING id",
    )
    .fetch_one(pool)
    .await?;
    // A city in the catalogue with no coordinates — measurable is a property
    // of the city, and this one has none.
    let unmeasurable = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code)
         VALUES ('coordinateless', 'coordinateless', 'PL') RETURNING id",
    )
    .fetch_one(pool)
    .await?;
    let head_unmeasurable = member("head-unmeasurable").await?;

    let reachable = |ws: Uuid, email: &str| {
        let pool = pool.clone();
        let email = email.to_owned();
        async move {
            let fan = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO fans (workspace_id, normalized_email, status)
                 VALUES ($1, $2, 'active') RETURNING id",
            )
            .bind(ws)
            .bind(&email)
            .fetch_one(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO fan_consents
                    (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
                 VALUES ($1, $2, 'marketing', true, 'v1', 'signup', now())",
            )
            .bind(ws)
            .bind(fan)
            .execute(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO fan_location_preferences
                    (workspace_id, fan_id, city_id, radius_km, nearby_gigs_enabled)
                 VALUES ($1, $2, $3, 50, true)",
            )
            .bind(ws)
            .bind(fan)
            .bind(city)
            .execute(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO fan_city_interests (workspace_id, fan_id, city_id)
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(ws)
            .bind(fan)
            .bind(city)
            .execute(&pool)
            .await?;
            Ok::<(), Box<dyn std::error::Error>>(())
        }
    };
    // The headliner reaches sixty; the support reaches four, two of them the
    // same people — 5,000bp, under the pairing ceiling. The echo reaches four
    // and every one of them is already the headliner's — one crowd twice.
    for index in 0..60 {
        reachable(head, &format!("n5head{index}@example.com")).await?;
    }
    for index in 0..2 {
        reachable(support, &format!("n5head{index}@example.com")).await?;
        reachable(support, &format!("n5own{index}@example.com")).await?;
    }
    for index in 0..4 {
        reachable(echo_support, &format!("n5head{index}@example.com")).await?;
    }

    /// The confirmed show with room on the bill — `open_support_slots` is the
    /// promoter's own declaration, not an inference.
    async fn open_slot_event(
        pool: &PgPool,
        workspace_id: Uuid,
        city_id: Uuid,
        slug: &str,
    ) -> Result<Uuid, Box<dyn std::error::Error>> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO events
                (id, workspace_id, city_id, slug, title, venue, starts_at,
                 status, published_at, open_support_slots)
             VALUES ($1, $2, $3, $4, $4, 'Klub N5',
                     now() + interval '30 days', 'published', now(), 1)",
        )
        .bind(id)
        .bind(workspace_id)
        .bind(city_id)
        .bind(slug)
        .execute(pool)
        .await?;
        Ok(id)
    }
    let promoter = |ws: Uuid, name: &str, email: &str| {
        let pool = pool.clone();
        let name = name.to_owned();
        let email = email.to_owned();
        async move {
            sqlx::query(
                "INSERT INTO viryaos_booking_targets
                    (workspace_id, city_id, target_kind, display_name, contact_email,
                     relationship_score, capacity)
                 VALUES ($1, $2, 'promoter', $3, $4, 70, 300)",
            )
            .bind(ws)
            .bind(city)
            .bind(name)
            .bind(email)
            .execute(&pool)
            .await?;
            Ok::<(), Box<dyn std::error::Error>>(())
        }
    };
    // An executor advertising one capability, the way the worker's heartbeat
    // does — an instance with only a different capability is the registry
    // that cannot send this letter.
    let advertise = |ws: Uuid, capability: &str| {
        let pool = pool.clone();
        let capability = capability.to_owned();
        async move {
            sqlx::query(
                "INSERT INTO viryaos_executor_instances
                    (workspace_id, executor_id, version, manifest_sha, observed_at, expires_at)
                 VALUES ($1,'n8n-n5-test','1','sha',now(),now() + interval '30 minutes')
                 ON CONFLICT DO NOTHING",
            )
            .bind(ws)
            .execute(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO viryaos_executor_capabilities
                    (workspace_id, executor_id, capability, capability_version,
                     observed_at, expires_at)
                 VALUES ($1,'n8n-n5-test',$2,'1',now(),now() + interval '30 minutes')",
            )
            .bind(ws)
            .bind(capability)
            .execute(&pool)
            .await?;
            Ok::<(), Box<dyn std::error::Error>>(())
        }
    };

    // ── The happy path: one show, one slot, one letter. ─────────────────
    let event_id = open_slot_event(pool, head, city, "n5-head-show").await?;
    promoter(head, "Anna", "anna-n5@example.com").await?;
    advertise(head, "gig.outreach").await?;

    let key = IdempotencyKey::parse("n5-ask-1").expect("valid key");
    // A refused edit refuses the whole approval and spends nothing — the
    // same key is still the operator's to click with.
    let locked_edit =
        std::collections::BTreeMap::from([("venue".to_owned(), "Anywhere Else".to_owned())]);
    match approve_ask(
        pool,
        org,
        head,
        support,
        city,
        &key,
        now,
        Some(&locked_edit),
    )
    .await
    {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("`venue` cannot be revised"),
            "the refusal did not name the locked field: {sentence}"
        ),
        other => return Err(format!("a venue edit was taken: {other:?}").into()),
    }
    // The operator fixes the first line as they approve (N.10) — the letter
    // that queues is theirs, and the fix lands on the ledger with it.
    let edit = std::collections::BTreeMap::from([(
        "opening_line".to_owned(),
        "Say the word and the N5 slot is theirs.".to_owned(),
    )]);
    let outcome = approve_ask(pool, org, head, support, city, &key, now, Some(&edit)).await?;
    let (action_id, recipients, opening_line, show_date) = match outcome {
        SupportSlotAskOutcome::Queued {
            action_id,
            headliner,
            support: named,
            venue,
            show_date,
            recipients,
            opening_line,
            ..
        } => {
            assert_eq!(headliner, "headliner");
            assert_eq!(named, "support");
            assert_eq!(venue, "Klub N5");
            (action_id, recipients, opening_line, show_date)
        }
        other => return Err(format!("expected a queued ask, got {other:?}").into()),
    };
    assert_eq!(
        recipients,
        vec!["Anna".to_owned()],
        "the letter goes to the headliner's own promoter, nobody else's"
    );
    assert_eq!(
        opening_line, "Say the word and the N5 slot is theirs.",
        "the letter did not carry the operator's fix"
    );

    // The action lives on the headliner's workspace, subject the show — the
    // roster operator approved it, but the relationship it spends is the
    // headliner's.
    let (workspace_id, subject_kind, subject_id, status, payload) =
        sqlx::query_as::<_, (Uuid, String, Uuid, String, serde_json::Value)>(
            "SELECT workspace_id, subject_kind, subject_id, status, payload
         FROM viryaos_autopilot_actions WHERE id = $1",
        )
        .bind(action_id)
        .fetch_one(pool)
        .await?;
    assert_eq!(
        workspace_id, head,
        "the ask was queued under the wrong account"
    );
    assert_eq!(subject_kind, "event");
    assert_eq!(subject_id, event_id);
    assert_eq!(status, "queued");
    assert_eq!(payload["kind"], "request_gig_outreach");
    assert_eq!(
        payload["letter"]["support_slot_ask"]["support_act"],
        serde_json::Value::String("support".to_owned()),
        "the payload lost the name the ask was made for: {payload}"
    );
    assert_eq!(
        payload["letter"]["support_slot_ask"]["event_id"],
        serde_json::Value::String(event_id.to_string()),
    );
    assert_eq!(
        payload["letter"]["support_slot_ask"]["show_date"].as_str(),
        Some(show_date.as_str()),
    );
    assert_eq!(
        payload["opening_line"].as_str(),
        Some(opening_line.as_str())
    );

    // The fix is on the ledger too — the machine's words and the operator's,
    // one row per field, hung off the approval's own audit row by the
    // operation_id foreign key.
    let (before_text, after_text) = sqlx::query_as::<_, (String, String)>(
        "SELECT before_text, after_text FROM viryaos_draft_revisions
         WHERE workspace_id = $1 AND action_id = $2 AND field = 'opening_line'",
    )
    .bind(head)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert!(before_text.contains("Klub N5") && before_text.contains("support"));
    assert_eq!(after_text, opening_line);

    // ── The same click twice answers from the ledger — even when the retry
    // carries a different edit. The stored letter is the answer. ──────────
    let different_edit = std::collections::BTreeMap::from([(
        "opening_line".to_owned(),
        "a different fix entirely".to_owned(),
    )]);
    let replay = approve_ask(
        pool,
        org,
        head,
        support,
        city,
        &key,
        now,
        Some(&different_edit),
    )
    .await?;
    match replay {
        SupportSlotAskOutcome::Replayed {
            action_id: replayed,
            ..
        } => assert_eq!(replayed, action_id, "a retried click made a second letter"),
        other => return Err(format!("expected a replay, got {other:?}").into()),
    }
    let total_actions = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM viryaos_autopilot_actions WHERE workspace_id = $1",
    )
    .bind(head)
    .fetch_one(pool)
    .await?;
    assert_eq!(total_actions, 1, "the replay wrote a second action");
    let still = sqlx::query_scalar::<_, String>(
        "SELECT payload ->> 'opening_line' FROM viryaos_autopilot_actions
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(head)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(still, opening_line, "the replay rewrote the stored letter");

    // ── A second key while the first is unanswered is the same letter. ──
    let second = IdempotencyKey::parse("n5-ask-2").expect("valid key");
    match approve_ask(pool, org, head, support, city, &second, now, None).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("already"),
            "the second key did not explain itself: {sentence}"
        ),
        other => return Err(format!("a second ask for one show was taken: {other:?}").into()),
    }
    let total_actions = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM viryaos_autopilot_actions WHERE workspace_id = $1",
    )
    .bind(head)
    .fetch_one(pool)
    .await?;
    assert_eq!(total_actions, 1, "the refused second key still queued");

    // ── What the letter actually emits: the ask's own template, or a
    // promoter reading it books a night that is already held. ────────────
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: url.to_owned(),
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    );
    let claimed = repository
        .claim_due_autonomous_actions(WorkspaceId::from_uuid(head), 8, now)
        .await?;
    let claimed_action = claimed
        .iter()
        .find(|candidate| candidate.id.into_uuid() == action_id)
        .ok_or("the queued ask was not claimable")?;
    repository
        .execute_action(WorkspaceId::from_uuid(head), claimed_action, now)
        .await
        .map_err(|error| format!("the approved ask did not execute: {error}"))?;
    let emitted = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT payload FROM outbox_events
         WHERE workspace_id = $1 AND action_id = $2
           AND event_type = 'crowdrelay.gig.outreach_requested'",
    )
    .bind(head)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(
        emitted["template_key"], "support.slot.ask.v1",
        "the ask left wearing the proposal's template: {emitted}"
    );
    assert_eq!(emitted["support_act"], "support");
    assert_eq!(
        emitted["event_id"].as_str(),
        Some(event_id.to_string().as_str())
    );
    assert_eq!(emitted["show_date"].as_str(), Some(show_date.as_str()));
    assert_eq!(
        emitted["recipients"][0]["contact_email"], "anna-n5@example.com",
        "the letter addressed somebody other than the headliner's promoter"
    );
    let succeeded = sqlx::query_scalar::<_, String>(
        "SELECT status FROM viryaos_autopilot_actions WHERE id = $1",
    )
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(succeeded, "succeeded");

    // ── Every shape of an expired offer is the same refusal. ────────────
    sqlx::query(
        "INSERT INTO events
            (workspace_id, city_id, slug, title, venue, starts_at,
             status, published_at, open_support_slots)
         VALUES ($1, $2, 'n5-cancelled', 'x', 'Klub N5',
                 now() + interval '30 days', 'cancelled', now(), 1)",
    )
    .bind(head_cancelled)
    .bind(city)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO events
            (workspace_id, city_id, slug, title, venue, starts_at,
             status, published_at, open_support_slots)
         VALUES ($1, $2, 'n5-filled', 'x', 'Klub N5',
                 now() + interval '30 days', 'published', now(), 0)",
    )
    .bind(head_filled)
    .bind(city)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO events
            (workspace_id, city_id, slug, title, venue, starts_at,
             status, published_at, open_support_slots)
         VALUES ($1, $2, 'n5-past', 'x', 'Klub N5',
                 now() - interval '3 days', 'published', now() - interval '60 days', 1)",
    )
    .bind(head_past)
    .bind(city)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO events
            (workspace_id, city_id, slug, title, venue, starts_at,
             status, open_support_slots)
         VALUES ($1, $2, 'n5-draft', 'x', 'Klub N5',
                 now() + interval '30 days', 'draft', 1)",
    )
    .bind(head_draft)
    .bind(city)
    .execute(pool)
    .await?;
    for (label, headliner) in [
        ("cancelled", head_cancelled),
        ("filled", head_filled),
        ("past", head_past),
        ("draft", head_draft),
    ] {
        let variant_key = IdempotencyKey::parse(format!("n5-ask-{label}")).expect("valid key");
        match approve_ask(pool, org, headliner, support, city, &variant_key, now, None).await {
            Err(GigOutreachError::Refused(sentence)) => assert!(
                sentence.contains("no published show with an open slot"),
                "the {label} refusal did not say what changed: {sentence}"
            ),
            other => {
                return Err(format!("a {label} show produced an ask: {other:?}").into());
            }
        }
    }

    // ── A support that fills nothing is a name on a poster. ─────────────
    // `head_alone`'s show and sender are set here rather than in its own
    // case: the reach and pairing gates sit ahead of the recipient check, so
    // this one fixture serves three different refusals.
    open_slot_event(pool, head_alone, city, "n5-alone-show").await?;
    advertise(head_alone, "gig.outreach").await?;
    let zero_key = IdempotencyKey::parse("n5-ask-zero").expect("valid key");
    match approve_ask(
        pool,
        org,
        head_alone,
        silent_support,
        city,
        &zero_key,
        now,
        None,
    )
    .await
    {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("reaches nobody"),
            "a zero-reach support was not named in the refusal: {sentence}"
        ),
        other => return Err(format!("a support reaching nobody was asked: {other:?}").into()),
    }

    // ── A city with no coordinates is unmeasurable, which is not zero. ──
    open_slot_event(pool, head_unmeasurable, unmeasurable, "n5-unmeasurable").await?;
    let unmeasurable_key = IdempotencyKey::parse("n5-ask-unmeasurable").expect("valid key");
    match approve_ask(
        pool,
        org,
        head_unmeasurable,
        support,
        unmeasurable,
        &unmeasurable_key,
        now,
        None,
    )
    .await
    {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("cannot measure"),
            "an unmeasurable city was not told apart from an empty one: {sentence}"
        ),
        other => return Err(format!("an unmeasurable ask was taken: {other:?}").into()),
    }

    // ── One crowd twice is a door split, not a bigger room. ─────────────
    // The overlap is measured against the *approving* headliner, so the echo
    // shares `head`'s fans — the first ask has already succeeded, which is
    // fine: nothing in flight is not nothing on the calendar.
    let echo_key = IdempotencyKey::parse("n5-ask-echo").expect("valid key");
    match approve_ask(pool, org, head, echo_support, city, &echo_key, now, None).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("already follow"),
            "a fully-overlapped support was not refused: {sentence}"
        ),
        other => {
            return Err(format!("an echo was put forward as a support: {other:?}").into());
        }
    }

    // ── Nobody to send it with refuses rather than parking. ─────────────
    open_slot_event(pool, head_silent, city, "n5-silent-show").await?;
    advertise(head_silent, "booking.outreach").await?;
    let silent_key = IdempotencyKey::parse("n5-ask-silent").expect("valid key");
    match approve_ask(
        pool,
        org,
        head_silent,
        support,
        city,
        &silent_key,
        now,
        None,
    )
    .await
    {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("nothing can send this letter yet"),
            "an unsendable ask did not name the missing sender: {sentence}"
        ),
        other => return Err(format!("an unsendable ask was queued: {other:?}").into()),
    }
    let silent_actions = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM viryaos_autopilot_actions WHERE workspace_id = $1",
    )
    .bind(head_silent)
    .fetch_one(pool)
    .await?;
    assert_eq!(silent_actions, 0, "an unsendable approval still queued");

    // ── Nobody to write it to is the same answer. ────────────────────────
    let alone_key = IdempotencyKey::parse("n5-ask-alone").expect("valid key");
    match approve_ask(pool, org, head_alone, support, city, &alone_key, now, None).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("no contactable promoter"),
            "a promoterless city was not refused honestly: {sentence}"
        ),
        other => return Err(format!("a letter with no recipients was queued: {other:?}").into()),
    }

    // ── An act cannot open for itself. ──────────────────────────────────
    let self_key = IdempotencyKey::parse("n5-ask-self").expect("valid key");
    match approve_ask(pool, org, head, head, city, &self_key, now, None).await {
        Err(GigOutreachError::Refused(sentence)) => assert!(
            sentence.contains("cannot open for itself"),
            "the self-ask refusal did not say why: {sentence}"
        ),
        other => return Err(format!("an act opened for itself: {other:?}").into()),
    }

    // ── A workspace the organisation does not own is not on the board. ──
    let foreign_key = IdempotencyKey::parse("n5-ask-foreign").expect("valid key");
    match approve_ask(
        pool,
        org,
        foreign_head,
        support,
        city,
        &foreign_key,
        now,
        None,
    )
    .await
    {
        Err(GigOutreachError::NotFound) => {}
        other => {
            return Err(format!("a foreign headliner was answered: {other:?}").into());
        }
    }
    match approve_ask(pool, org, head, foreign_head, city, &foreign_key, now, None).await {
        Err(GigOutreachError::NotFound) => {}
        other => {
            return Err(format!("a foreign support was named: {other:?}").into());
        }
    }

    // A different organisation's id is the same answer — membership is checked
    // against the id the caller gave, not inferred.
    let wrong_org_key = IdempotencyKey::parse("n5-ask-wrong-org").expect("valid key");
    match approve_ask(
        pool,
        foreign_org,
        head,
        support,
        city,
        &wrong_org_key,
        now,
        None,
    )
    .await
    {
        Err(GigOutreachError::NotFound) => {}
        other => {
            return Err(format!("the wrong organisation approved an ask: {other:?}").into());
        }
    }

    Ok(())
}
