//! Sprint 3.5b.2 — the peer write path, against a real schema.
//!
//! The domain tests prove the transition rules; this target proves what only
//! Postgres can: the operator-action ledger replaying a spent key instead of
//! writing twice, the partial name index turning a live same-name create
//! into a conflict while a rejected tombstone still suppresses it, and the
//! proposal pass landing exactly the acts whose canonicalised genres
//! intersect the band's listing — once, capped, and never a refused name
//! twice.

use crate::common;

use crowdrelay_application::IdempotencyKey;
use crowdrelay_domain::{
    WorkspaceId,
    content_engine::{PeerStatus, PeerTier},
};
use crowdrelay_infra::content_engine::{ContentEngineError, PostgresContentEngineRepository};
use crowdrelay_infra::content_peers::{NewPeer, PeerOutcome, PeerPatch};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn seed_workspace(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!(
            "content-peers-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Content Peers Test")
        .execute(pool)
        .await?;
    Ok(())
}

/// The band's own listing — its `genre_tags` are the genres the proposal
/// pass intersects against.
async fn seed_listing(
    pool: &PgPool,
    workspace_id: Uuid,
    genre_tags: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query(
        "INSERT INTO band_listings (workspace_id, act_name, genre_tags) \
         VALUES ($1, $2, $3)",
    )
    .bind(workspace_id)
    .bind("The Tenant Band")
    .bind(genre_tags)
    .execute(pool)
    .await?;
    Ok(())
}

/// A global peer act with its attributed genre tags — the graph the
/// proposal pass reads. `name_key` is the lower/trim normalisation the
/// writer applies. Returns the act's id so a fixture can bill it or pin
/// its home city.
async fn seed_peer_act(
    pool: &PgPool,
    display_name: &str,
    genres: &[&str],
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let act_id = Uuid::now_v7();
    sqlx::query("INSERT INTO place_peer_acts (id, name_key, display_name) VALUES ($1, $2, $3)")
        .bind(act_id)
        .bind(display_name.to_lowercase())
        .bind(display_name)
        .execute(pool)
        .await?;
    for genre in genres {
        sqlx::query(
            "INSERT INTO place_peer_act_genres
                (peer_act_id, genre_tag, provenance, source_ref)
             VALUES ($1, $2, 'researched', 'test-suite')",
        )
        .bind(act_id)
        .bind(genre)
        .execute(pool)
        .await?;
    }
    Ok(act_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn operator_peers_ledger_and_confirm_patch() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_operator_path(&database).await
}

async fn run_operator_path(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let repo = PostgresContentEngineRepository::new(pool.clone());
    let workspace_id = WorkspaceId::new();
    seed_workspace(pool, workspace_id).await?;
    let unique = workspace_id.into_uuid().simple().to_string();
    let key = |suffix: &str| {
        IdempotencyKey::parse(format!("peer-{unique}-{suffix}")).expect("a valid key")
    };
    let new_peer = |name: String| NewPeer {
        name,
        handles: json!({"youtube": "@theband"}),
        tier: PeerTier::NearPeer,
        watch_for: vec!["format".to_owned()],
        why: "the operator chose them".to_owned(),
        proposed_by: "operator".to_owned(),
        confirmed: true,
    };

    // The create lands confirmed and writes its audit row against the peer
    // it actually inserted — a ledger target no row carries would be a lie.
    let peer = match repo
        .create_operator_peer(
            workspace_id,
            &new_peer(format!("Chosen {unique}")),
            &key("a"),
            None,
        )
        .await?
    {
        PeerOutcome::Applied(peer) => peer,
        PeerOutcome::Replayed(_) => panic!("a fresh key must apply, not replay"),
    };
    assert_eq!(peer.status, PeerStatus::Confirmed);
    assert_eq!(peer.proposed_by, "operator");
    assert!(peer.confirmed_at.is_some());
    let audit_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM operator_actions
         WHERE workspace_id = $1 AND action = 'create_peer' AND target_id = $2",
    )
    .bind(workspace_id.into_uuid())
    .bind(peer.id.into_uuid())
    .fetch_one(pool)
    .await?;
    assert_eq!(audit_count, 1, "the ledger row is the audit");

    // The same key and body replays — the stored row answers, no twin lands.
    match repo
        .create_operator_peer(
            workspace_id,
            &new_peer(format!("Chosen {unique}")),
            &key("a"),
            None,
        )
        .await?
    {
        PeerOutcome::Replayed(row) => assert_eq!(row.id, peer.id),
        PeerOutcome::Applied(_) => panic!("a spent key must replay, not write again"),
    }
    // The same key on a different peer conflicts — the ledger is the record
    // of what a key did, and a reused key must not answer with somebody
    // else's action.
    let reused = repo
        .create_operator_peer(
            workspace_id,
            &new_peer(format!("Other {unique}")),
            &key("a"),
            None,
        )
        .await
        .expect_err("a spent key on a new body conflicts");
    assert!(matches!(reused, ContentEngineError::KeyConflict));
    // A fresh key on the same name conflicts too, case-folded — "already
    // there" is the honest answer where the scanner's same clash is a
    // silent no-op.
    let taken = repo
        .create_operator_peer(
            workspace_id,
            &new_peer(format!("chosen {unique}")),
            &key("b"),
            None,
        )
        .await
        .expect_err("a live same-name row conflicts");
    assert!(matches!(taken, ContentEngineError::PeerNameTaken));

    // A scanner-style proposal arrives with empty handles — never
    // observable until the confirm patches them in the same UPDATE.
    let proposal_id = common::seed_peer(
        pool,
        workspace_id.into_uuid(),
        &format!("Scanned {unique}"),
        "proposed",
        None,
    )
    .await?;
    let patched = match repo
        .resolve_peer_operator(
            workspace_id,
            crowdrelay_domain::PeerId::from_uuid(proposal_id),
            PeerStatus::Confirmed,
            None,
            Some(&PeerPatch {
                handles: Some(json!({"rss": "https://example.test/feed"})),
                watch_for: Some(vec!["tour_routing".to_owned()]),
                tier: Some(PeerTier::Lateral),
            }),
            &key("patch"),
            None,
        )
        .await?
    {
        PeerOutcome::Applied(peer) => peer,
        PeerOutcome::Replayed(_) => panic!("a fresh key must apply, not replay"),
    };
    assert_eq!(patched.status, PeerStatus::Confirmed);
    assert_eq!(patched.handles, json!({"rss": "https://example.test/feed"}));
    assert_eq!(patched.watch_for, vec!["tour_routing".to_owned()]);
    assert_eq!(patched.tier, PeerTier::Lateral);
    assert!(patched.confirmed_at.is_some());

    // Fields on a reject are refused — the row is terminal, there is
    // nothing left to edit.
    let second_id = common::seed_peer(
        pool,
        workspace_id.into_uuid(),
        &format!("Refuse {unique}"),
        "proposed",
        None,
    )
    .await?;
    let with_fields = repo
        .resolve_peer_operator(
            workspace_id,
            crowdrelay_domain::PeerId::from_uuid(second_id),
            PeerStatus::Rejected,
            Some("not our scene"),
            Some(&PeerPatch {
                tier: Some(PeerTier::Lateral),
                ..PeerPatch::default()
            }),
            &key("with-fields"),
            None,
        )
        .await
        .expect_err("a rejection carrying edits is invalid");
    assert!(matches!(with_fields, ContentEngineError::InvalidTransition));

    // The ledger'd resolve: replay answers with the row as it stands.
    let resolved = match repo
        .resolve_peer_operator(
            workspace_id,
            crowdrelay_domain::PeerId::from_uuid(second_id),
            PeerStatus::Rejected,
            Some("not our scene"),
            None,
            &key("r"),
            None,
        )
        .await?
    {
        PeerOutcome::Applied(peer) => peer,
        PeerOutcome::Replayed(_) => panic!("a fresh key must apply, not replay"),
    };
    assert_eq!(resolved.status, PeerStatus::Rejected);
    assert_eq!(resolved.rejection_reason.as_deref(), Some("not our scene"));
    match repo
        .resolve_peer_operator(
            workspace_id,
            crowdrelay_domain::PeerId::from_uuid(second_id),
            PeerStatus::Rejected,
            Some("not our scene"),
            None,
            &key("r"),
            None,
        )
        .await?
    {
        PeerOutcome::Replayed(row) => {
            assert_eq!(row.id, resolved.id);
            assert_eq!(row.status, PeerStatus::Rejected);
        }
        PeerOutcome::Applied(_) => panic!("a spent key must replay, not resolve again"),
    }
    Ok(())
}

/// The proposal pass: peer acts whose canonicalised genres intersect the
/// workspace's listing land `proposed` — once, capped per sweep — and a
/// refused name is never asked again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_proposal_pass_lands_matching_acts_once() -> Result<(), Box<dyn std::error::Error>> {
    let database = common::test_pool("CROWDRELAY_TEST_DATABASE_URL")
        .await
        .expect("connect to the migrated suite database");

    run_proposals(&database).await
}

async fn run_proposals(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let repo = PostgresContentEngineRepository::new(pool.clone());
    let workspace_id = WorkspaceId::new();
    seed_workspace(pool, workspace_id).await?;
    // "doom" canonicalises through the 0316 alias seed to "doom metal";
    // "Deathcore" folds on case to its self-mapped canonical.
    seed_listing(pool, workspace_id.into_uuid(), &["doom", "Deathcore"]).await?;

    // The matching acts — one through the alias bridge, one on a spelling
    // the canonical fold still recognises. The tier each lands is not a
    // constant: Kindred Doom bills a tracked room and is a demonstrated
    // peer; Case Fold's home town resolves, which is scene evidence too;
    // Directory Act matches on genre alone — a famous name on a sheet with
    // no billing, no resolvable city and no lead is aspirational, never a
    // near_peer (the Rammstein case: genre fit is not a level claim).
    let kindred = seed_peer_act(pool, "Kindred Doom", &["doom metal"]).await?;
    let case_fold = seed_peer_act(pool, "Case Fold", &["DOOM METAL"]).await?;
    seed_peer_act(pool, "Directory Act", &["doom metal"]).await?;
    let city_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO cities (slug, name, country_code, latitude, longitude)
         VALUES ('peer-city', 'peer-city', 'PL', 51.1, 17.0) RETURNING id",
    )
    .fetch_one(pool)
    .await?;
    let night = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO events (workspace_id, city_id, slug, title, venue, starts_at, status)
         VALUES ($1, $2, 'peer-night', 'peer-night', 'Klub Peer',
                 now() - interval '20 days', 'completed')
         RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(city_id)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO event_acts (workspace_id, event_id, act_slug, act_name, peer_act_id)
         VALUES ($1, $2, 'kindred-doom', 'Kindred Doom', $3)",
    )
    .bind(workspace_id.into_uuid())
    .bind(night)
    .bind(kindred)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE place_peer_acts SET home_city_id = $2 WHERE id = $1")
        .bind(case_fold)
        .bind(city_id)
        .execute(pool)
        .await?;
    // The ones the pass must leave alone.
    seed_peer_act(pool, "Polka Stars", &["polka"]).await?;
    seed_peer_act(pool, "Genreless Act", &[]).await?;
    seed_peer_act(pool, "Listed Already", &["doom metal"]).await?;
    seed_peer_act(pool, "Refused Once", &["doom metal"]).await?;
    // A live peer already carrying the name is not re-proposed.
    common::seed_peer(
        pool,
        workspace_id.into_uuid(),
        "listed already",
        "confirmed",
        None,
    )
    .await?;
    // Nor is a refused name — the rejection row is the suppression record,
    // and the name match is case-insensitive on both sides.
    common::seed_peer(
        pool,
        workspace_id.into_uuid(),
        "REFUSED ONCE",
        "rejected",
        Some("not our scene"),
    )
    .await?;

    let proposed = repo.propose_peers(workspace_id).await?;
    let mut names: Vec<String> = proposed.iter().map(|peer| peer.name.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "Case Fold".to_owned(),
            "Directory Act".to_owned(),
            "Kindred Doom".to_owned()
        ],
        "exactly the canonical-genre matches land — no non-match, no standing name, no refused name"
    );
    for peer in &proposed {
        assert_eq!(peer.status, PeerStatus::Proposed);
        assert_eq!(peer.proposed_by, "peer-act-graph");
        assert_eq!(peer.handles, json!({}), "the scanner never invents handles");
        assert!(peer.watch_for.is_empty());
        assert!(peer.confirmed_at.is_none());
        assert!(
            peer.why.contains("doom metal"),
            "the why quotes the evidence, got: {}",
            peer.why
        );
    }
    let by_name = |name: &str| proposed.iter().find(|peer| peer.name == name);
    let kindred = by_name("Kindred Doom").ok_or("the billed act proposed")?;
    assert_eq!(kindred.tier, PeerTier::NearPeer);
    assert!(
        kindred.why.contains("billed in 1 tracked rooms"),
        "a billing is the peer evidence, got: {}",
        kindred.why
    );
    let case_fold = by_name("Case Fold").ok_or("the placed act proposed")?;
    assert_eq!(case_fold.tier, PeerTier::NearPeer);
    assert!(
        case_fold.why.contains("placed in a catalogued city"),
        "a resolved home city is the peer evidence, got: {}",
        case_fold.why
    );
    let directory = by_name("Directory Act").ok_or("the bare name proposed")?;
    assert_eq!(
        directory.tier,
        PeerTier::Aspirational,
        "a genre match with no circuit evidence is a name to watch, not a peer"
    );
    assert!(
        directory.why.contains("directory entry"),
        "the why says which evidence is missing, got: {}",
        directory.why
    );

    // A rescan is a no-op: the name index plus the NOT EXISTS guard mean the
    // same sweep can run forever without re-asking.
    assert!(repo.propose_peers(workspace_id).await?.is_empty());

    // The cap: a rich graph cannot flood the queue in one tick — twelve
    // matches land ten, then the remaining two next sweep.
    let rich = WorkspaceId::new();
    seed_workspace(pool, rich).await?;
    seed_listing(pool, rich.into_uuid(), &["wave-cap"]).await?;
    for index in 0..12 {
        seed_peer_act(pool, &format!("Wave Act {index:02}"), &["wave-cap"]).await?;
    }
    assert_eq!(repo.propose_peers(rich).await?.len(), 10);
    assert_eq!(repo.propose_peers(rich).await?.len(), 2);
    assert!(repo.propose_peers(rich).await?.is_empty());
    Ok(())
}

/// Sprint: peers are evidence, not free distribution. A confirmed,
/// observable peer informs suggestions only once the audience owner
/// has actually consented, in the right direction, with headroom and
/// someone reachable — and the research row is never itself authority.
#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn a_confirmed_peer_is_learning_evidence_not_distribution_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let repo = PostgresContentEngineRepository::new(pool.clone());
    let beneficiary = WorkspaceId::new();
    seed_workspace(&pool, beneficiary).await?;
    sqlx::query("UPDATE workspaces SET name='VIRYA test' WHERE id=$1")
        .bind(beneficiary.into_uuid())
        .execute(&pool)
        .await?;

    // Enough capability to make the catalogue's peer_cover. No communities,
    // press or local fan audience: peer reach is the only possible promise in
    // this fixture.
    let member_id: Uuid = sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
         VALUES ($1, $2, 'staff', 'active') RETURNING id",
    )
    .bind(beneficiary.into_uuid())
    .bind(format!(
        "peer-authority-{}@example.test",
        beneficiary.into_uuid().simple()
    ))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1,$2,'video-person',true,ARRAY['video']::text[])",
    )
    .bind(beneficiary.into_uuid())
    .bind(member_id)
    .execute(&pool)
    .await?;

    // Misscore-shaped peer: confirmed and observable, with a YouTube handle.
    // This is useful trend evidence. It is NOT consent to use their audience.
    let peer_name = format!("Misscore {}", beneficiary.into_uuid().simple());
    let peer = repo
        .create_operator_peer(
            beneficiary,
            &NewPeer {
                name: peer_name.clone(),
                handles: json!({"youtube":"@misscore"}),
                tier: PeerTier::NearPeer,
                watch_for: vec!["format".to_owned()],
                why: "good comparable band".to_owned(),
                proposed_by: "operator".to_owned(),
                confirmed: true,
            },
            &IdempotencyKey::parse(format!("pa-{}", Uuid::now_v7().simple()))
                .expect("valid idempotency key"),
            None,
        )
        .await?;
    assert!(matches!(peer, PeerOutcome::Applied(_)));

    let today = time::OffsetDateTime::now_utc().date();
    let without_consent = repo.refresh_suggestions(beneficiary, today).await?;
    assert!(
        without_consent.is_empty(),
        "a confirmed peer alone must not manufacture a distribution promise: {without_consent:?}"
    );

    // A co-bill consent is event-scoped. It must not silently become standing
    // authority for generic peer-content promotion.
    let audience_owner = WorkspaceId::new();
    seed_workspace(&pool, audience_owner).await?;
    sqlx::query("UPDATE workspaces SET name='Misscore' WHERE id=$1")
        .bind(audience_owner.into_uuid())
        .execute(&pool)
        .await?;
    let organization_id: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (slug,name) VALUES ($1,'Test label') RETURNING id",
    )
    .bind(format!(
        "peer-authority-{}",
        beneficiary.into_uuid().simple()
    ))
    .fetch_one(&pool)
    .await?;
    sqlx::query("UPDATE workspaces SET organization_id=$3 WHERE id IN ($1,$2)")
        .bind(beneficiary.into_uuid())
        .bind(audience_owner.into_uuid())
        .bind(organization_id)
        .execute(&pool)
        .await?;

    sqlx::query(
        "INSERT INTO amplification_consents
             (organization_id, from_workspace_id, to_workspace_id, purpose,
              scope, status, max_campaigns_per_month, cooldown_days,
              approved_by, approved_at)
         VALUES ($1,$2,$3,'event_crossbill','double_opt_in','active',2,21,
                 'test',now())",
    )
    .bind(organization_id)
    .bind(audience_owner.into_uuid())
    .bind(beneficiary.into_uuid())
    .execute(&pool)
    .await?;
    let crossbill_only = repo.refresh_suggestions(beneficiary, today).await?;
    assert!(
        crossbill_only.is_empty(),
        "event_crossbill must not authorize generic peer-audience content: {crossbill_only:?}"
    );

    // Direction matters. VIRYA consenting to carry Misscore does not mean
    // Misscore's audience is available to VIRYA.
    sqlx::query(
        "DELETE FROM amplification_consents
         WHERE from_workspace_id=$1 AND to_workspace_id=$2",
    )
    .bind(audience_owner.into_uuid())
    .bind(beneficiary.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO amplification_consents
             (organization_id, from_workspace_id, to_workspace_id, purpose,
              scope, status, max_campaigns_per_month, cooldown_days,
              approved_by, approved_at)
         VALUES ($1,$2,$3,'cross_promote','double_opt_in','active',2,21,
                 'test',now())",
    )
    .bind(organization_id)
    .bind(beneficiary.into_uuid())
    .bind(audience_owner.into_uuid())
    .execute(&pool)
    .await?;
    let wrong_direction = repo.refresh_suggestions(beneficiary, today).await?;
    assert!(
        wrong_direction.is_empty(),
        "reverse consent must not manufacture access to the other act's audience: {wrong_direction:?}"
    );
    sqlx::query(
        "DELETE FROM amplification_consents
         WHERE from_workspace_id=$1 AND to_workspace_id=$2",
    )
    .bind(beneficiary.into_uuid())
    .bind(audience_owner.into_uuid())
    .execute(&pool)
    .await?;

    // A correctly directed edge with nobody actually reachable is still zero
    // reach. The consent is permission, not a made-up audience.
    let edge_id: Uuid = sqlx::query_scalar(
        "INSERT INTO amplification_consents
             (organization_id, from_workspace_id, to_workspace_id, purpose,
              scope, status, max_campaigns_per_month, cooldown_days,
              approved_by, approved_at)
         VALUES ($1,$2,$3,'cross_promote','double_opt_in','active',2,21,
                 'test',now())
         RETURNING id",
    )
    .bind(organization_id)
    .bind(audience_owner.into_uuid())
    .bind(beneficiary.into_uuid())
    .fetch_one(&pool)
    .await?;
    let nobody_reachable = repo.refresh_suggestions(beneficiary, today).await?;
    assert!(
        nobody_reachable.is_empty(),
        "an active edge with zero reachable humans is measured zero reach: {nobody_reachable:?}"
    );

    let owner_fan: Uuid = sqlx::query_scalar(
        "INSERT INTO fans (workspace_id, normalized_email, status)
         VALUES ($1,$2,'active') RETURNING id",
    )
    .bind(audience_owner.into_uuid())
    .bind(format!(
        "misscore-fan-{}@example.test",
        beneficiary.into_uuid().simple()
    ))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents
             (workspace_id, fan_id, purpose, granted, policy_version, source)
         VALUES ($1,$2,'marketing',true,'v1','test')",
    )
    .bind(audience_owner.into_uuid())
    .bind(owner_fan)
    .execute(&pool)
    .await?;

    // Permission can exist while every reachable person is still cooling down.
    sqlx::query(
        "INSERT INTO amplification_deliveries
             (consent_id, from_workspace_id, to_workspace_id, fan_id,
              campaign_reference, delivered_at)
         VALUES ($1,$2,$3,$4,'recent-campaign',now())",
    )
    .bind(edge_id)
    .bind(audience_owner.into_uuid())
    .bind(beneficiary.into_uuid())
    .bind(owner_fan)
    .execute(&pool)
    .await?;
    let cooling_down = repo.refresh_suggestions(beneficiary, today).await?;
    assert!(
        cooling_down.is_empty(),
        "a consent edge with nobody outside cooldown is not available reach: {cooling_down:?}"
    );
    sqlx::query("DELETE FROM amplification_deliveries WHERE consent_id=$1")
        .bind(edge_id)
        .execute(&pool)
        .await?;

    // The monthly campaign ceiling is also an availability gate. Two distinct
    // campaigns spend this edge's cap even if the same fan received both.
    for reference in ["spent-1", "spent-2"] {
        sqlx::query(
            "INSERT INTO amplification_deliveries
                 (consent_id, from_workspace_id, to_workspace_id, fan_id,
                  campaign_reference, delivered_at)
             VALUES ($1,$2,$3,$4,$5,now())",
        )
        .bind(edge_id)
        .bind(audience_owner.into_uuid())
        .bind(beneficiary.into_uuid())
        .bind(owner_fan)
        .bind(reference)
        .execute(&pool)
        .await?;
    }
    let cap_spent = repo.refresh_suggestions(beneficiary, today).await?;
    assert!(
        cap_spent.is_empty(),
        "a fully spent consent edge must not be promised as current reach: {cap_spent:?}"
    );
    sqlx::query("DELETE FROM amplification_deliveries WHERE consent_id=$1")
        .bind(edge_id)
        .execute(&pool)
        .await?;

    // Only now do we have standing consent, the correct direction, monthly
    // headroom and at least one eligible human.
    let consented = repo.refresh_suggestions(beneficiary, today).await?;
    assert!(
        !consented.is_empty(),
        "explicit cross-promotion consent with a reachable fan should unlock peer-audience formats"
    );
    let peer_promises: Vec<&serde_json::Value> = consented
        .iter()
        .filter_map(|suggestion| suggestion.distribution_promise.get("peer_audience"))
        .collect();
    assert!(
        peer_promises.iter().any(|audience| {
            audience
                .as_array()
                .is_some_and(|items| items.iter().any(|name| name.as_str() == Some("Misscore")))
        }),
        "the promise names only the consented audience owner: {consented:?}"
    );
    assert!(
        consented.iter().all(|suggestion| {
            suggestion
                .distribution_promise
                .get("peer_audience")
                .is_none_or(|audience| {
                    audience.as_array().is_some_and(|items| {
                        !items
                            .iter()
                            .any(|name| name.as_str() == Some(peer_name.as_str()))
                    })
                })
        }),
        "the research peer row itself must never become distribution authority"
    );
    Ok(())
}
