//! The research gate and the preview, split out of `latarnik.rs` for the
//! source-size ratchet. The fixtures are that file's; these are the proofs that
//! nobody is written to unread, that the outreach engine's history counts, and
//! that what the operator previews is what a click would send.

use crate::common;
use crate::latarnik::{
    beacon, campaign_answer, city, contacted, published_show, replied, researched, settings,
    workspace,
};

use crowdrelay_application::IdempotencyKey;
use crowdrelay_infra::contact_research::{
    ResearchError, queue_contact_research, record_hook_for_beacon,
};
use crowdrelay_infra::latarnik::{
    InviteError, InviteOutcome, approve_latarnik_invite, dual_role_review, preview_latarnik_invite,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// The outreach engine's own row for an address: when it last mailed them and
/// how they answered. Production held 485 outbound mails a month here and 56
/// rows in the governor, so the invitation lane saw nobody.
async fn outreach_target(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    last_outreach_at: OffsetDateTime,
    disposition: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO outreach_targets
            (workspace_id, target_kind, display_name, contact_email,
             last_outreach_at, last_reply_disposition)
        VALUES ($1, 'press', $2, $3, $4, $5)
        "#,
    )
    .bind(workspace_id)
    .bind(email)
    .bind(email)
    .bind(last_outreach_at)
    .bind(disposition)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn people_the_outreach_engine_knows_are_not_cold() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let now = OffsetDateTime::now_utc();
    let act = workspace(&pool).await?;
    let city = city(&pool).await?;

    // Marta was mailed by the outreach engine forty days ago. The governor
    // never heard of it. She is a relationship, not a cold contact.
    beacon(
        &pool,
        act,
        city,
        "local_press",
        "Marta",
        "marta@example.test",
        72,
        true,
    )
    .await?;
    outreach_target(
        &pool,
        act,
        "marta@example.test",
        now - time::Duration::days(40),
        "none",
    )
    .await?;

    // Nikodem scores below the bar, but he answered positively: a reply
    // outranks a score.
    beacon(
        &pool,
        act,
        city,
        "local_press",
        "Nikodem",
        "nikodem@example.test",
        50,
        true,
    )
    .await?;
    outreach_target(
        &pool,
        act,
        "nikodem@example.test",
        now - time::Duration::days(30),
        "positive",
    )
    .await?;

    researched(
        &pool,
        act,
        "marta@example.test",
        now - time::Duration::days(5),
    )
    .await?;
    researched(
        &pool,
        act,
        "nikodem@example.test",
        now - time::Duration::days(5),
    )
    .await?;

    // Wiktor was mailed forty days ago and is a relationship, but unread.
    beacon(
        &pool,
        act,
        city,
        "local_press",
        "Wiktor",
        "wiktor@example.test",
        75,
        true,
    )
    .await?;
    outreach_target(
        &pool,
        act,
        "wiktor@example.test",
        now - time::Duration::days(40),
        "none",
    )
    .await?;

    // Olga was mailed five days ago. Business first.
    beacon(
        &pool,
        act,
        city,
        "promoter",
        "Olga",
        "olga@example.test",
        70,
        true,
    )
    .await?;
    outreach_target(
        &pool,
        act,
        "olga@example.test",
        now - time::Duration::days(5),
        "none",
    )
    .await?;

    // Piotr and Rafał said no to the pitch. Asking through the other role is
    // the oldest trick in mailing-list software.
    beacon(
        &pool,
        act,
        city,
        "local_press",
        "Piotr",
        "piotr@example.test",
        80,
        true,
    )
    .await?;
    outreach_target(
        &pool,
        act,
        "piotr@example.test",
        now - time::Duration::days(40),
        "declined",
    )
    .await?;
    beacon(
        &pool,
        act,
        city,
        "local_press",
        "Rafał",
        "rafal@example.test",
        80,
        true,
    )
    .await?;
    outreach_target(
        &pool,
        act,
        "rafal@example.test",
        now - time::Duration::days(40),
        "do_not_contact",
    )
    .await?;

    // Tola declined a beacon campaign. That used to count as "replied".
    let tola = beacon(
        &pool,
        act,
        city,
        "promoter",
        "Tola",
        "tola@example.test",
        80,
        true,
    )
    .await?;
    campaign_answer(&pool, act, tola, city, now, "declined").await?;
    contacted(
        &pool,
        act,
        "tola@example.test",
        "beacon_outreach",
        now - time::Duration::days(40),
    )
    .await?;

    // Stefan was never contacted in either ledger: still cold.
    beacon(
        &pool,
        act,
        city,
        "promoter",
        "Stefan",
        "stefan@example.test",
        90,
        true,
    )
    .await?;

    let review = dual_role_review(&pool, act, now, true).await?;
    let by_name = |name: &str| {
        review
            .contacts
            .iter()
            .find(|contact| contact.display_name == name)
            .unwrap_or_else(|| panic!("{name} missing from the review"))
    };

    let marta = by_name("Marta");
    assert!(marta.invitable, "Marta: {:?}", marta.hold_reason);
    assert_eq!(marta.days_since_last_contact, Some(40));

    let nikodem = by_name("Nikodem");
    assert!(nikodem.invitable, "Nikodem: {:?}", nikodem.hold_reason);
    assert!(nikodem.has_replied);

    let olga = by_name("Olga");
    assert!(!olga.invitable);
    assert!(
        olga.hold_reason
            .as_deref()
            .unwrap_or_default()
            .contains("recently"),
        "{:?}",
        olga.hold_reason
    );

    for name in ["Piotr", "Rafał", "Tola"] {
        let row = by_name(name);
        assert!(
            !row.invitable,
            "{name} said no and was offered an invitation"
        );
        assert!(row.do_not_contact, "{name} must read as refused");
    }

    let wiktor = by_name("Wiktor");
    assert!(
        !wiktor.invitable,
        "an unread contact was offered an invitation"
    );
    assert!(
        wiktor
            .hold_reason
            .as_deref()
            .unwrap_or_default()
            .contains("not read yet"),
        "{:?}",
        wiktor.hold_reason
    );

    let stefan = by_name("Stefan");
    assert!(!stefan.invitable, "a contact nobody ever wrote to is cold");

    assert_eq!(review.invitable_now, 2, "Marta and Nikodem, today");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_preview_is_the_letter_the_click_would_send() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let now = OffsetDateTime::now_utc();
    let act = workspace(&pool).await?;
    let city = city(&pool).await?;
    settings(&pool, act, "member_site_base_url", "https://virya.music").await?;
    settings(&pool, act, "act_style", "modern metal").await?;

    let anna = beacon(
        &pool,
        act,
        city,
        "promoter",
        "Anna",
        "anna@example.test",
        72,
        true,
    )
    .await?;
    replied(&pool, act, anna, city, now).await?;
    contacted(
        &pool,
        act,
        "anna@example.test",
        "gig_outreach",
        now - time::Duration::days(40),
    )
    .await?;
    published_show(&pool, act, city, now + time::Duration::days(45)).await?;
    researched(
        &pool,
        act,
        "anna@example.test",
        now - time::Duration::days(10),
    )
    .await?;

    // Dawid was never written to: a click would refuse him, so the preview must.
    let dawid = beacon(
        &pool,
        act,
        city,
        "promoter",
        "Dawid",
        "dawid@example.test",
        95,
        true,
    )
    .await?;

    let preview = preview_latarnik_invite(&pool, act, anna, now).await?;
    assert_eq!(preview.recipient_email, "anna@example.test");
    assert_eq!(preview.recipient_name, "Anna");
    assert!(preview.body.starts_with("Cześć Anna,"), "{}", preview.body);
    // The operator can check what the letter claims to have read.
    assert_eq!(
        preview.hook_fact,
        "recenzja płyty „Szum” w audycji „Metalowy Wieczór”"
    );
    assert_eq!(
        preview.hook_source_url,
        "https://example.test/anna@example.test"
    );

    // Reading it queued nothing and wrote no ask: Anna is still invitable.
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM autopilot_actions WHERE workspace_id = $1")
            .bind(act)
            .fetch_one(&pool)
            .await?;
    assert_eq!(queued, 0, "a preview queued an action");
    let again = preview_latarnik_invite(&pool, act, anna, now).await?;
    assert_eq!(again.body, preview.body, "a preview is not repeatable");

    // The word-for-word promise: what was shown is what the click queues.
    let key = IdempotencyKey::parse("latarnik-preview-anna").expect("valid key");
    let InviteOutcome::Queued { action_id, .. } =
        approve_latarnik_invite(&pool, act, anna, &key, now).await?
    else {
        return Err("expected a queued invitation".into());
    };
    let payload = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT payload FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        payload["draft"]["subject"].as_str(),
        Some(preview.subject.as_str())
    );
    assert_eq!(
        payload["draft"]["body"].as_str(),
        Some(preview.body.as_str())
    );
    assert_eq!(payload["reason"].as_str(), Some(preview.reason.as_str()));

    // Every refusal a click gives, the preview gives: cold, and already asked.
    match preview_latarnik_invite(&pool, act, dawid, now).await {
        Err(InviteError::Refused(sentence)) => {
            assert!(sentence.contains("no relationship"), "{sentence}");
        }
        other => return Err(format!("a cold contact was previewed: {other:?}").into()),
    }
    match preview_latarnik_invite(&pool, act, anna, now).await {
        Err(InviteError::Refused(_)) => {}
        other => return Err(format!("an already-asked contact was previewed: {other:?}").into()),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn nobody_is_written_to_unread() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    let now = OffsetDateTime::now_utc();
    let today = now.date();
    let act = workspace(&pool).await?;
    let city = city(&pool).await?;
    settings(&pool, act, "member_site_base_url", "https://virya.music").await?;

    let anna = beacon(
        &pool,
        act,
        city,
        "promoter",
        "Anna",
        "anna@example.test",
        72,
        true,
    )
    .await?;
    replied(&pool, act, anna, city, now).await?;
    contacted(
        &pool,
        act,
        "anna@example.test",
        "gig_outreach",
        now - time::Duration::days(40),
    )
    .await?;
    published_show(&pool, act, city, now + time::Duration::days(45)).await?;

    fn refused_with<T: std::fmt::Debug>(result: Result<T, InviteError>, needle: &str) {
        match result {
            Err(InviteError::Refused(sentence)) => assert!(sentence.contains(needle), "{sentence}"),
            other => panic!("expected a refusal containing {needle:?}, got {other:?}"),
        }
    }

    // 1. Warm and askable in every other way, but unread: held, in the preview
    //    and in the approval alike.
    refused_with(
        preview_latarnik_invite(&pool, act, anna, now).await,
        "not read yet",
    );
    let key = IdempotencyKey::parse("latarnik-unread").expect("valid key");
    refused_with(
        approve_latarnik_invite(&pool, act, anna, &key, now).await,
        "not read yet",
    );

    // 2. A fact that is not the band's voice, has no real source, or is not
    //    recent is refused by the writer, not stored.
    let record =
        |fact: &'static str, praise: Option<&'static str>, source: &'static str, days: i64| {
            let pool = pool.clone();
            async move {
                record_hook_for_beacon(
                    &pool,
                    act,
                    anna,
                    fact,
                    praise,
                    source,
                    (now - time::Duration::days(days)).date(),
                    "pl",
                    "operator",
                    today,
                )
                .await
            }
        };
    let fact = "recenzja płyty „Szum” w audycji „Metalowy Wieczór”";
    for (result, needle) in [
        (
            record(fact, Some("Świetna robota!"), "https://example.test/a", 5).await,
            "exclamation",
        ),
        (
            record(fact, None, "http://example.test/a", 5).await,
            "https address",
        ),
        (
            record(fact, None, "https://example.test/a", 400).await,
            "not 'lately'",
        ),
        (
            record("nowy odcinek", None, "https://example.test/a", 5).await,
            "too short",
        ),
    ] {
        match result {
            Err(ResearchError::Refused(sentence)) => {
                assert!(sentence.contains(needle), "{sentence}")
            }
            other => panic!("expected a refusal containing {needle:?}, got {other:?}"),
        }
    }
    refused_with(
        preview_latarnik_invite(&pool, act, anna, now).await,
        "not read yet",
    );

    // 3. A row that went stale on file does not count either: "lately" is
    //    decided at read time, not only when it was written.
    researched(
        &pool,
        act,
        "anna@example.test",
        now - time::Duration::days(200),
    )
    .await?;
    refused_with(
        preview_latarnik_invite(&pool, act, anna, now).await,
        "not read yet",
    );

    // 4. A recent, sourced, in-voice fact opens the door, and is what the
    //    letter says.
    record_hook_for_beacon(
        &pool,
        act,
        anna,
        fact,
        Some("Rzadko ktoś omawia tę płytę tak konkretnie."),
        "https://example.test/audycje/metalowy-wieczor",
        (now - time::Duration::days(12)).date(),
        "pl",
        "agent:contact-researcher",
        today,
    )
    .await?;
    let preview = preview_latarnik_invite(&pool, act, anna, now).await?;
    assert!(preview.body.contains(fact), "{}", preview.body);
    assert!(
        preview.body.contains("Rzadko ktoś omawia"),
        "{}",
        preview.body
    );
    assert!(
        !preview.body.contains("example.test"),
        "the source leaked into the letter"
    );
    assert_eq!(
        preview.hook_source_url,
        "https://example.test/audycje/metalowy-wieczor"
    );

    // Recording the same source again is idempotent, not a second row.
    record_hook_for_beacon(
        &pool,
        act,
        anna,
        fact,
        None,
        "https://example.test/audycje/metalowy-wieczor",
        (now - time::Duration::days(12)).date(),
        "pl",
        "operator",
        today,
    )
    .await?;
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM contact_research WHERE workspace_id = $1 AND source_url = $2",
    )
    .bind(act)
    .bind("https://example.test/audycje/metalowy-wieczor")
    .fetch_one(&pool)
    .await?;
    assert_eq!(rows, 1);
    Ok(())
}

/// `agent_service_tasks` belongs to the agents service; no CrowdRelay migration
/// creates it, so the suite database needs the columns the queue reads.
async fn create_foreign_task_table(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS agent_service_tasks (
            id uuid PRIMARY KEY,
            workspace_id uuid NOT NULL,
            template_id text NOT NULL,
            model_id text NOT NULL,
            prompt text NOT NULL,
            status text NOT NULL DEFAULT 'queued',
            tier text NOT NULL DEFAULT 'basic',
            metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
            created_at timestamptz NOT NULL DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn only_people_worth_reading_are_sent_to_the_research_agent()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");
    create_foreign_task_table(&pool).await?;
    let now = OffsetDateTime::now_utc();
    let act = workspace(&pool).await?;
    let city = city(&pool).await?;

    // Warm, askable, unread: exactly who research is for.
    let anna = beacon(
        &pool,
        act,
        city,
        "promoter",
        "Anna",
        "anna@example.test",
        72,
        true,
    )
    .await?;
    replied(&pool, act, anna, city, now).await?;
    contacted(
        &pool,
        act,
        "anna@example.test",
        "gig_outreach",
        now - time::Duration::days(40),
    )
    .await?;
    // Never written to: held as cold, and research would be wasted on someone who cannot be asked.
    let dawid = beacon(
        &pool,
        act,
        city,
        "promoter",
        "Dawid",
        "dawid@example.test",
        95,
        true,
    )
    .await?;
    // Written to last week: held as too soon.
    let ewa = beacon(
        &pool,
        act,
        city,
        "promoter",
        "Ewa",
        "ewa@example.test",
        75,
        true,
    )
    .await?;
    replied(&pool, act, ewa, city, now).await?;
    contacted(
        &pool,
        act,
        "ewa@example.test",
        "gig_outreach",
        now - time::Duration::days(4),
    )
    .await?;
    // Already read.
    let bogdan = beacon(
        &pool,
        act,
        city,
        "local_press",
        "Bogdan",
        "bogdan@example.test",
        80,
        true,
    )
    .await?;
    replied(&pool, act, bogdan, city, now).await?;
    contacted(
        &pool,
        act,
        "bogdan@example.test",
        "gig_outreach",
        now - time::Duration::days(40),
    )
    .await?;
    researched(
        &pool,
        act,
        "bogdan@example.test",
        now - time::Duration::days(5),
    )
    .await?;

    fn refused_with<T: std::fmt::Debug>(result: Result<T, ResearchError>, needle: &str) {
        match result {
            Err(ResearchError::Refused(sentence)) => {
                assert!(sentence.contains(needle), "{sentence}")
            }
            other => panic!("expected a refusal containing {needle:?}, got {other:?}"),
        }
    }
    refused_with(
        queue_contact_research(&pool, act, dawid, now).await,
        "no relationship",
    );
    refused_with(
        queue_contact_research(&pool, act, ewa, now).await,
        "recently",
    );
    refused_with(
        queue_contact_research(&pool, act, bogdan, now).await,
        "fact is on file",
    );

    // Anna is queued, pinned in metadata, as a premium read-only research task.
    let task_id = queue_contact_research(&pool, act, anna, now).await?;
    let (template, tier, status, metadata, prompt) = sqlx::query_as::<
        _,
        (String, String, String, serde_json::Value, String),
    >(
        "SELECT template_id, tier, status, metadata, prompt FROM agent_service_tasks WHERE id = $1",
    )
    .bind(task_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(template, "contact-researcher");
    assert_eq!(tier, "premium");
    assert_eq!(status, "queued");
    assert_eq!(
        metadata["subject_beacon_id"].as_str(),
        Some(anna.to_string().as_str())
    );
    assert!(
        prompt.contains(&anna.to_string()),
        "the item must be able to carry the id: {prompt}"
    );
    assert!(
        !prompt.contains("anna@example.test"),
        "the address is not the agent's business"
    );

    // Asking again inside the week is refused; a failed attempt may be retried.
    refused_with(
        queue_contact_research(&pool, act, anna, now).await,
        "last 7 days",
    );
    sqlx::query("UPDATE agent_service_tasks SET status = 'failed' WHERE id = $1")
        .bind(task_id)
        .execute(&pool)
        .await?;
    queue_contact_research(&pool, act, anna, now).await?;

    // It contacted nobody: no action, no outbox event.
    let actions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM autopilot_actions WHERE workspace_id = $1")
            .bind(act)
            .fetch_one(&pool)
            .await?;
    assert_eq!(actions, 0);
    Ok(())
}
