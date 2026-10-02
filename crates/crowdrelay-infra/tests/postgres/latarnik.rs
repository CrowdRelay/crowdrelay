//! P.1 — one person, two roles.
//!
//! The join is the point and only a database can show it: a promoter and a fan
//! are two tables, and until now nothing said they were the same human. The
//! rules that decide who may be asked live in the domain and are unit-tested
//! there; what is tested here is which rows the read admits, what it counts,
//! and — the part that matters most — that somebody who opted out is never
//! reported as reachable again.

use crate::common;

use crowdrelay_application::IdempotencyKey;
use crowdrelay_infra::latarnik::{
    InviteError, InviteOutcome, approve_latarnik_invite, dual_role_review,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_industry_list_is_also_an_audience() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run(&database).await
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool).await?;
    let city = city(pool).await?;

    // Anna books the room, replied to the band once, and was last written to
    // forty days ago. The person this whole feature exists for.
    let anna = beacon(
        pool,
        act,
        city,
        "promoter",
        "Anna",
        "anna@example.test",
        72,
        true,
    )
    .await?;
    replied(pool, act, anna, city, now).await?;
    contacted(
        pool,
        act,
        "anna@example.test",
        "gig_outreach",
        now - time::Duration::days(40),
    )
    .await?;

    researched(
        pool,
        act,
        "anna@example.test",
        now - time::Duration::days(10),
    )
    .await?;

    // Filip is warm in every way the relationship rules ask: he replied and was
    // last written to fifty days ago. But nobody has looked at what he did
    // lately, and the band does not write to people it has not read.
    let filip = beacon(
        pool,
        act,
        city,
        "promoter",
        "Filip",
        "filip@example.test",
        80,
        true,
    )
    .await?;
    replied(pool, act, filip, city, now).await?;
    contacted(
        pool,
        act,
        "filip@example.test",
        "gig_outreach",
        now - time::Duration::days(50),
    )
    .await?;

    // Bogdan writes about records and already gets the dates — he is in, and
    // the read must say so rather than offering to invite him again.
    let bogdan = beacon(
        pool,
        act,
        city,
        "local_press",
        "Bogdan",
        "bogdan@example.test",
        80,
        true,
    )
    .await?;
    replied(pool, act, bogdan, city, now).await?;
    contacted(
        pool,
        act,
        "bogdan@example.test",
        "beacon_outreach",
        now - time::Duration::days(60),
    )
    .await?;
    consented_fan(pool, act, "bogdan@example.test", true).await?;

    // Celina signed up once and unsubscribed. The address is known and she is
    // NOT reachable — the failure this test exists to prevent.
    let celina = beacon(
        pool,
        act,
        city,
        "photographer",
        "Celina",
        "celina@example.test",
        70,
        true,
    )
    .await?;
    replied(pool, act, celina, city, now).await?;
    contacted(
        pool,
        act,
        "celina@example.test",
        "beacon_outreach",
        now - time::Duration::days(90),
    )
    .await?;
    consented_fan(pool, act, "celina@example.test", false).await?;

    // Dawid came off a directory sweep: a decent score, never contacted, never
    // replied. Cold, and cold is never invited.
    beacon(
        pool,
        act,
        city,
        "promoter",
        "Dawid",
        "dawid@example.test",
        95,
        true,
    )
    .await?;

    // Ewa was written to four days ago about a date. Business first.
    let ewa = beacon(
        pool,
        act,
        city,
        "promoter",
        "Ewa",
        "ewa@example.test",
        75,
        true,
    )
    .await?;
    replied(pool, act, ewa, city, now).await?;
    contacted(
        pool,
        act,
        "ewa@example.test",
        "gig_outreach",
        now - time::Duration::days(4),
    )
    .await?;

    let review = dual_role_review(pool, act, now, true).await?;
    assert_eq!(
        review.total, 6,
        "every active contactable beacon is reviewed"
    );
    assert_eq!(
        review.already_hear_the_dates, 1,
        "only Bogdan has live consent"
    );

    let by_name = |name: &str| {
        review
            .contacts
            .iter()
            .find(|contact| contact.display_name == name)
            .unwrap_or_else(|| panic!("{name} missing from the review"))
    };

    let anna_row = by_name("Anna");
    assert!(anna_row.invitable, "Anna: {:?}", anna_row.hold_reason);
    assert!(!anna_row.hears_the_dates);
    assert_eq!(anna_row.days_since_last_contact, Some(40));

    let bogdan_row = by_name("Bogdan");
    assert!(bogdan_row.hears_the_dates);
    assert!(!bogdan_row.invitable);
    assert!(
        bogdan_row
            .hold_reason
            .as_deref()
            .unwrap_or_default()
            .contains("already get the dates"),
        "{:?}",
        bogdan_row.hold_reason
    );

    // The one that matters: an opt-out is known, not reachable, and not
    // silently re-subscribed by being offered the invitation again.
    let celina_row = by_name("Celina");
    assert!(!celina_row.hears_the_dates, "an opt-out is not reachable");
    assert!(celina_row.known_but_not_consented, "the address is known");

    let dawid_row = by_name("Dawid");
    assert!(
        !dawid_row.invitable,
        "a cold contact was offered an invitation"
    );
    assert!(
        dawid_row
            .hold_reason
            .as_deref()
            .unwrap_or_default()
            .contains("no relationship"),
        "{:?}",
        dawid_row.hold_reason
    );

    let ewa_row = by_name("Ewa");
    assert!(!ewa_row.invitable);
    assert!(
        ewa_row
            .hold_reason
            .as_deref()
            .unwrap_or_default()
            .contains("recently"),
        "{:?}",
        ewa_row.hold_reason
    );

    let filip_row = by_name("Filip");
    assert!(
        !filip_row.invitable,
        "somebody unread was offered an invitation"
    );
    assert!(!filip_row.has_research);
    assert!(
        filip_row
            .hold_reason
            .as_deref()
            .unwrap_or_default()
            .contains("not read yet"),
        "{:?}",
        filip_row.hold_reason
    );
    assert!(by_name("Anna").has_research);

    assert_eq!(review.invitable_now, 1, "Anna alone, today");

    // A band with nothing on the calendar has nothing to say, and the read says
    // that rather than offering letters with no reason in them.
    let nothing_on = dual_role_review(pool, act, now, false).await?;
    assert_eq!(nothing_on.invitable_now, 0);
    assert!(
        nothing_on.contacts.iter().any(|contact| contact
            .hold_reason
            .as_deref()
            .unwrap_or_default()
            .contains("nothing concrete")),
        "no row explained that there is nothing to tell them"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_invitation_carries_its_letter_and_refuses_the_rest()
-> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_send(&database).await
}

async fn run_send(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let now = OffsetDateTime::now_utc();
    let act = workspace(pool).await?;
    let city = city(pool).await?;
    settings(pool, act, "member_site_base_url", "https://virya.music").await?;
    settings(pool, act, "act_style", "modern metal").await?;

    let anna = beacon(
        pool,
        act,
        city,
        "promoter",
        "Anna",
        "anna@example.test",
        72,
        true,
    )
    .await?;
    replied(pool, act, anna, city, now).await?;
    contacted(
        pool,
        act,
        "anna@example.test",
        "gig_outreach",
        now - time::Duration::days(40),
    )
    .await?;

    researched(
        pool,
        act,
        "anna@example.test",
        now - time::Duration::days(10),
    )
    .await?;

    // A published night in Anna's city is the reason the letter opens with.
    published_show(pool, act, city, now + time::Duration::days(45)).await?;

    let key = IdempotencyKey::parse("latarnik-anna-1").expect("valid key");
    let outcome = approve_latarnik_invite(pool, act, anna, &key, now).await?;
    let action_id = match outcome {
        InviteOutcome::Queued {
            action_id,
            recipient,
            subject,
        } => {
            assert_eq!(recipient, "anna@example.test");
            assert!(subject.contains("Virya"), "{subject}");
            action_id
        }
        other => return Err(format!("expected a queued invitation, got {other:?}").into()),
    };

    let (kind, status, payload) = sqlx::query_as::<_, (String, String, serde_json::Value)>(
        "SELECT action_kind, status, payload FROM autopilot_actions
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(kind, "latarnik.invite.request");
    assert_eq!(status, "queued");

    // The letter is in the payload, whole, and it opens with her fact rather
    // than with the band's. That is the difference between this and a blast.
    let body = payload["draft"]["body"].as_str().unwrap_or_default();
    assert!(body.starts_with("Cześć Anna,"), "{body}");
    // What the band read comes first, before anything it wants from her, and
    // the source stays out of the text.
    let read = body.find("recenzja płyty „Szum” w audycji „Metalowy Wieczór”");
    let ask = body.find("Gramy w Wrocław");
    assert!(
        read.is_some() && read < ask,
        "the letter does not open with her work: {body}"
    );
    assert!(!body.contains("example.test"), "{body}");
    assert!(
        body.contains("Gramy w Wrocław"),
        "the letter does not open with her city: {body}"
    );
    // Since #426 every outward link in a letter is a tracked one, so the
    // invitation points at the redirect that lands on the latarnik page.
    assert!(body.contains("https://virya.music/l/latarnik"), "{body}");
    assert!(
        !body.contains('!'),
        "an exclamation mark reached a working promoter: {body}"
    );
    assert!(
        payload["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("koncert w Wrocław"),
        "the ledger did not record why she was written to: {payload}"
    );

    // O.2 applies here too: an outward send waits before a worker may claim it.
    let available_at = sqlx::query_scalar::<_, OffsetDateTime>(
        "SELECT available_at FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(act)
    .bind(action_id)
    .fetch_one(pool)
    .await?;
    assert!(
        available_at > now,
        "the invitation was claimable the instant it was approved"
    );

    // The same click twice is the same invitation.
    match approve_latarnik_invite(pool, act, anna, &key, now).await? {
        InviteOutcome::Replayed {
            action_id: replayed,
            ..
        } => assert_eq!(replayed, action_id),
        other => return Err(format!("expected a replay, got {other:?}").into()),
    }

    // A second key does not buy a second ask: the governor row now records
    // `latarnik_invite` as her last contact, and once-ever refuses.
    let second = IdempotencyKey::parse("latarnik-anna-2").expect("valid key");
    match approve_latarnik_invite(pool, act, anna, &second, now).await {
        Err(InviteError::Refused(sentence)) => assert!(
            sentence.contains("already queued") || sentence.contains("asked once"),
            "the second ask was refused for the wrong reason: {sentence}"
        ),
        other => return Err(format!("a second invitation was taken: {other:?}").into()),
    }

    // Somebody who left the list is refused by name, whatever else is true.
    let celina = beacon(
        pool,
        act,
        city,
        "photographer",
        "Celina",
        "celina@example.test",
        90,
        true,
    )
    .await?;
    replied(pool, act, celina, city, now).await?;
    contacted(
        pool,
        act,
        "celina@example.test",
        "beacon_outreach",
        now - time::Duration::days(200),
    )
    .await?;
    consented_fan(pool, act, "celina@example.test", false).await?;
    let key = IdempotencyKey::parse("latarnik-celina").expect("valid key");
    match approve_latarnik_invite(pool, act, celina, &key, now).await {
        Err(InviteError::Refused(sentence)) => assert!(
            sentence.contains("unsubscribed"),
            "an opt-out was refused for the wrong reason: {sentence}"
        ),
        other => return Err(format!("an unsubscribed contact was written to: {other:?}").into()),
    }
    Ok(())
}

pub(crate) async fn settings(
    pool: &PgPool,
    workspace_id: Uuid,
    key: &str,
    value: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn published_show(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    starts_at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO events
            (workspace_id, slug, title, status, starts_at, timezone, city_id, published_at)
        VALUES ($1, $2, 'Koncert', 'published', $3, 'Europe/Warsaw', $4, now())
        "#,
    )
    .bind(workspace_id)
    .bind(format!("gig-{}", Uuid::now_v7().simple()))
    .bind(starts_at)
    .bind(city_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The band has read this person's recent work: one dated, sourced fact.
pub(crate) async fn researched(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    observed: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO contact_research
            (workspace_id, normalized_email, fact, praise, source_url, observed_on,
             researched_by)
        VALUES ($1, $2, 'recenzja płyty „Szum” w audycji „Metalowy Wieczór”',
                'W recenzji „Szum” zwróciło nam uwagę, że weszliście w aranżację, a nie tylko brzmienie.', $3, $4, 'test')
        "#,
    )
    .bind(workspace_id)
    .bind(email)
    .bind(format!("https://example.test/{email}"))
    .bind(observed.date())
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Virya')")
        .bind(id)
        .bind(format!("latarnik-{}", id.simple()))
        .execute(pool)
        .await?;
    Ok(id)
}

pub(crate) async fn city(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let suffix = Uuid::now_v7().simple().to_string();
    Ok(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ($1, $2, 'PL', 51.1, 17.0) RETURNING id",
    )
    .bind(format!("wroclaw-{}", suffix))
    // A unique name, not a bare `Wrocław`. These proofs share one database
    // with the rest of the e2e suite in CI, and the city resolver refuses an
    // ambiguous name rather than guess: three more `Wrocław` rows made an
    // unrelated proof (`a_band_sheet_lands_as_attributed_peer_facts`) count
    // two unresolved cities instead of one. The letter assertions use
    // `contains("… Wrocław")`, which the suffix does not disturb.
    .bind(format!("Wrocław {suffix}"))
    .fetch_one(pool)
    .await?)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn beacon(
    pool: &PgPool,
    workspace_id: Uuid,
    city_id: Uuid,
    kind: &str,
    name: &str,
    email: &str,
    score: i32,
    accepts: bool,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO beacons
            (workspace_id, city_id, beacon_kind, display_name, contact_email,
             active, verified, accepts_outreach, relationship_score)
        VALUES ($1, $2, $3, $4, $5, true, true, $6, $7)
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(city_id)
    .bind(kind)
    .bind(name)
    .bind(email)
    .bind(accepts)
    .bind(score)
    .fetch_one(pool)
    .await?)
}

/// A beacon campaign that got an answer — the reply that outranks a score.
pub(crate) async fn replied(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    city_id: Uuid,
    now: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    campaign_answer(pool, workspace_id, beacon_id, city_id, now, "received").await
}

/// A beacon campaign whose answer was `disposition`.
pub(crate) async fn campaign_answer(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    city_id: Uuid,
    now: OffsetDateTime,
    disposition: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let event_id = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO events
            (workspace_id, slug, title, status, starts_at, timezone, city_id, published_at)
        VALUES ($1, $2, 'Test night', 'published', $3, 'Europe/Warsaw', $4, now())
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(format!("night-{}", Uuid::now_v7().simple()))
    .bind(now + time::Duration::days(30))
    .bind(city_id)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO beacon_campaigns
            (workspace_id, beacon_id, event_id, status, last_reply_disposition)
        VALUES ($1, $2, $3, 'contacted', $4)
        "#,
    )
    .bind(workspace_id)
    .bind(beacon_id)
    .bind(event_id)
    .bind(disposition)
    .execute(pool)
    .await?;
    Ok(())
}

/// The governor row: the band reached this address, in whatever role.
pub(crate) async fn contacted(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    context: &str,
    at: OffsetDateTime,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        r#"
        INSERT INTO contact_governor
            (workspace_id, normalized_contact, last_context, last_outbound_at, next_contact_after)
        VALUES ($1, $2, $3, $4, $4)
        "#,
    )
    .bind(workspace_id)
    .bind(email)
    .bind(context)
    .bind(at)
    .execute(pool)
    .await?;
    Ok(())
}

/// A fan row with the newest marketing consent granted or withdrawn.
async fn consented_fan(
    pool: &PgPool,
    workspace_id: Uuid,
    email: &str,
    granted: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let fan_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO fans (workspace_id, normalized_email, status)
         VALUES ($1, $2, 'active') RETURNING id",
    )
    .bind(workspace_id)
    .bind(email)
    .fetch_one(pool)
    .await?;
    // An opt-out is recorded as a newer row, never as a deletion: the read has
    // to take the newest record, not any record.
    sqlx::query(
        r#"
        INSERT INTO fan_consents
            (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
        VALUES ($1, $2, 'marketing', true, 'privacy-v1', 'test', now() - INTERVAL '10 days')
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .execute(pool)
    .await?;
    if !granted {
        sqlx::query(
            r#"
            INSERT INTO fan_consents
                (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at)
            VALUES ($1, $2, 'marketing', false, 'privacy-v1', 'test', now())
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}
