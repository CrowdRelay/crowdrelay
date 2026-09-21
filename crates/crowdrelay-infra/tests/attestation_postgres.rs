//! An attestation must survive the three things people will try on it.
//!
//! Somebody will edit the number in the document they were handed. Somebody
//! will write a document from scratch and sign it with their own key. Somebody
//! will keep using a link the band withdrew. Each of those is a business
//! outcome, not a bug class, and each is checked here against a real database
//! because the guarantees live partly in SQL: the digest uniqueness, the
//! immutability trigger and the revocation timestamp are all schema.
//!
//! The measurement queries are driven too. They name columns across
//! `ticket_orders`, `concert_checkins`, `fans`, `fan_consents` and
//! `fan_location_preferences`, and a column that does not exist is a runtime
//! failure this repository has no compile-time check for — SQLx runs queries at
//! runtime here by design. `concert_checkins` was already guessed wrong once
//! while this file was being written (as `event_check_ins`), which is the whole
//! argument for driving them.

mod common;

use crowdrelay_infra::attestation::{
    AttestationError, AttestationSigningKey, PostgresAttestationRepository,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

fn key() -> AttestationSigningKey {
    AttestationSigningKey::derive_from_secret(b"attestation-test-signing-secret")
}

async fn seed_workspace(pool: &PgPool) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("ws-{}", id.simple()))
        .bind("Test Workspace")
        .execute(pool)
        .await?;
    Ok(id)
}

/// Two paid orders across two events by one buyer, plus a third buyer with one
/// order. Gives `tickets_sold = 4` across 3 orders (one order carries two
/// tickets — a fixture where orders equal tickets could not tell the join
/// apart) and `repeat_attenders = 1`.
async fn seed_ticket_history(
    pool: &PgPool,
    workspace_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    let city = sqlx::query_scalar::<_, Uuid>("SELECT id FROM cities LIMIT 1")
        .fetch_one(pool)
        .await?;
    for (email, event_index, quantity) in [
        ("returning@example.com", 0, 2),
        ("returning@example.com", 1, 1),
        ("once@example.com", 2, 1),
    ] {
        let event_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO events (workspace_id, city_id, slug, title, venue, starts_at, status)
            VALUES ($1, $2, $3, $4, 'Klub X', now() - interval '30 days', 'completed')
            ON CONFLICT (workspace_id, slug) DO UPDATE SET title = EXCLUDED.title
            RETURNING id
            "#,
        )
        .bind(workspace_id)
        .bind(city)
        .bind(format!("show-{event_index}"))
        .bind(format!("Show {event_index}"))
        .fetch_one(pool)
        .await?;

        // A sale needs an admission pool, and both carry more required columns
        // than a fixture wants to know about. They are spelled out rather than
        // defaulted because getting one wrong is exactly the failure this file
        // exists to catch, and a fixture that quietly diverges from the real
        // schema tests nothing.
        let existing = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM ticket_sales WHERE workspace_id = $1 AND event_id = $2 LIMIT 1",
        )
        .bind(workspace_id)
        .bind(event_id)
        .fetch_optional(pool)
        .await?;
        let sale_id = match existing {
            Some(id) => id,
            None => {
                let pool_id = sqlx::query_scalar::<_, Uuid>(
                    r#"
                    INSERT INTO admission_pools (workspace_id, event_id, name, slug, capacity)
                    VALUES ($1, $2, 'General', $3, 500)
                    RETURNING id
                    "#,
                )
                .bind(workspace_id)
                .bind(event_id)
                .bind(format!("general-{event_index}"))
                .fetch_one(pool)
                .await
                .map_err(|error| format!("admission pool {event_index}: {error}"))?;

                sqlx::query_scalar::<_, Uuid>(
                    r#"
                    INSERT INTO ticket_sales
                        (workspace_id, event_id, admission_pool_id, capacity,
                         sales_open_at, sales_close_at)
                    VALUES ($1, $2, $3, 500,
                            now() - interval '90 days', now() - interval '31 days')
                    RETURNING id
                    "#,
                )
                .bind(workspace_id)
                .bind(event_id)
                .bind(pool_id)
                .fetch_one(pool)
                .await
                .map_err(|error| format!("ticket sale {event_index}: {error}"))?
            }
        };

        // `paid_at` is not decoration: a CHECK ties it to the status, so an
        // order marked paid without it is rejected. That constraint is why the
        // measurement query can trust `status` alone.
        // `paid_at` is not decoration: a CHECK ties it to the status, so an
        // order marked paid without it is rejected. That constraint is why the
        // measurement query can trust `status` alone.
        let order_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO ticket_orders
                (workspace_id, ticket_sale_id, public_reference, buyer_email, status,
                 currency, amount_gross_minor, amount_net_minor, amount_vat_minor,
                 vat_rate_basis_points, reservation_key, request_hash,
                 checkout_token_hash, expires_at, paid_at)
            VALUES ($1, $2, $3, $4, 'paid', 'PLN', 5000, 4630, 370, 800,
                    $5, $6, $7, now() + interval '1 day', now() - interval '29 days')
            RETURNING id
            "#,
        )
        .bind(workspace_id)
        .bind(sale_id)
        // `public_reference` is CHECKed against `^VRY-ORD-[A-F0-9]{16}$`, and
        // the two hashes are `bytea` of exactly 32 bytes. Spelled out because
        // a fixture that sidesteps the real constraints proves nothing about
        // the real table.
        .bind(format!(
            "VRY-ORD-{}",
            Uuid::now_v7().simple().to_string()[..16].to_uppercase()
        ))
        .bind(email)
        .bind(Uuid::now_v7().to_string())
        .bind(vec![0u8; 32])
        .bind(vec![1u8; 32])
        .fetch_one(pool)
        .await
        .map_err(|error| format!("order for {email}: {error}"))?;

        // The figure sums items, not orders — an order with no item row is
        // not a sale of anything, so every fixture order carries one.
        let ticket_type_id = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO ticket_types
                (workspace_id, ticket_sale_id, slug, name, price_gross_minor)
            VALUES ($1, $2, $3, 'General', 5000)
            ON CONFLICT (workspace_id, ticket_sale_id, slug) DO UPDATE
              SET name = EXCLUDED.name
            RETURNING id
            "#,
        )
        .bind(workspace_id)
        .bind(sale_id)
        .bind(format!("general-{event_index}"))
        .fetch_one(pool)
        .await
        .map_err(|error| format!("ticket type {event_index}: {error}"))?;
        sqlx::query(
            r#"
            INSERT INTO ticket_order_items
                (workspace_id, ticket_order_id, ticket_type_id, quantity,
                 unit_gross_minor, unit_net_minor, unit_vat_minor,
                 total_gross_minor, total_net_minor, total_vat_minor)
            VALUES ($1, $2, $3, $4, 2500, 2315, 185,
                    2500 * $4, 2315 * $4, 185 * $4)
            "#,
        )
        .bind(workspace_id)
        .bind(order_id)
        .bind(ticket_type_id)
        .bind(quantity)
        .execute(pool)
        .await
        .map_err(|error| format!("order item {event_index}: {error}"))?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_attestation_survives_the_three_things_people_try()
-> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run(&database).await
}

async fn run(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = seed_workspace(pool).await?;
    seed_ticket_history(pool, workspace).await?;
    let repository = PostgresAttestationRepository::new(pool.clone(), key());
    let now = OffsetDateTime::now_utc();

    // ── Measurement: the queries name columns that exist, and count right. ──
    let issued = repository
        .issue_for_workspace(workspace, "Virya", &["wroclaw".to_owned()], now)
        .await?;

    let tickets = issued
        .figures
        .iter()
        .find(|figure| figure.metric.as_str() == "tickets_sold")
        .ok_or("tickets_sold was not measured")?;
    assert_eq!(
        tickets.value,
        crowdrelay_domain::attestation::PublishedValue::Exact(4),
        "three paid orders carrying four tickets did not count as four"
    );

    let repeats = issued
        .figures
        .iter()
        .find(|figure| figure.metric.as_str() == "repeat_attenders")
        .ok_or("repeat_attenders was not measured")?;
    assert_eq!(
        repeats.value,
        crowdrelay_domain::attestation::PublishedValue::Exact(1),
        "one buyer across two events did not count as one repeat attender"
    );

    // ── The happy path: a link-holder verifies. ─────────────────────────────
    let token = sqlx::query_scalar::<_, Uuid>(
        "SELECT share_token FROM viryaos_attestations WHERE digest = $1",
    )
    .bind(&issued.digest)
    .fetch_one(pool)
    .await?;

    let seen = repository.read_by_token(token, now).await?;
    assert!(seen.unedited, "a freshly issued document read as edited");
    assert!(seen.issued_by_us, "our own signature did not verify");
    assert!(seen.current);
    assert!(!seen.revoked);
    assert_eq!(seen.verdict(), "issued by CrowdRelay, unedited, current");

    // A stranger with only the digest gets the same answer, no token, no
    // account. This is the path a label's lawyer actually uses.
    let by_digest = repository.verify_digest(&issued.digest, now).await?;
    assert_eq!(
        by_digest.verdict(),
        "issued by CrowdRelay, unedited, current"
    );

    // ── Try one: edit the number in the stored document. ────────────────────
    //
    // The trigger refuses, which is the strongest possible answer: the edit
    // does not happen at all, so there is no edited document to detect.
    let edit =
        sqlx::query("UPDATE viryaos_attestations SET figures = '[]'::jsonb WHERE digest = $1")
            .bind(&issued.digest)
            .execute(pool)
            .await;
    assert!(
        edit.is_err(),
        "an issued attestation's figures were editable in place"
    );
    for column in [
        "act_name = 'Somebody Else'",
        "valid_until = now() + interval '900 days'",
    ] {
        let forced = sqlx::query(&format!(
            "UPDATE viryaos_attestations SET {column} WHERE digest = $1"
        ))
        .bind(&issued.digest)
        .execute(pool)
        .await;
        assert!(forced.is_err(), "`{column}` was editable after issue");
    }

    // ── Try two: forge one. ─────────────────────────────────────────────────
    //
    // A row written directly, signed under somebody else's key. The digest is
    // internally consistent — they computed it correctly — and it is still not
    // ours, which is exactly the distinction `unedited` and `issued_by_us`
    // exist to keep apart.
    let forger = AttestationSigningKey::derive_from_secret(b"a-forgers-secret");
    let forged_digest = "f".repeat(64);
    sqlx::query(
        r#"
        INSERT INTO viryaos_attestations
            (workspace_id, act_name, figures, issued_at, valid_until, digest, signature)
        VALUES ($1, 'Virya', '[]'::jsonb, now(), now() + interval '30 days', $2, $3)
        "#,
    )
    .bind(workspace)
    .bind(&forged_digest)
    .bind(PostgresAttestationRepository::new(pool.clone(), forger).sign_digest(&forged_digest))
    .execute(pool)
    .await?;
    let forged = repository.verify_digest(&forged_digest, now).await?;
    assert!(
        !forged.issued_by_us,
        "a document signed with another key verified as ours"
    );
    assert_eq!(forged.verdict(), "not issued by CrowdRelay");

    // ── Try three: keep using a withdrawn link. ─────────────────────────────
    repository.revoke(workspace, &issued.digest, now).await?;
    let after_revoke = repository.read_by_token(token, now).await?;
    assert!(after_revoke.revoked);
    assert!(
        after_revoke.issued_by_us && after_revoke.unedited,
        "revocation must not make a real document look forged"
    );
    assert_eq!(
        after_revoke.verdict(),
        "issued by CrowdRelay, then withdrawn by the act"
    );

    // A retried revoke finds the row already withdrawn — that is the same
    // outcome the caller asked for, not an error. A digest the workspace
    // never issued stays NotFound either way.
    repository
        .revoke(workspace, &issued.digest, now)
        .await
        .expect("a repeated revoke must be idempotent");
    assert!(
        matches!(
            repository
                .revoke(workspace, "sha256:never-issued", now)
                .await,
            Err(AttestationError::NotFound)
        ),
        "revoking a digest that was never issued must stay NotFound"
    );

    // Rotating the token kills the link that was sent.
    let fresh = repository
        .rotate_share_token(workspace, &issued.digest)
        .await?;
    assert_ne!(fresh, token);
    assert!(
        matches!(
            repository.read_by_token(token, now).await,
            Err(AttestationError::NotFound)
        ),
        "an old share link still resolved after rotation"
    );

    // ── Re-issuing an identical document un-revokes it. ─────────────────────
    //
    // A digest collides only when the same workspace issues the same figures
    // inside the same second. That used to be `DO NOTHING`, so a band that
    // revoked a link sent to the wrong person and immediately re-issued got
    // their old token back, still pointing at the revoked row: they believed
    // they had a fresh link, the recipient read "withdrawn by the act", and
    // nothing anywhere said otherwise.
    let reissued = repository
        .issue_for_workspace(workspace, "Virya", &["wroclaw".to_owned()], now)
        .await?;
    assert_eq!(
        reissued.digest, issued.digest,
        "identical facts at the same instant should be the same document"
    );
    let after_reissue = repository.verify_digest(&issued.digest, now).await?;
    assert!(
        !after_reissue.revoked,
        "re-issuing left the document withdrawn, so the band's new link was dead on arrival"
    );
    let reissued_token = repository
        .share_token_for(workspace, &issued.digest)
        .await?;
    assert_ne!(
        reissued_token, fresh,
        "re-issue handed back the previous token, so a link the band meant to retire stayed live"
    );

    // ── Expiry is read at the moment of asking, not baked in. ───────────────
    let later = now + time::Duration::days(31);
    let expired = repository.verify_digest(&issued.digest, later).await?;
    assert!(
        !expired.current,
        "a 31-day-old document still read as current"
    );

    Ok(())
}
