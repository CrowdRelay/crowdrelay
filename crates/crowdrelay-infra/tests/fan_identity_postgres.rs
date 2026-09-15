//! The identity spine (§4e-5) on real Postgres: merges move what can move,
//! retain what would collide, tombstone the loser, record the audit — and
//! unmerge restores exactly. Email resolution routes merged-away addresses
//! to the survivor so one person is never two contacts.

use crowdrelay_application::{
    DismissMergeCandidateCommand, FanIdentityError, FanIdentityRepository, MergeFansCommand,
    UnmergeFanCommand,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_infra::fan_identity::{
    PgFanIdentityRepository, record_merge_candidate, resolve_fan_for_email,
};
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use time::OffsetDateTime;
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

async fn pool() -> Result<PgPool> {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let url = std::env::var("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await?;
    crowdrelay_infra::database::MIGRATOR.run(&pool).await?;
    Ok(pool)
}

async fn workspace(pool: &PgPool, label: &str) -> Result<WorkspaceId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("{label}-{}", id.simple()))
        .bind(label)
        .execute(pool)
        .await?;
    Ok(WorkspaceId::from_uuid(id))
}

async fn fan(pool: &PgPool, ws: Uuid, email: &str, status: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO fans (id, workspace_id, normalized_email, status) VALUES ($1,$2,$3,$4)",
    )
    .bind(id)
    .bind(ws)
    .bind(email)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn event(pool: &PgPool, ws: Uuid, slug: &str) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events (id, workspace_id, slug, title, status, starts_at, published_at) \
         VALUES ($1,$2,$3,$4,'published',$5, now())",
    )
    .bind(id)
    .bind(ws)
    .bind(slug)
    .bind(slug)
    .bind(OffsetDateTime::now_utc() + time::Duration::days(7))
    .execute(pool)
    .await?;
    Ok(id)
}

async fn checkin(
    pool: &PgPool,
    ws: Uuid,
    event_id: Uuid,
    campaign_id: Uuid,
    fan_id: Uuid,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO concert_checkins (id, workspace_id, event_id, campaign_id, fan_id, checked_in_at, identity_source) \
         VALUES ($1,$2,$3,$4,$5, now(), 'email_claim')",
    )
    .bind(id)
    .bind(ws)
    .bind(event_id)
    .bind(campaign_id)
    .bind(fan_id)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn qr_campaign(pool: &PgPool, ws: Uuid, event_id: Uuid) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO concert_qr_campaigns (id, workspace_id, event_id, label, valid_from, valid_until) \
         VALUES ($1,$2,$3,'test', now() - interval '1 day', now() + interval '1 day')",
    )
    .bind(id)
    .bind(ws)
    .bind(event_id)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn fan_status(pool: &PgPool, ws: Uuid, fan_id: Uuid) -> Result<(String, Option<Uuid>)> {
    let row = sqlx::query_as::<_, (String, Option<Uuid>)>(
        "SELECT status, merged_into_fan_id FROM fans WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(fan_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

fn merge_cmd(ws: Uuid, survivor: Uuid, merged: Uuid) -> MergeFansCommand {
    MergeFansCommand {
        workspace_id: WorkspaceId::from_uuid(ws),
        survivor_fan_id: survivor,
        merged_fan_id: merged,
        reason: Some("same person".to_string()),
        merged_by: "test-operator".to_string(),
        request_id: "test-request".to_string(),
    }
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn merge_moves_rows_tombstones_and_unmerge_restores() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "merge-basic").await?.into_uuid();
    let survivor = fan(&pool, ws, "survivor@example.com", "active").await?;
    let merged = fan(&pool, ws, "merged@example.com", "pending").await?;
    let event_id = event(&pool, ws, "gig-one").await?;
    let campaign_id = qr_campaign(&pool, ws, event_id).await?;
    let merged_checkin = checkin(&pool, ws, event_id, campaign_id, merged).await?;
    sqlx::query(
        "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source, recorded_at) \
         VALUES ($1,$2,'marketing',true,'v1','test', now())",
    )
    .bind(ws)
    .bind(merged)
    .execute(&pool)
    .await?;

    let repo = PgFanIdentityRepository::new(pool.clone());
    let view = repo.merge_fans(&merge_cmd(ws, survivor, merged)).await?;
    assert_eq!(view.survivor_fan_id, survivor);
    assert_eq!(view.merged_fan_id, merged);
    assert!(view.unmerged_at.is_none());
    // The checkin moved; the merged fan's email identifier followed.
    assert_eq!(view.moved_counts["concert_checkins"], 1);
    assert_eq!(view.moved_counts["fan_identifiers"], 1);
    // The survivor had no marketing consent — the merged fan's latest
    // decision was mirrored, not overwritten onto existing rows.
    assert_eq!(view.consents_mirrored, 1);
    // History stayed: the consent row itself is append-only.
    assert_eq!(view.retained_counts["fan_consents"], 1);

    let (status, merged_into) = fan_status(&pool, ws, merged).await?;
    assert_eq!(status, "merged");
    assert_eq!(merged_into, Some(survivor));

    // Post-merge, the merged fan's email resolves to the survivor — the
    // double-contact fix.
    let mut tx = pool.begin().await?;
    let resolved = resolve_fan_for_email(&mut tx, ws, "merged@example.com").await?;
    assert_eq!(resolved, Some((survivor, "active".to_string())));
    tx.rollback().await?;

    // Unmerge: the recorded moved rows return, the prior status restores.
    let view = repo
        .unmerge_fan(&UnmergeFanCommand {
            workspace_id: WorkspaceId::from_uuid(ws),
            merged_fan_id: merged,
            unmerged_by: "test-operator".to_string(),
            request_id: "test-request".to_string(),
        })
        .await?;
    assert!(view.unmerged_at.is_some());
    let (status, merged_into) = fan_status(&pool, ws, merged).await?;
    assert_eq!(status, "pending");
    assert_eq!(merged_into, None);
    let owner = sqlx::query_scalar::<_, Uuid>(
        "SELECT fan_id FROM concert_checkins WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(merged_checkin)
    .fetch_one(&pool)
    .await?;
    assert_eq!(owner, merged);
    let id_owner = sqlx::query_scalar::<_, Uuid>(
        "SELECT fan_id FROM fan_identifiers WHERE workspace_id = $1 AND kind = 'email' AND value = $2",
    )
    .bind(ws)
    .bind("merged@example.com")
    .fetch_one(&pool)
    .await?;
    assert_eq!(id_owner, merged);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn merge_retains_colliding_rows_instead_of_choosing() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "merge-collide").await?.into_uuid();
    let survivor = fan(&pool, ws, "a@example.com", "active").await?;
    let merged = fan(&pool, ws, "b@example.com", "active").await?;
    let event_id = event(&pool, ws, "gig-two").await?;
    let campaign_id = qr_campaign(&pool, ws, event_id).await?;
    // Both fans checked into the same event — the survivor's row wins and
    // the loser's stays on the tombstone, recorded as retained.
    checkin(&pool, ws, event_id, campaign_id, survivor).await?;
    checkin(&pool, ws, event_id, campaign_id, merged).await?;

    let repo = PgFanIdentityRepository::new(pool.clone());
    let view = repo.merge_fans(&merge_cmd(ws, survivor, merged)).await?;
    assert!(view.moved_counts.get("concert_checkins").is_none());
    assert_eq!(view.retained_counts["concert_checkins"], 1);
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM concert_checkins WHERE workspace_id = $1 AND fan_id = $2",
    )
    .bind(ws)
    .bind(merged)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn merge_resolves_open_candidates_for_the_pair() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "merge-candidates").await?.into_uuid();
    let a = fan(&pool, ws, "cand-a@example.com", "active").await?;
    let b = fan(&pool, ws, "cand-b@example.com", "pending").await?;

    let first =
        record_merge_candidate(&pool, ws, a, b, json!({"kind": "order_email_vs_checkin"})).await?;
    assert!(first.is_some());
    // Idempotent on the canonical pair — either direction, once.
    let again =
        record_merge_candidate(&pool, ws, b, a, json!({"kind": "shared_signal_install"})).await?;
    assert!(again.is_none());

    let repo = PgFanIdentityRepository::new(pool.clone());
    repo.merge_fans(&merge_cmd(ws, a, b)).await?;
    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM fan_merge_candidates WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(first.unwrap())
    .fetch_one(&pool)
    .await?;
    assert_eq!(status, "merged");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn dismiss_marks_candidate_and_rejects_repeats() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "dismiss").await?.into_uuid();
    let a = fan(&pool, ws, "d-a@example.com", "active").await?;
    let b = fan(&pool, ws, "d-b@example.com", "active").await?;
    let id = record_merge_candidate(&pool, ws, a, b, json!({"kind": "shared_signal_install"}))
        .await?
        .unwrap();
    let repo = PgFanIdentityRepository::new(pool.clone());
    repo.dismiss_merge_candidate(&DismissMergeCandidateCommand {
        workspace_id: WorkspaceId::from_uuid(ws),
        candidate_id: id,
    })
    .await?;
    let err = repo
        .dismiss_merge_candidate(&DismissMergeCandidateCommand {
            workspace_id: WorkspaceId::from_uuid(ws),
            candidate_id: id,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, FanIdentityError::AlreadyResolved));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn merge_validation_refuses_bad_shapes() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "merge-validation").await?.into_uuid();
    let a = fan(&pool, ws, "v-a@example.com", "active").await?;
    let b = fan(&pool, ws, "v-b@example.com", "active").await?;
    let repo = PgFanIdentityRepository::new(pool.clone());

    let err = repo.merge_fans(&merge_cmd(ws, a, a)).await.unwrap_err();
    assert!(matches!(
        err,
        FanIdentityError::InvalidMerge(crowdrelay_domain::fan_identity::MergeError::SameFan)
    ));
    let err = repo
        .merge_fans(&merge_cmd(ws, a, Uuid::now_v7()))
        .await
        .unwrap_err();
    assert!(matches!(err, FanIdentityError::NotFound));

    repo.merge_fans(&merge_cmd(ws, a, b)).await?;
    // The tombstone has nothing left to give; the survivor cannot be merged.
    let c = fan(&pool, ws, "v-c@example.com", "active").await?;
    let err = repo.merge_fans(&merge_cmd(ws, c, b)).await.unwrap_err();
    assert!(matches!(
        err,
        FanIdentityError::InvalidMerge(crowdrelay_domain::fan_identity::MergeError::AlreadyMerged)
    ));
    let err = repo.merge_fans(&merge_cmd(ws, b, c)).await.unwrap_err();
    assert!(matches!(
        err,
        FanIdentityError::InvalidMerge(crowdrelay_domain::fan_identity::MergeError::SurvivorMerged)
    ));
    // Nothing left to unmerge twice.
    repo.unmerge_fan(&UnmergeFanCommand {
        workspace_id: WorkspaceId::from_uuid(ws),
        merged_fan_id: b,
        unmerged_by: "op".to_string(),
        request_id: "r".to_string(),
    })
    .await?;
    let err = repo
        .unmerge_fan(&UnmergeFanCommand {
            workspace_id: WorkspaceId::from_uuid(ws),
            merged_fan_id: b,
            unmerged_by: "op".to_string(),
            request_id: "r".to_string(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, FanIdentityError::NothingToUnmerge));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn workspace_isolation_holds() -> Result<()> {
    let pool = pool().await?;
    let ws_a = workspace(&pool, "iso-a").await?.into_uuid();
    let ws_b = workspace(&pool, "iso-b").await?.into_uuid();
    let fan_a = fan(&pool, ws_a, "iso@example.com", "active").await?;
    let fan_b = fan(&pool, ws_b, "iso@example.com", "active").await?;
    let repo = PgFanIdentityRepository::new(pool.clone());
    // A merge in one workspace must not see the other workspace's fan.
    let err = repo
        .merge_fans(&merge_cmd(ws_a, fan_a, fan_b))
        .await
        .unwrap_err();
    assert!(matches!(err, FanIdentityError::NotFound));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn checkin_against_another_fans_ticket_order_records_a_candidate() -> Result<()> {
    use crowdrelay_application::{CheckinCommand, ConcertQrRepository};
    use crowdrelay_infra::concert_qr::PostgresConcertQrRepository;

    let pool = pool().await?;
    let ws = workspace(&pool, "order-cand").await?.into_uuid();
    let buyer = fan(&pool, ws, "buyer@example.com", "active").await?;
    let event_id = event(&pool, ws, "gig-three").await?;
    let campaign_id = qr_campaign(&pool, ws, event_id).await?;
    let pool_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO admission_pools (id, workspace_id, event_id, name, capacity, slug) \
         VALUES ($1,$2,$3,'General',200,'general')",
    )
    .bind(pool_id)
    .bind(ws)
    .bind(event_id)
    .execute(&pool)
    .await?;
    let sale_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO ticket_sales (id, workspace_id, event_id, admission_pool_id, capacity, sales_open_at, sales_close_at) \
         VALUES ($1,$2,$3,$4,200, now() - interval '30 days', now() + interval '7 days')",
    )
    .bind(sale_id)
    .bind(ws)
    .bind(event_id)
    .bind(pool_id)
    .execute(&pool)
    .await?;
    sqlx::query(
        r#"INSERT INTO ticket_orders (
               workspace_id, ticket_sale_id, public_reference, buyer_email,
               currency, amount_gross_minor, amount_net_minor,
               amount_vat_minor, vat_rate_basis_points, reservation_key,
               request_hash, checkout_token_hash, status, paid_at, expires_at
           ) VALUES (
               $1, $2, 'VRY-ORD-0123456789ABCDEF', 'buyer@example.com',
               'PLN', 10000, 8130, 1870, 2300, 'res-cand-1',
               decode(repeat('ab', 32), 'hex'), decode(repeat('cd', 32), 'hex'),
               'paid', now(), now() + interval '1 hour'
           )"#,
    )
    .bind(ws)
    .bind(sale_id)
    .execute(&pool)
    .await?;

    // A different fan checks into the same event — the honest signal that
    // checker and buyer may be one person parks a candidate.
    let repo = PostgresConcertQrRepository::new(pool.clone());
    let slug: String =
        sqlx::query_scalar("SELECT slug FROM events WHERE workspace_id = $1 AND id = $2")
            .bind(ws)
            .bind(event_id)
            .fetch_one(&pool)
            .await?;
    repo.check_in(&CheckinCommand {
        workspace_id: ws,
        event_slug: slug,
        campaign_id,
        event_id,
        expires_at: (OffsetDateTime::now_utc() + time::Duration::days(1)).unix_timestamp(),
        session_token: None,
        email: Some("checker@example.com".to_owned()),
        consent: None,
        now: OffsetDateTime::now_utc(),
        request_id: Some("req-cand".to_owned()),
    })
    .await?;

    let checker = sqlx::query_scalar::<_, Uuid>(
        "SELECT fan_id FROM concert_checkins WHERE workspace_id = $1 AND event_id = $2",
    )
    .bind(ws)
    .bind(event_id)
    .fetch_one(&pool)
    .await?;
    assert_ne!(checker, buyer);

    let (a, b) = if checker < buyer {
        (checker, buyer)
    } else {
        (buyer, checker)
    };
    let evidence = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT evidence FROM fan_merge_candidates \
         WHERE workspace_id = $1 AND fan_id_a = $2 AND fan_id_b = $3 AND status = 'pending'",
    )
    .bind(ws)
    .bind(a)
    .bind(b)
    .fetch_one(&pool)
    .await?;
    assert_eq!(evidence["kind"], "order_email_vs_checkin");

    // A rescan must not duplicate the candidate.
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM fan_merge_candidates WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn email_identifier_attaches_on_fan_create() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "identifier").await?.into_uuid();
    let fan_id = fan(&pool, ws, "auto@example.com", "pending").await?;
    let owner = sqlx::query_scalar::<_, Uuid>(
        "SELECT fan_id FROM fan_identifiers WHERE workspace_id = $1 AND kind = 'email' AND value = $2",
    )
    .bind(ws)
    .bind("auto@example.com")
    .fetch_one(&pool)
    .await?;
    assert_eq!(owner, fan_id);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn merge_refuses_chains_until_the_first_is_unmerged() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "merge-chain").await?.into_uuid();
    let a = fan(&pool, ws, "chain-a@example.com", "active").await?;
    let b = fan(&pool, ws, "chain-b@example.com", "active").await?;
    let c = fan(&pool, ws, "chain-c@example.com", "active").await?;
    let repo = PgFanIdentityRepository::new(pool.clone());

    repo.merge_fans(&merge_cmd(ws, a, b)).await?;
    // B is the survivor of an open merge — merging it into C would orphan
    // the audit an out-of-order unmerge relies on.
    let err = repo.merge_fans(&merge_cmd(ws, c, a)).await.unwrap_err();
    assert!(matches!(
        err,
        FanIdentityError::InvalidMerge(
            crowdrelay_domain::fan_identity::MergeError::MergedFanIsSurvivor
        )
    ));

    repo.unmerge_fan(&UnmergeFanCommand {
        workspace_id: WorkspaceId::from_uuid(ws),
        merged_fan_id: b,
        unmerged_by: "op".to_string(),
        request_id: "r".to_string(),
    })
    .await?;
    // With the open merge closed, merging the restored fan is allowed again.
    repo.merge_fans(&merge_cmd(ws, c, a)).await?;
    let (status, _) = fan_status(&pool, ws, a).await?;
    assert_eq!(status, "merged");
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn unmerge_reopens_the_candidates_the_merge_resolved() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "unmerge-cand").await?.into_uuid();
    let a = fan(&pool, ws, "re-a@example.com", "active").await?;
    let b = fan(&pool, ws, "re-b@example.com", "active").await?;
    let candidate =
        record_merge_candidate(&pool, ws, a, b, json!({"kind": "shared_signal_install"}))
            .await?
            .unwrap();

    let repo = PgFanIdentityRepository::new(pool.clone());
    repo.merge_fans(&merge_cmd(ws, a, b)).await?;
    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM fan_merge_candidates WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(candidate)
    .fetch_one(&pool)
    .await?;
    assert_eq!(status, "merged");

    repo.unmerge_fan(&UnmergeFanCommand {
        workspace_id: WorkspaceId::from_uuid(ws),
        merged_fan_id: b,
        unmerged_by: "op".to_string(),
        request_id: "r".to_string(),
    })
    .await?;
    let row = sqlx::query_as::<_, (String, Option<Uuid>)>(
        "SELECT status, resolved_merge_id FROM fan_merge_candidates \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(ws)
    .bind(candidate)
    .fetch_one(&pool)
    .await?;
    assert_eq!(row.0, "pending");
    assert_eq!(row.1, None);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn merge_repoints_third_party_candidates_to_the_survivor() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "repoint-cand").await?.into_uuid();
    let survivor = fan(&pool, ws, "rp-s@example.com", "active").await?;
    let merged = fan(&pool, ws, "rp-m@example.com", "active").await?;
    let third = fan(&pool, ws, "rp-t@example.com", "active").await?;
    // Evidence pairs the loser with a third fan; merging must not leave a
    // pending candidate pointing at a tombstone.
    record_merge_candidate(
        &pool,
        ws,
        merged,
        third,
        json!({"kind": "shared_signal_install"}),
    )
    .await?;

    let repo = PgFanIdentityRepository::new(pool.clone());
    repo.merge_fans(&merge_cmd(ws, survivor, merged)).await?;

    let (a, b) = crowdrelay_domain::fan_identity::canonical_pair(survivor, third);
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT status FROM fan_merge_candidates \
         WHERE workspace_id = $1 AND fan_id_a = $2 AND fan_id_b = $3",
    )
    .bind(ws)
    .bind(a)
    .bind(b)
    .fetch_one(&pool)
    .await?;
    assert_eq!(row.0, "pending");
    // Nothing still names the tombstone in a pending pair.
    let stale = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM fan_merge_candidates WHERE workspace_id = $1 \
         AND status = 'pending' AND (fan_id_a = $2 OR fan_id_b = $2)",
    )
    .bind(ws)
    .bind(merged)
    .fetch_one(&pool)
    .await?;
    assert_eq!(stale, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn merge_retains_a_second_paid_pass_in_the_same_pool() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "merge-paid").await?.into_uuid();
    let survivor = fan(&pool, ws, "p-s@example.com", "active").await?;
    let merged = fan(&pool, ws, "p-m@example.com", "active").await?;
    let event_id = event(&pool, ws, "gig-paid").await?;
    let pool_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO admission_pools (id, workspace_id, event_id, name, capacity, slug) \
         VALUES ($1,$2,$3,'General',200,'general')",
    )
    .bind(pool_id)
    .bind(ws)
    .bind(event_id)
    .execute(&pool)
    .await?;
    for fan_id in [survivor, merged] {
        sqlx::query(
            "INSERT INTO admission_passes \
                 (id, workspace_id, event_id, admission_pool_id, fan_id, issuance_method, \
                  public_reference, claim_expires_at, status, issued_at, claimed_at, \
                  claim_token_consumed_at) \
             VALUES ($1,$2,$3,$4,$5,'first_come',$6, now() + interval '1 day', 'claimed', now(), \
                     now(), now())",
        )
        .bind(Uuid::now_v7())
        .bind(ws)
        .bind(event_id)
        .bind(pool_id)
        .bind(fan_id)
        .bind(format!("VIRYA-{}", Uuid::now_v7().simple()))
        .execute(&pool)
        .await?;
    }

    let repo = PgFanIdentityRepository::new(pool.clone());
    let view = repo.merge_fans(&merge_cmd(ws, survivor, merged)).await?;
    // The pool holds one pass per fan — the loser's paid pass cannot move
    // onto the survivor, so it stays on the tombstone, recorded as retained.
    assert_eq!(view.retained_counts["admission_passes"], 1);
    assert!(view.moved_counts.get("admission_passes").is_none());
    let owner_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM admission_passes \
         WHERE workspace_id = $1 AND admission_pool_id = $2 AND fan_id = $3",
    )
    .bind(ws)
    .bind(pool_id)
    .bind(merged)
    .fetch_one(&pool)
    .await?;
    assert_eq!(owner_count, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn merge_moves_campaign_deliveries_with_their_recipients() -> Result<()> {
    let pool = pool().await?;
    let ws = workspace(&pool, "merge-delivery").await?.into_uuid();
    let survivor = fan(&pool, ws, "d-s@example.com", "active").await?;
    let merged = fan(&pool, ws, "d-m@example.com", "active").await?;
    let segment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO audience_segments (id, workspace_id, slug, name, filter, active) \
         VALUES ($1,$2,'seg','seg','{}',true)",
    )
    .bind(segment_id)
    .bind(ws)
    .execute(&pool)
    .await?;
    let campaign_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO communication_campaigns \
             (id, workspace_id, segment_id, slug, name, channel, template_key, content, status) \
         VALUES ($1,$2,$3,'camp','camp','email','post_show','{}','draft')",
    )
    .bind(campaign_id)
    .bind(ws)
    .bind(segment_id)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO communication_campaign_recipients (workspace_id, campaign_id, fan_id, snapshotted_at) \
         VALUES ($1,$2,$3, now())",
    )
    .bind(ws)
    .bind(campaign_id)
    .bind(merged)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO communication_campaign_deliveries \
             (workspace_id, campaign_id, fan_id, attempt_key, status, claimed_at, completed_at) \
         VALUES ($1,$2,$3,'a1','delivered', now(), now())",
    )
    .bind(ws)
    .bind(campaign_id)
    .bind(merged)
    .execute(&pool)
    .await?;

    let repo = PgFanIdentityRepository::new(pool.clone());
    // The survivor was never in this campaign: both the recipient row and
    // its delivery must move — a delivery can only re-point once its
    // parent recipient already names the survivor.
    let view = repo.merge_fans(&merge_cmd(ws, survivor, merged)).await?;
    assert_eq!(view.moved_counts["communication_campaign_recipients"], 1);
    assert_eq!(view.moved_counts["communication_campaign_deliveries"], 1);
    let owner = sqlx::query_scalar::<_, Uuid>(
        "SELECT fan_id FROM communication_campaign_deliveries \
         WHERE workspace_id = $1 AND campaign_id = $2",
    )
    .bind(ws)
    .bind(campaign_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(owner, survivor);
    Ok(())
}
