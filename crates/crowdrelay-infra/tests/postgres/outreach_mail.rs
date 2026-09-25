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
    MailDirection, MailTouch, message_recorded, record_mail_touch,
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
