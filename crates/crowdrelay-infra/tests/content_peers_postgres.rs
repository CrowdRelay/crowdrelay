//! Sprint 3.5b.2 — the peer write path, against a real schema.
//!
//! The domain tests prove the transition rules; this target proves what only
//! Postgres can: the operator-action ledger replaying a spent key instead of
//! writing twice, the partial name index turning a live same-name create
//! into a conflict while a rejected tombstone still suppresses it, and the
//! proposal pass landing exactly the acts whose canonicalised genres
//! intersect the band's listing — once, capped, and never a refused name
//! twice.

mod common;

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
/// writer applies.
async fn seed_peer_act(
    pool: &PgPool,
    display_name: &str,
    genres: &[&str],
) -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
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
    let proposal = repo
        .create_peer(
            workspace_id,
            &NewPeer {
                name: format!("Scanned {unique}"),
                handles: json!({}),
                tier: PeerTier::NearPeer,
                watch_for: vec![],
                why: "shared genre doom metal".to_owned(),
                proposed_by: "peer-act-graph".to_owned(),
                confirmed: false,
            },
        )
        .await?
        .expect("the proposal lands");
    let patched = repo
        .resolve_peer(
            workspace_id,
            proposal.id,
            PeerStatus::Confirmed,
            None,
            Some(&PeerPatch {
                handles: Some(json!({"rss": "https://example.test/feed"})),
                watch_for: Some(vec!["tour_routing".to_owned()]),
                tier: Some(PeerTier::Lateral),
            }),
        )
        .await?;
    assert_eq!(patched.status, PeerStatus::Confirmed);
    assert_eq!(patched.handles, json!({"rss": "https://example.test/feed"}));
    assert_eq!(patched.watch_for, vec!["tour_routing".to_owned()]);
    assert_eq!(patched.tier, PeerTier::Lateral);
    assert!(patched.confirmed_at.is_some());

    // Fields on a reject are refused — the row is terminal, there is
    // nothing left to edit.
    let second = repo
        .create_peer(
            workspace_id,
            &NewPeer {
                name: format!("Refuse {unique}"),
                handles: json!({}),
                tier: PeerTier::NearPeer,
                watch_for: vec![],
                why: "wrong fit".to_owned(),
                proposed_by: "peer-act-graph".to_owned(),
                confirmed: false,
            },
        )
        .await?
        .expect("the second proposal lands");
    let with_fields = repo
        .resolve_peer(
            workspace_id,
            second.id,
            PeerStatus::Rejected,
            Some("not our scene"),
            Some(&PeerPatch {
                tier: Some(PeerTier::Lateral),
                ..PeerPatch::default()
            }),
        )
        .await
        .expect_err("a rejection carrying edits is invalid");
    assert!(matches!(with_fields, ContentEngineError::InvalidTransition));

    // The ledger'd resolve: replay answers with the row as it stands.
    let resolved = match repo
        .resolve_peer_operator(
            workspace_id,
            second.id,
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
            second.id,
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
    // the canonical fold still recognises.
    seed_peer_act(pool, "Kindred Doom", &["doom metal"]).await?;
    seed_peer_act(pool, "Case Fold", &["DOOM METAL"]).await?;
    // The ones the pass must leave alone.
    seed_peer_act(pool, "Polka Stars", &["polka"]).await?;
    seed_peer_act(pool, "Genreless Act", &[]).await?;
    seed_peer_act(pool, "Listed Already", &["doom metal"]).await?;
    seed_peer_act(pool, "Refused Once", &["doom metal"]).await?;
    // A live peer already carrying the name is not re-proposed.
    repo.create_peer(
        workspace_id,
        &NewPeer {
            name: "listed already".to_owned(),
            handles: json!({}),
            tier: PeerTier::NearPeer,
            watch_for: vec![],
            why: "operator already watches".to_owned(),
            proposed_by: "operator".to_owned(),
            confirmed: true,
        },
    )
    .await?
    .expect("the standing peer lands");
    // Nor is a refused name — the rejection row is the suppression record,
    // and the name match is case-insensitive on both sides.
    let refused = repo
        .create_peer(
            workspace_id,
            &NewPeer {
                name: "REFUSED ONCE".to_owned(),
                handles: json!({}),
                tier: PeerTier::NearPeer,
                watch_for: vec![],
                why: "asked once".to_owned(),
                proposed_by: "peer-act-graph".to_owned(),
                confirmed: false,
            },
        )
        .await?
        .expect("the soon-refused proposal lands");
    repo.resolve_peer(
        workspace_id,
        refused.id,
        PeerStatus::Rejected,
        Some("not our scene"),
        None,
    )
    .await?;

    let proposed = repo.propose_peers(workspace_id).await?;
    let mut names: Vec<String> = proposed.iter().map(|peer| peer.name.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["Case Fold".to_owned(), "Kindred Doom".to_owned()],
        "exactly the canonical-genre matches land — no non-match, no standing name, no refused name"
    );
    for peer in &proposed {
        assert_eq!(peer.status, PeerStatus::Proposed);
        assert_eq!(peer.proposed_by, "peer-act-graph");
        assert_eq!(peer.handles, json!({}), "the scanner never invents handles");
        assert!(peer.watch_for.is_empty());
        assert!(peer.confirmed_at.is_none());
        assert!(
            peer.why.contains("doom metal") && peer.why.contains("tracked rooms"),
            "the why quotes the evidence, got: {}",
            peer.why
        );
    }

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
