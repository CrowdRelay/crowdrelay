//! The roster archive against a real schema (5.14).
//!
//! Worth a database rather than a unit test for the usual reason: the
//! marks are trigger-maintained from `events`, the recurrence is a join
//! across `workspaces.organization_id`, and the registry's identity is a
//! normalized-email unique key — none of it checked at compile time. What
//! is asserted: a published event naming a counterparty marks the shared
//! row; the same counterparty across two member acts reports
//! `shared_by_acts = 2` with both shares; a draft or cancelled event marks
//! nothing; an outsider workspace's mark never enters the archive; and the
//! same email under a different tenant resolves to one registry row.

mod common;

use crowdrelay_infra::roster_counterparty_archive::counterparty_archive;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

async fn organization(pool: &PgPool, slug: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(slug)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn workspace(
    pool: &PgPool,
    slug: &str,
    organization_id: Option<Uuid>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name, organization_id) VALUES ($1, $2, $2, $3)")
        .bind(id)
        .bind(slug)
        .bind(organization_id)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn event(
    pool: &PgPool,
    workspace_id: Uuid,
    title: &str,
    starts_at: OffsetDateTime,
    status: &str,
    counterparty_name: Option<&str>,
    counterparty_email: Option<&str>,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"INSERT INTO events (id, workspace_id, slug, title, starts_at, status,
                               published_at, counterparty_name, counterparty_email)
           VALUES ($1, $2, $3, $3, $4, $5,
                   CASE WHEN $5 IN ('published','completed') THEN now() ELSE NULL END,
                   $6, $7)"#,
    )
    .bind(id)
    .bind(workspace_id)
    .bind(title)
    .bind(starts_at)
    .bind(status)
    .bind(counterparty_name)
    .bind(counterparty_email)
    .execute(pool)
    .await?;
    Ok(id)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_same_promoter_across_acts_reports_the_recurrence()
-> Result<(), Box<dyn std::error::Error>> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let org = organization(&db, "label").await?;
        let act_a = workspace(&db, "act-a", Some(org)).await?;
        let act_b = workspace(&db, "act-b", Some(org)).await?;
        let outsider = workspace(&db, "outsider", None).await?;
        let now = OffsetDateTime::now_utc();

        // The shared promoter: act A met them twice, act B once — the
        // recurrence the archive exists to surface.
        event(
            &db,
            act_a,
            "a-first",
            now - time::Duration::days(90),
            "completed",
            Some("Promoter Pat"),
            Some("pat@promo.example"),
        )
        .await?;
        event(
            &db,
            act_a,
            "a-second",
            now - time::Duration::days(30),
            "completed",
            Some("Promoter Pat"),
            Some("PAT@promo.example"),
        )
        .await?;
        event(
            &db,
            act_b,
            "b-first",
            now - time::Duration::days(10),
            "published",
            Some("Pat P."),
            Some("pat@promo.example"),
        )
        .await?;
        // An act-private counterparty: recurrence of one.
        event(
            &db,
            act_a,
            "a-third",
            now - time::Duration::days(5),
            "completed",
            Some("Solo Sam"),
            Some("sam@venue.example"),
        )
        .await?;
        // A draft and a cancelled event mark nothing.
        event(
            &db,
            act_b,
            "b-draft",
            now + time::Duration::days(5),
            "draft",
            Some("Drafty"),
            Some("draft@example.com"),
        )
        .await?;
        event(
            &db,
            act_b,
            "b-cancelled",
            now - time::Duration::days(3),
            "cancelled",
            Some("Never"),
            Some("never@example.com"),
        )
        .await?;
        // The outsider's mark on the same promoter email resolves to the
        // same registry row but must not enter our archive.
        event(
            &db,
            outsider,
            "o-first",
            now - time::Duration::days(1),
            "completed",
            Some("Promoter Pat"),
            Some("pat@promo.example"),
        )
        .await?;

        let archive = counterparty_archive(&db, org, now).await?;
        assert_eq!(archive.member_count, 2);
        assert_eq!(
            archive.counterparties.len(),
            2,
            "draft/cancelled mark nothing"
        );

        let pat = &archive.counterparties[0];
        assert_eq!(pat.email, "pat@promo.example");
        assert_eq!(pat.shared_by_acts, 2);
        // Three member marks — the outsider's fourth never enters.
        assert_eq!(pat.shows, 3);
        // The most recent member mark's name is the displayed one.
        assert_eq!(pat.display_name.as_deref(), Some("Pat P."));
        assert_eq!(pat.acts.len(), 2);
        let act_a_share = pat
            .acts
            .iter()
            .find(|share| share.workspace_id == act_a)
            .expect("act a's share");
        assert_eq!(act_a_share.shows, 2);

        let sam = &archive.counterparties[1];
        assert_eq!(sam.shared_by_acts, 1);
        assert_eq!(sam.shows, 1);

        // The registry itself is global: the outsider's mark joined the
        // same counterparty row — one identity, three tenants' claims.
        let registry_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM place_counterparties WHERE email_key = 'pat@promo.example'",
        )
        .fetch_one(&db)
        .await?;
        assert_eq!(registry_count, 1);
        let mark_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM place_counterparty_marks WHERE counterparty_id = (
                 SELECT id FROM place_counterparties WHERE email_key = 'pat@promo.example')",
        )
        .fetch_one(&db)
        .await?;
        assert_eq!(
            mark_count, 4,
            "the outsider's mark exists in the ledger, just not in our archive"
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_retracted_counterparty_unmarks_the_event() -> Result<(), Box<dyn std::error::Error>> {
    let db = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    async {
        let org = organization(&db, "label").await?;
        let act = workspace(&db, "act", Some(org)).await?;
        let now = OffsetDateTime::now_utc();

        let event_id = event(
            &db,
            act,
            "the-show",
            now - time::Duration::days(7),
            "completed",
            Some("Pat"),
            Some("pat@promo.example"),
        )
        .await?;
        // The show is cancelled — the claim it made retracts.
        sqlx::query("UPDATE events SET status = 'cancelled' WHERE id = $1")
            .bind(event_id)
            .execute(&db)
            .await?;
        let marks: i64 = sqlx::query_scalar("SELECT count(*) FROM place_counterparty_marks")
            .fetch_one(&db)
            .await?;
        assert_eq!(marks, 0, "a cancelled night is not a met promoter");

        let archive = counterparty_archive(&db, org, now).await?;
        assert!(archive.counterparties.is_empty());
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await
}
