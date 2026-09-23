//! Archive → fanbase bulk promote (F1), against a real Postgres.
//!
//! The segment cut, the one-transaction import+mark, and the
//! suppressed-stays-staged rule are the parts of bulk promote a mock cannot
//! see — the predicates are SQL and the atomicity is a real transaction.
//! Workspace/seed helpers come from `audience_portfolio`, where the fan
//! import's other live-database tests already live.
//!
//! Run via `just test-postgres`.

use crate::audience_portfolio::{cleanup, pool, seed_workspace};
use crowdrelay_infra::fan_import::{ImportEntry, InvitationContext, PostgresFanImportRepository};
use crowdrelay_infra::gdrive::{ContactSegment, PostgresGDriveRepository};
use sqlx::PgPool;
use uuid::Uuid;

/// One staged archive row. `sources` is the connector list the extractor
/// merged — `{"gdrive"}` etc. — and the optional columns drive the segment
/// predicates directly.
async fn seed_drive_contact(
    pool: &PgPool,
    workspace: Uuid,
    email: &str,
    suggested_kind: Option<&str>,
    staged_status: Option<&str>,
    disappeared: bool,
    last_inbound: bool,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO drive_contacts
            (id, workspace_id, normalized_email, suggested_kind, staged_status,
             disappeared_at, last_inbound_at, source_file_id, source_file_name,
             sources)
        VALUES ($1, $2, $3, $4, $5,
                CASE WHEN $6 THEN now() END,
                CASE WHEN $7 THEN now() - interval '40 days' END,
                'file-1', 'contacts.csv', '{gdrive}')
        "#,
    )
    .bind(id)
    .bind(workspace)
    .bind(email)
    .bind(suggested_kind)
    .bind(staged_status)
    .bind(disappeared)
    .bind(last_inbound)
    .execute(pool)
    .await
    .expect("seed drive contact");
    id
}

/// The API's promote-batch flow at the layer the transaction lives on:
/// segment read → import → mark, all inside one `tx`.
async fn bulk_promote(
    pool: &PgPool,
    workspace: Uuid,
    segment: ContactSegment,
    invitation: &InvitationContext,
) -> Result<
    (crowdrelay_infra::fan_import::ImportCounts, Vec<String>, u64),
    Box<dyn std::error::Error>,
> {
    let gdrive = PostgresGDriveRepository::new(pool.clone());
    let import = PostgresFanImportRepository::new(pool.clone());
    let contacts = gdrive
        .staged_fan_contacts_in_segment(workspace, segment)
        .await?;
    let mut tx = pool.begin().await?;
    let mut counts = crowdrelay_infra::fan_import::ImportCounts::default();
    let mut suppressed: Vec<String> = Vec::new();
    let mut promotable: Vec<Uuid> = Vec::new();
    let mut groups: std::collections::BTreeMap<String, Vec<ImportEntry>> =
        std::collections::BTreeMap::new();
    for contact in &contacts {
        groups
            .entry(contact.sources.join("+"))
            .or_default()
            .push(ImportEntry {
                email: contact.normalized_email.clone(),
                display_name: contact.display_name.clone(),
                locale: None,
            });
    }
    for (source, entries) in &groups {
        let outcome = import
            .import_batch_in_tx(&mut tx, workspace, source, entries, 2, 60, invitation)
            .await?;
        counts.imported_pending += outcome.counts.imported_pending;
        counts.confirmation_resent += outcome.counts.confirmation_resent;
        counts.already_active += outcome.counts.already_active;
        counts.skipped_suppressed += outcome.counts.skipped_suppressed;
        counts.cooldown_skipped += outcome.counts.cooldown_skipped;
        suppressed.extend(outcome.suppressed_emails);
    }
    for contact in &contacts {
        if !suppressed
            .iter()
            .any(|email| email == &contact.normalized_email)
        {
            promotable.push(contact.id);
        }
    }
    let marked = gdrive
        .mark_fans_promoted_by_ids(&mut tx, workspace, &promotable)
        .await?;
    tx.commit().await?;
    Ok((counts, suppressed, marked))
}

async fn contact_outcome(pool: &PgPool, workspace: Uuid, id: Uuid) -> String {
    sqlx::query_scalar("SELECT fan_outcome FROM drive_contacts WHERE workspace_id = $1 AND id = $2")
        .bind(workspace)
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("contact outcome")
}

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn bulk_promote_segment_imports_and_marks_in_one_tx() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = pool().await;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let workspace = seed_workspace(&pool, "bulkpromote").await;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, 'crew_locale', 'pl')",
    )
    .bind(workspace)
    .execute(&pool)
    .await?;

    // Three staged rows: a personal address, an organisation, a typed venue.
    // Only the first is a likely fan.
    let fan_row = seed_drive_contact(
        &pool,
        workspace,
        "basia@gmail.com",
        None,
        None,
        false,
        false,
    )
    .await;
    let org_row = seed_drive_contact(
        &pool,
        workspace,
        "bookings@klubx.pl",
        None,
        None,
        false,
        false,
    )
    .await;
    let venue_row = seed_drive_contact(
        &pool,
        workspace,
        "room@stodola.pl",
        Some("venue"),
        None,
        false,
        false,
    )
    .await;

    let (counts, suppressed, marked) = bulk_promote(
        &pool,
        workspace,
        ContactSegment::LikelyFan,
        &InvitationContext {
            locale: Some("pl".to_owned()),
            reason: Some("Przenosimy listę do Signal".to_owned()),
        },
    )
    .await?;
    assert_eq!(counts.imported_pending, 1);
    assert_eq!(counts.skipped_suppressed, 0);
    assert!(suppressed.is_empty());
    assert_eq!(marked, 1);

    // Exactly one pending fan, and the confirmation carries the tenant's
    // voice: crew locale as the locale fallback and the operator's reason.
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fans WHERE workspace_id = $1 AND status = 'pending'",
    )
    .bind(workspace)
    .fetch_one(&pool)
    .await?;
    assert_eq!(pending, 1);
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events \
         WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested'",
    )
    .bind(workspace)
    .fetch_one(&pool)
    .await?;
    assert_eq!(payload["locale"], "pl");
    assert_eq!(
        payload["invitation"]["reason"],
        "Przenosimy listę do Signal"
    );
    assert_eq!(payload["invitation"]["source_label"], "archive");
    assert_eq!(payload["import_source"], "gdrive");

    // Only the fan row moved; the org and the venue stay staged.
    assert_eq!(contact_outcome(&pool, workspace, fan_row).await, "promoted");
    assert_eq!(contact_outcome(&pool, workspace, org_row).await, "staged");
    assert_eq!(contact_outcome(&pool, workspace, venue_row).await, "staged");

    // No `cleanup` here: importing writes an `audit_events` row, and that
    // table is append-only by trigger, so the helper's DELETE is refused.
    // Each run seeds its own workspace, so leaving the rows is harmless.
    Ok(())
}

/// A failure after the import inside the same transaction must leave no fan
/// row at all — the import and the marks commit together or not at all.
#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn bulk_promote_rolls_back_when_mark_fails() -> Result<(), Box<dyn std::error::Error>> {
    let pool = pool().await;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let workspace = seed_workspace(&pool, "bulkrollback").await;
    let contact = seed_drive_contact(
        &pool,
        workspace,
        "rollback@gmail.com",
        None,
        None,
        false,
        false,
    )
    .await;

    let import = PostgresFanImportRepository::new(pool.clone());
    let mut tx = pool.begin().await?;
    let outcome = import
        .import_batch_in_tx(
            &mut tx,
            workspace,
            "gdrive",
            &[ImportEntry {
                email: "rollback@gmail.com".to_owned(),
                display_name: None,
                locale: None,
            }],
            2,
            60,
            &InvitationContext::default(),
        )
        .await?;
    assert_eq!(outcome.counts.imported_pending, 1);

    // The mark stage blows up (a nonexistent id cannot fail an UPDATE, so
    // a statement that cannot succeed stands in for whatever the write
    // hits — division by zero aborts the transaction on the spot).
    let failed = sqlx::query("SELECT 1/0").execute(&mut *tx).await;
    assert!(failed.is_err());
    drop(tx);

    let fans: i64 = sqlx::query_scalar("SELECT count(*) FROM fans WHERE workspace_id = $1")
        .bind(workspace)
        .fetch_one(&pool)
        .await?;
    assert_eq!(fans, 0, "the import must roll back with the failed mark");
    assert_eq!(
        contact_outcome(&pool, workspace, contact).await,
        "staged",
        "the staging row never moved either"
    );

    cleanup(&pool, &[workspace]).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn segment_counts_match_predicates() -> Result<(), Box<dyn std::error::Error>> {
    let pool = pool().await;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let workspace = seed_workspace(&pool, "segments").await;
    let gdrive = PostgresGDriveRepository::new(pool.clone());

    // Personal provider, no kind → likely fan.
    seed_drive_contact(&pool, workspace, "a@gmail.com", None, None, false, false).await;
    // Corporate domain but already wrote in → still a likely fan.
    seed_drive_contact(&pool, workspace, "b@company.pl", None, None, false, true).await;
    // Corporate domain, never wrote → organisation review.
    seed_drive_contact(&pool, workspace, "c@klubx.pl", None, None, false, false).await;
    // Typed by the sheet → beacon.
    seed_drive_contact(
        &pool,
        workspace,
        "d@stodola.pl",
        Some("venue"),
        None,
        false,
        false,
    )
    .await;
    // Verification sheet's verdict → inactive.
    seed_drive_contact(
        &pool,
        workspace,
        "e@gmail.com",
        None,
        Some("inactive"),
        false,
        false,
    )
    .await;
    // Retracted by the source → gone.
    seed_drive_contact(&pool, workspace, "f@gmail.com", None, None, true, false).await;
    // A decided row — promoted on the fan axis, dismissed on the beacon axis.
    let decided =
        seed_drive_contact(&pool, workspace, "g@gmail.com", None, None, false, false).await;
    sqlx::query(
        "UPDATE drive_contacts SET fan_outcome = 'promoted', beacon_outcome = 'dismissed' \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace)
    .bind(decided)
    .execute(&pool)
    .await?;

    let counts = gdrive.segment_counts(workspace).await?;
    assert_eq!(counts.likely_fan, 2, "personal + inbound-corporate");
    assert_eq!(counts.likely_org, 1, "corporate without inbound");
    assert_eq!(counts.beacon, 1);
    assert_eq!(counts.inactive, 1);
    assert_eq!(counts.gone, 1);
    assert_eq!(counts.decided, 1);

    // The page filter and the count agree — the same predicate drives both.
    let page = gdrive
        .list_contacts(workspace, Some(ContactSegment::LikelyFan), 500)
        .await?;
    assert_eq!(page.len(), 2);
    let orgs = gdrive
        .list_contacts(workspace, Some(ContactSegment::LikelyOrg), 500)
        .await?;
    assert_eq!(orgs.len(), 1);
    assert_eq!(orgs[0].row.normalized_email, "c@klubx.pl");

    cleanup(&pool, &[workspace]).await;
    Ok(())
}

/// A suppressed address is skipped by the import and must not be marked —
/// the staging row stays `staged`, exactly like the single promote's rule.
#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn suppressed_address_stays_staged_in_bulk() -> Result<(), Box<dyn std::error::Error>> {
    let pool = pool().await;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let workspace = seed_workspace(&pool, "bulksuppress").await;
    let contact = seed_drive_contact(
        &pool,
        workspace,
        "unsubscribed@gmail.com",
        None,
        None,
        false,
        false,
    )
    .await;
    sqlx::query(
        "INSERT INTO fans (workspace_id, normalized_email, status) VALUES ($1, $2, 'suppressed')",
    )
    .bind(workspace)
    .bind("unsubscribed@gmail.com")
    .execute(&pool)
    .await?;

    let (counts, suppressed, marked) = bulk_promote(
        &pool,
        workspace,
        ContactSegment::LikelyFan,
        &InvitationContext::default(),
    )
    .await?;
    assert_eq!(counts.skipped_suppressed, 1);
    assert_eq!(suppressed, vec!["unsubscribed@gmail.com".to_owned()]);
    assert_eq!(marked, 0, "nothing suppressed is ever marked promoted");
    assert_eq!(contact_outcome(&pool, workspace, contact).await, "staged");

    let outbox: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events \
         WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested'",
    )
    .bind(workspace)
    .fetch_one(&pool)
    .await?;
    assert_eq!(outbox, 0, "no confirmation mail leaves for a suppression");

    // Same append-only reason as the first test: the import's audit row
    // cannot be deleted, so this test leaves its workspace behind.
    Ok(())
}

/// `locale` in the confirmation payload: the entry's own tag wins, the crew
/// locale is the fallback, and unset stays `null` — never an invented "en".
#[tokio::test]
#[ignore = "requires an explicit CROWDRELAY_TEST_DATABASE_URL PostgreSQL database"]
async fn confirmation_payload_carries_crew_locale_or_null() -> Result<(), Box<dyn std::error::Error>>
{
    let pool = pool().await;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    let import = PostgresFanImportRepository::new(pool.clone());

    // Crew locale set, entry without one → the payload carries "pl".
    let with_locale = seed_workspace(&pool, "locale-pl").await;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, 'crew_locale', 'pl')",
    )
    .bind(with_locale)
    .execute(&pool)
    .await?;
    let mut tx = pool.begin().await?;
    import
        .import_batch_in_tx(
            &mut tx,
            with_locale,
            "csv",
            &[ImportEntry {
                email: "crew@x.test".to_owned(),
                display_name: None,
                locale: None,
            }],
            2,
            60,
            &InvitationContext {
                locale: Some("pl".to_owned()),
                reason: None,
            },
        )
        .await?;
    tx.commit().await?;
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events \
         WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested'",
    )
    .bind(with_locale)
    .fetch_one(&pool)
    .await?;
    assert_eq!(payload["locale"], "pl");
    assert_eq!(payload["invitation"]["source_label"], "archive");
    assert!(payload["invitation"]["reason"].is_null());

    // Neither set → JSON null, not a guessed default.
    let without_locale = seed_workspace(&pool, "locale-none").await;
    let mut tx = pool.begin().await?;
    import
        .import_batch_in_tx(
            &mut tx,
            without_locale,
            "csv",
            &[ImportEntry {
                email: "plain@x.test".to_owned(),
                display_name: None,
                locale: None,
            }],
            2,
            60,
            &InvitationContext::default(),
        )
        .await?;
    tx.commit().await?;
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events \
         WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested'",
    )
    .bind(without_locale)
    .fetch_one(&pool)
    .await?;
    assert!(
        payload["locale"].is_null(),
        "an unmeasured locale is null: {payload}"
    );

    // An entry's own locale still wins over the crew tag.
    let entry_wins = seed_workspace(&pool, "locale-entry").await;
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, 'crew_locale', 'pl')",
    )
    .bind(entry_wins)
    .execute(&pool)
    .await?;
    let mut tx = pool.begin().await?;
    import
        .import_batch_in_tx(
            &mut tx,
            entry_wins,
            "csv",
            &[ImportEntry {
                email: "own@x.test".to_owned(),
                display_name: None,
                locale: Some("en".to_owned()),
            }],
            2,
            60,
            &InvitationContext {
                locale: Some("pl".to_owned()),
                reason: None,
            },
        )
        .await?;
    tx.commit().await?;
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events \
         WHERE workspace_id = $1 AND event_type = 'fan.confirmation_requested'",
    )
    .bind(entry_wins)
    .fetch_one(&pool)
    .await?;
    assert_eq!(payload["locale"], "en", "the entry's own locale wins");

    // Same append-only reason: three imports, three audit rows that stay.
    Ok(())
}
