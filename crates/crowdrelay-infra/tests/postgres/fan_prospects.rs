//! The person layer against a real schema: what only the database can prove.
//!
//! - one handle is one person and one prospect, however it is capitalized or
//!   prefixed, and re-reading the same comment appends nothing;
//! - a prospect who has said no (refused/suppressed) is not collected against;
//! - retention moves forward only on new evidence and a lapsed, non-progressing
//!   prospect is deleted with its evidence — while a converted prospect (a fan's
//!   provenance) and a refusal (the record of a no) are kept;
//! - erasing a fan's account deletes the public handle the band had on file
//!   for them, and nobody else's;
//! - another tenant's people are invisible.

use crate::common;

use crowdrelay_domain::fan_prospect::{ObservationKind, ProspectSource};
use crowdrelay_infra::fan_prospects::{
    ObserveOutcome, ObservedPerson, expire, link_verified_fan, observe, source_counts,
    status_counts,
};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

async fn workspace(pool: &sqlx::PgPool, label: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
    let ws = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(format!("{label}-{}", ws.simple()))
        .bind(label)
        .execute(pool)
        .await?;
    Ok(ws)
}

fn seen<'a>(handle: &'a str, source_ref: &'a str, at: OffsetDateTime) -> ObservedPerson<'a> {
    ObservedPerson {
        source: ProspectSource::OwnComments,
        platform: "Instagram",
        platform_user_id: None,
        handle: Some(handle),
        display_identity: handle,
        display_name: None,
        profile_url: None,
        kind: ObservationKind::ActiveUnderOurPost,
        source_ref,
        source_url: Some("https://youtu.be/abc"),
        observed_at: at,
        evidence: "Kiedy gracie Wrocław?",
        confidence_basis_points: 3_000,
    }
}

async fn active_fan(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    email: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let fan = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1,$2,$3,'active')",
    )
    .bind(fan)
    .bind(workspace_id)
    .bind(email)
    .execute(pool)
    .await?;
    Ok(fan)
}

async fn count(
    pool: &sqlx::PgPool,
    sql: &str,
    ws: Uuid,
) -> Result<i64, Box<dyn std::error::Error>> {
    Ok(sqlx::query_scalar(sql).bind(ws).fetch_one(pool).await?)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn one_handle_is_one_person_and_rereading_appends_nothing()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "prospects").await?;
    let now = OffsetDateTime::now_utc();

    let first = observe(&pool, ws, &seen("@Kuba_Metal", "c1", now)).await?;
    let ObserveOutcome::Created { prospect_id } = first else {
        panic!("first sight creates: {first:?}");
    };
    // Same person, other spelling, new comment: same prospect, new evidence.
    assert_eq!(
        observe(&pool, ws, &seen("kuba_metal", "c2", now)).await?,
        ObserveOutcome::Known {
            prospect_id,
            appended: true
        }
    );
    // The same comment again: known, nothing appended.
    assert_eq!(
        observe(&pool, ws, &seen("KUBA_METAL", "c2", now)).await?,
        ObserveOutcome::Known {
            prospect_id,
            appended: false
        }
    );
    assert_eq!(
        observe(&pool, ws, &seen("two words", "c3", now)).await?,
        ObserveOutcome::NotAnIdentity
    );

    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM persons WHERE workspace_id = $1",
            ws
        )
        .await?,
        1
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM person_identities WHERE workspace_id = $1",
            ws
        )
        .await?,
        1
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM fan_prospect_observations WHERE workspace_id = $1",
            ws
        )
        .await?,
        2
    );
    let (shown, basis, platform): (String, String, String) = sqlx::query_as(
        "SELECT external_identity, lawful_basis, platform FROM fan_prospects WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (shown.as_str(), basis.as_str(), platform.as_str()),
        ("Kuba_Metal", "legitimate_interest", "instagram")
    );

    let statuses = status_counts(&pool, ws).await?;
    assert_eq!(statuses.len(), 1);
    assert_eq!(
        (statuses[0].status.as_str(), statuses[0].prospects),
        ("observed", 1)
    );
    let sources = source_counts(&pool, ws).await?;
    assert_eq!(
        (
            sources[0].platform.as_str(),
            sources[0].source.as_str(),
            sources[0].prospects,
            sources[0].converted
        ),
        ("instagram", "own_comments", 1, 0)
    );

    // Another tenant sees none of it, and the same handle there is another person.
    let other = workspace(&pool, "prospects-other").await?;
    assert!(status_counts(&pool, other).await?.is_empty());
    assert!(matches!(
        observe(&pool, other, &seen("kuba_metal", "c1", now)).await?,
        ObserveOutcome::Created { .. }
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn stable_provider_id_survives_a_display_name_change()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "prospects-stable-id").await?;
    let now = OffsetDateTime::now_utc();

    let first = ObservedPerson {
        source: ProspectSource::OwnComments,
        platform: "youtube",
        platform_user_id: Some("UCaBcD123"),
        handle: None,
        display_identity: "Metal Fan",
        display_name: Some("Metal Fan"),
        profile_url: None,
        kind: ObservationKind::ActiveUnderOurPost,
        source_ref: "yt-1",
        source_url: None,
        observed_at: now,
        evidence: "ale siadło",
        confidence_basis_points: 3_000,
    };
    let ObserveOutcome::Created { prospect_id } = observe(&pool, ws, &first).await? else {
        panic!("created");
    };
    let renamed = ObservedPerson {
        display_identity: "Nowa Nazwa",
        display_name: Some("Nowa Nazwa"),
        source_ref: "yt-2",
        ..first
    };
    assert_eq!(
        observe(&pool, ws, &renamed).await?,
        ObserveOutcome::Known {
            prospect_id,
            appended: true
        }
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM persons WHERE workspace_id=$1", ws).await?,
        1
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM fan_prospects WHERE workspace_id=$1", ws).await?,
        1
    );
    let shown: String = sqlx::query_scalar(
        "SELECT external_identity FROM fan_prospects WHERE workspace_id=$1 AND id=$2",
    )
    .bind(ws)
    .bind(prospect_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(shown, "Nowa Nazwa");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn conversion_only_links_an_existing_verified_same_workspace_fan()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "prospects-link").await?;
    let other = workspace(&pool, "prospects-link-other").await?;
    let now = OffsetDateTime::now_utc();
    let ObserveOutcome::Created { prospect_id } =
        observe(&pool, ws, &seen("warm_person", "c1", now)).await?
    else {
        panic!("created");
    };

    let foreign = active_fan(&pool, other, "foreign@fan.test").await?;
    assert!(!link_verified_fan(&pool, ws, prospect_id, foreign, now).await?);

    let local = active_fan(&pool, ws, "local@fan.test").await?;
    assert!(
        !link_verified_fan(&pool, ws, prospect_id, local, now).await?,
        "an unverified first-party identity is not enough"
    );
    sqlx::query(
        "INSERT INTO fan_identifiers
             (workspace_id, fan_id, kind, value, source, verified_at)
         VALUES ($1,$2,'email','local@fan.test','test',now())",
    )
    .bind(ws)
    .bind(local)
    .execute(&pool)
    .await?;
    assert!(link_verified_fan(&pool, ws, prospect_id, local, now).await?);

    let linked: (String, Option<Uuid>) = sqlx::query_as(
        "SELECT status, linked_fan_id FROM fan_prospects
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(ws)
    .bind(prospect_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(linked, ("converted".to_owned(), Some(local)));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_no_is_not_collected_against() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "prospects-no").await?;
    let now = OffsetDateTime::now_utc();
    let ObserveOutcome::Created { prospect_id } =
        observe(&pool, ws, &seen("grumpy", "c1", now)).await?
    else {
        panic!("created");
    };
    sqlx::query(
        "UPDATE fan_prospects SET status = 'refused', status_reason = 'asked us to stop'
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(prospect_id)
    .execute(&pool)
    .await?;
    let later = now + Duration::days(2);
    assert_eq!(
        observe(&pool, ws, &seen("grumpy", "c2", later)).await?,
        ObserveOutcome::NotCollected { prospect_id }
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM fan_prospect_observations WHERE workspace_id = $1",
            ws
        )
        .await?,
        1,
        "no new evidence about someone who said no"
    );
    let (seen_at,): (OffsetDateTime,) = sqlx::query_as(
        "SELECT last_seen_at FROM fan_prospects WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(prospect_id)
    .fetch_one(&pool)
    .await?;
    assert!(seen_at < later, "not even a last-seen bump: {seen_at}");
    // The schema refuses a terminal no without a reason.
    let refused_without_reason = sqlx::query(
        "UPDATE fan_prospects SET status = 'suppressed', status_reason = NULL
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(prospect_id)
    .execute(&pool)
    .await;
    assert!(refused_without_reason.is_err());
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn retention_moves_on_evidence_and_deletes_what_never_progressed()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "prospects-expiry").await?;
    let t0 = OffsetDateTime::now_utc() - Duration::days(100);

    // `stale` was last read at t0; `active` was read again at t0 + 70 days.
    let _ = observe(&pool, ws, &seen("stale", "c1", t0)).await?;
    let _ = observe(&pool, ws, &seen("active", "c1", t0)).await?;
    let _ = observe(&pool, ws, &seen("active", "c2", t0 + Duration::days(70))).await?;
    // A refusal and a conversion are old too, and must survive.
    let ObserveOutcome::Created {
        prospect_id: refused,
    } = observe(&pool, ws, &seen("said_no", "c1", t0)).await?
    else {
        panic!("created");
    };
    sqlx::query(
        "UPDATE fan_prospects SET status = 'refused', status_reason = 'declined' WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(refused)
    .execute(&pool)
    .await?;

    let now = OffsetDateTime::now_utc();
    let deleted = expire(&pool, ws, now, 100).await?;
    assert_eq!(deleted, 1, "only `stale` lapsed without progress");
    let remaining: Vec<(String,)> = sqlx::query_as(
        "SELECT external_identity FROM fan_prospects WHERE workspace_id = $1 ORDER BY 1",
    )
    .bind(ws)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        remaining.into_iter().map(|(h,)| h).collect::<Vec<_>>(),
        ["active", "said_no"]
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM persons WHERE workspace_id = $1",
            ws
        )
        .await?,
        2
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM fan_prospect_observations WHERE workspace_id = $1",
            ws
        )
        .await?,
        3,
        "the lapsed prospect's evidence went with it"
    );
    // Expiry is idempotent.
    assert_eq!(expire(&pool, ws, now, 100).await?, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn erasing_a_fan_deletes_the_handle_the_band_held_for_them_and_nobody_elses()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "prospects-erase").await?;
    let now = OffsetDateTime::now_utc();
    let ObserveOutcome::Created {
        prospect_id: joined,
    } = observe(&pool, ws, &seen("leaving_fan", "c1", now)).await?
    else {
        panic!("created");
    };
    let _ = observe(&pool, ws, &seen("bystander", "c1", now)).await?;
    let fan = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status)
         VALUES ($1, $2, 'a@example.test', 'active')",
    )
    .bind(fan)
    .bind(ws)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE fan_prospects SET status = 'converted', linked_fan_id = $3
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(joined)
    .bind(fan)
    .execute(&pool)
    .await?;

    // The statement fan_privacy runs for the person layer.
    sqlx::query(
        "DELETE FROM persons WHERE workspace_id = $1 AND id IN (
             SELECT person_id FROM fan_prospects
             WHERE workspace_id = $1 AND linked_fan_id = $2)",
    )
    .bind(ws)
    .bind(fan)
    .execute(&pool)
    .await?;
    let left: Vec<(String,)> =
        sqlx::query_as("SELECT external_identity FROM fan_prospects WHERE workspace_id = $1")
            .bind(ws)
            .fetch_all(&pool)
            .await?;
    assert_eq!(left, [("bystander".to_owned(),)]);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM fan_prospect_observations WHERE workspace_id = $1",
            ws
        )
        .await?,
        1
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM person_identities WHERE workspace_id = $1",
            ws
        )
        .await?,
        1
    );
    Ok(())
}
