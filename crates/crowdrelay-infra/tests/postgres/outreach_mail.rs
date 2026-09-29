//! The outreach ledger kept by the mailbox, against a real schema.
//!
//! What only real rows prove: a message to or from an outreach contact lands
//! once per contact, with the right direction and phase; the contact's clocks
//! move forward only; an answer marks the contact served without overwriting
//! a verdict someone already recorded; a second read of the same message
//! writes nothing; and another workspace's contact with the same address is
//! never touched.

use crate::common;
use crowdrelay_infra::outreach_mail::{
    MailDirection, MailTouch, message_recorded, pitched_reply_targets, record_mail_touch,
    record_reply_text, unbodied_replies,
};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

async fn workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, 'Mail ledger')")
        .bind(id)
        .bind(format!("mail-{}", id.simple()))
        .execute(pool)
        .await?;
    Ok(id)
}

async fn target(
    pool: &PgPool,
    workspace: Uuid,
    email: &str,
    disposition: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outreach_targets (id, workspace_id, target_kind, display_name, contact_email, last_reply_disposition)
         VALUES ($1, $2, 'press', $3, $4, $5)",
    )
    .bind(id)
    .bind(workspace)
    .bind(email)
    .bind(email)
    .bind(disposition)
    .execute(pool)
    .await?;
    Ok(id)
}

fn touch(id: &str, direction: MailDirection, to: &[&str], at: OffsetDateTime) -> MailTouch {
    MailTouch {
        message_id: id.to_owned(),
        direction,
        counterparts: to.iter().map(|s| (*s).to_owned()).collect(),
        at,
    }
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_mailbox_keeps_the_outreach_ledger() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let other = workspace(&pool).await?;
    let zine = target(&pool, ws, "editor@zine.pl", "none").await?;
    let radio = target(&pool, ws, "radio@station.fm", "positive").await?;
    let foreign = target(&pool, other, "editor@zine.pl", "none").await?;
    let now = OffsetDateTime::now_utc();

    // The act writes to both: the zine's first message, the radio's too.
    let sent = record_mail_touch(
        &pool,
        ws,
        &touch(
            "m1",
            MailDirection::Outbound,
            &["editor@zine.pl", "radio@station.fm", "stranger@x.com"],
            now - Duration::days(10),
        ),
    )
    .await?;
    assert_eq!(sent, 2, "two contacts touched; a stranger is not a contact");
    assert!(message_recorded(&pool, ws, "m1").await?);
    // Read twice: nothing new.
    assert_eq!(
        record_mail_touch(
            &pool,
            ws,
            &touch(
                "m1",
                MailDirection::Outbound,
                &["editor@zine.pl"],
                now - Duration::days(10)
            )
        )
        .await?,
        0
    );

    // A later follow-up to the zine is phased as one.
    record_mail_touch(
        &pool,
        ws,
        &touch(
            "m2",
            MailDirection::Outbound,
            &["editor@zine.pl"],
            now - Duration::days(3),
        ),
    )
    .await?;
    let phases: Vec<String> = sqlx::query_scalar(
        "SELECT phase FROM outreach_interactions WHERE workspace_id = $1 AND target_id = $2 ORDER BY occurred_at",
    )
    .bind(ws)
    .bind(zine)
    .fetch_all(&pool)
    .await?;
    assert_eq!(phases, ["initial", "followup"]);

    // An old message read late does not move the clock back.
    record_mail_touch(
        &pool,
        ws,
        &touch(
            "m0",
            MailDirection::Outbound,
            &["editor@zine.pl"],
            now - Duration::days(40),
        ),
    )
    .await?;
    let last: OffsetDateTime =
        sqlx::query_scalar("SELECT last_outreach_at FROM outreach_targets WHERE id = $1")
            .bind(zine)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        last.unix_timestamp(),
        (now - Duration::days(3)).unix_timestamp()
    );

    // Their answers: the zine is served, the radio keeps its positive verdict.
    record_mail_touch(
        &pool,
        ws,
        &touch(
            "m3",
            MailDirection::Inbound,
            &["editor@zine.pl"],
            now - Duration::days(1),
        ),
    )
    .await?;
    record_mail_touch(
        &pool,
        ws,
        &touch(
            "m4",
            MailDirection::Inbound,
            &["radio@station.fm"],
            now - Duration::days(1),
        ),
    )
    .await?;
    let verdicts: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, last_reply_disposition::text FROM outreach_targets WHERE workspace_id = $1 ORDER BY id",
    )
    .bind(ws)
    .fetch_all(&pool)
    .await?;
    for (id, verdict) in verdicts {
        if id == zine {
            assert_eq!(verdict, "received");
        } else if id == radio {
            assert_eq!(
                verdict, "positive",
                "a recorded verdict is not overwritten by a receipt"
            );
        }
    }
    let inbound: (String, String) = sqlx::query_as(
        "SELECT phase, disposition FROM outreach_interactions WHERE workspace_id = $1 AND source_key = 'gmail:m3'",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await?;
    assert_eq!(inbound, ("reply".to_owned(), "received".to_owned()));

    // Every touch left an audit row with the new version.
    let history: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outreach_target_history WHERE workspace_id = $1 AND target_id = $2",
    )
    .bind(ws)
    .bind(zine)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        history, 4,
        "m1, m2, m0 and m3 each moved the zine's row once"
    );

    // The other workspace's contact with the same address saw nothing.
    let foreign_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outreach_interactions WHERE target_id = $1")
            .bind(foreign)
            .fetch_one(&pool)
            .await?;
    assert_eq!(foreign_rows, 0);
    Ok(())
}

/// The reply-body capture gate and write, on real rows: only a contact we
/// actually mailed in the last 60 days answers `pitched_reply_targets`; the
/// body then lands on the inbound interaction once — a second read of the
/// same message writes nothing and queues no second classification.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn only_a_pitched_contacts_reply_earns_a_body() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool).await?;
    let now = OffsetDateTime::now_utc();
    let pitched = target(&pool, ws, "curator@channel.tg", "none").await?;
    let stale = target(&pool, ws, "old@zine.pl", "none").await?;
    let never = target(&pool, ws, "fan@fanmail.pl", "none").await?;

    // Pitched recently, pitched long ago, never pitched.
    record_mail_touch(
        &pool,
        ws,
        &touch(
            "p1",
            MailDirection::Outbound,
            &["curator@channel.tg"],
            now - Duration::days(5),
        ),
    )
    .await?;
    record_mail_touch(
        &pool,
        ws,
        &touch(
            "p2",
            MailDirection::Outbound,
            &["old@zine.pl"],
            now - Duration::days(90),
        ),
    )
    .await?;
    // A reply exists for `never` too — inbound alone does not make a pitch.
    record_mail_touch(
        &pool,
        ws,
        &touch(
            "p3",
            MailDirection::Inbound,
            &["fan@fanmail.pl"],
            now - Duration::days(1),
        ),
    )
    .await?;

    let candidates = pitched_reply_targets(
        &pool,
        ws,
        &[
            "curator@channel.tg".to_owned(),
            "old@zine.pl".to_owned(),
            "fan@fanmail.pl".to_owned(),
        ],
        60,
    )
    .await?;
    let ids: Vec<Uuid> = candidates.iter().map(|c| c.target_id).collect();
    assert_eq!(ids, [pitched], "only the recently-pitched target qualifies");
    assert!(!ids.contains(&stale) && !ids.contains(&never));

    // The reply lands: the interaction carries reply_text and one 'auto'
    // classification row is queued with the pre-reply disposition.
    record_mail_touch(
        &pool,
        ws,
        &touch(
            "r1",
            MailDirection::Inbound,
            &["curator@channel.tg"],
            now - Duration::days(1),
        ),
    )
    .await?;
    let wrote = record_reply_text(
        &pool,
        ws,
        "r1",
        &candidates[0],
        "Yes, send the press kit.",
        now - Duration::days(1),
    )
    .await?;
    assert!(wrote);
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT metadata->>'reply_text' FROM outreach_interactions \
         WHERE workspace_id = $1 AND target_id = $2 AND source_key = 'gmail:r1'",
    )
    .bind(ws)
    .bind(pitched)
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored.as_deref(), Some("Yes, send the press kit."));
    let queued: (String, Option<String>) = sqlx::query_as(
        "SELECT classification_result, previous_disposition::text FROM reply_classifications \
         WHERE workspace_id = $1 AND target_id = $2 AND reply_text = 'Yes, send the press kit.'",
    )
    .bind(ws)
    .bind(pitched)
    .fetch_one(&pool)
    .await?;
    assert_eq!(queued.0, "auto");
    assert_eq!(queued.1.as_deref(), Some("none"));

    // Same message again: nothing writes twice.
    assert!(
        !record_reply_text(
            &pool,
            ws,
            "r1",
            &candidates[0],
            "Yes, send the press kit.",
            now
        )
        .await?
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM reply_classifications WHERE workspace_id = $1 AND target_id = $2",
    )
    .bind(ws)
    .bind(pitched)
    .fetch_one(&pool)
    .await?;
    assert_eq!(rows, 1);

    // The backfill lists recorded replies that still lack a body — a second
    // answer from the pitched target qualifies; the never-pitched fan mail
    // (p3) does not, because the window gates the listing just like the live
    // path; and r1 dropped out once its body was captured.
    record_mail_touch(
        &pool,
        ws,
        &touch(
            "r2",
            MailDirection::Inbound,
            &["curator@channel.tg"],
            now - Duration::hours(2),
        ),
    )
    .await?;
    let unbodied = unbodied_replies(&pool, ws, now - Duration::days(30), 60, 10).await?;
    let message_ids: Vec<&str> = unbodied.iter().map(|(id, _, _)| id.as_str()).collect();
    assert_eq!(
        message_ids,
        ["r2"],
        "only a pitched contact's unbodied reply is listed"
    );
    Ok(())
}
