//! Sprint 3.5a persistence contract, against a real schema.
//!
//! The unit tests prove the domain rules; this target proves the table
//! behaviour the unit tests cannot see — the dedup indexes, the
//! `WHERE status = expected` transition guards under a real `UPDATE`, and
//! that every row a second workspace asks for comes back empty.

use crate::common;
use crowdrelay_application::IdempotencyKey;
use crowdrelay_domain::{
    WorkspaceId,
    content_engine::{PeerStatus, PeerTier},
    team_operations::TeamSkill,
};
use crowdrelay_infra::content_engine::{
    ContentEngineError, NewFanObservation, NewPeerObservation, PostgresContentEngineRepository,
};
use crowdrelay_infra::content_peers::{NewPeer, PeerOutcome};
use serde_json::json;
use sqlx::PgPool;
use time::{Date, Month};
use uuid::Uuid;

fn date(year: i32, month: u8, day: u8) -> Date {
    Date::from_calendar_date(year, Month::try_from(month).expect("valid month"), day)
        .expect("valid date")
}

async fn repository()
-> Result<(PostgresContentEngineRepository, PgPool), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    Ok((PostgresContentEngineRepository::new(pool.clone()), pool))
}

/// Every workspace table now `REFERENCES workspaces(id)` — a test workspace
/// has to exist before rows under it can land.
async fn seed_workspace(
    pool: &PgPool,
    workspace_id: WorkspaceId,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!(
            "content-engine-{}",
            workspace_id.into_uuid().simple()
        ))
        .bind("Content Engine Test")
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn catalogue_is_seeded_and_global() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, _pool) = repository().await?;
    let entries = repo.list_format_entries().await?;
    assert!(
        entries.len() >= 30,
        "the seeded catalogue should carry §4b-2's formats, found {}",
        entries.len()
    );
    let playthrough = entries
        .iter()
        .find(|entry| entry.key == "playthrough")
        .expect("the metal staple is seeded");
    assert_eq!(playthrough.effort_standalone.as_str(), "medium");
    assert_eq!(
        playthrough.effort_marginal.as_str(),
        "low",
        "a playthrough while the shoot is already happening is near-free"
    );
    assert_eq!(playthrough.skill.as_str(), "video");
    Ok(())
}

/// A fresh idempotency key for each operator-path call — the ledger turns a
/// reused key into a replay, which is not the assertion these fixtures make.
fn key(label: &str) -> IdempotencyKey {
    IdempotencyKey::parse(format!("ce-{label}-{}", Uuid::now_v7().simple()))
        .expect("valid idempotency key")
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn peers_resolve_once_and_dedup_by_name() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let unique = workspace_id.into_uuid().simple().to_string();
    let name = format!("Peer Band {unique}");

    // A scanner proposal lands `proposed` — never observed until confirmed.
    let proposed = match repo
        .create_operator_peer(
            workspace_id,
            &NewPeer {
                name: name.clone(),
                handles: json!({"spotify": "peer-band"}),
                tier: PeerTier::NearPeer,
                watch_for: vec!["Format".to_owned(), "format".to_owned()],
                why: "same circuit".to_owned(),
                proposed_by: "release-scanner".to_owned(),
                confirmed: false,
            },
            &key("create"),
            None,
        )
        .await?
    {
        PeerOutcome::Applied(peer) => peer,
        PeerOutcome::Replayed(_) => panic!("a fresh key must apply, not replay"),
    };
    assert_eq!(proposed.status, PeerStatus::Proposed);
    assert_eq!(
        proposed.watch_for,
        vec!["format".to_owned()],
        "watch_for is normalized and deduplicated"
    );

    // The same name again — scanner retries and operator typos both collapse
    // into the standing-row conflict.
    let taken = repo
        .create_operator_peer(
            workspace_id,
            &NewPeer {
                name: name.clone(),
                handles: json!({}),
                tier: PeerTier::Aspirational,
                watch_for: vec![],
                why: "duplicate".to_owned(),
                proposed_by: "operator".to_owned(),
                confirmed: true,
            },
            &key("dupe"),
            None,
        )
        .await
        .expect_err("a live same-name row conflicts");
    assert!(matches!(taken, ContentEngineError::PeerNameTaken));

    // Proposed peers are invisible to sweeps.
    let confirmed_before = repo
        .list_peers(workspace_id, Some(PeerStatus::Confirmed))
        .await?;
    assert!(confirmed_before.is_empty());

    let confirmed = match repo
        .resolve_peer_operator(
            workspace_id,
            proposed.id,
            PeerStatus::Confirmed,
            None,
            None,
            &key("confirm"),
            None,
        )
        .await?
    {
        PeerOutcome::Applied(peer) => peer,
        PeerOutcome::Replayed(_) => panic!("a fresh key must apply, not replay"),
    };
    assert_eq!(confirmed.status, PeerStatus::Confirmed);
    assert!(confirmed.confirmed_at.is_some());

    // A confirmed peer cannot be rejected afterwards — one way only.
    let err = repo
        .resolve_peer_operator(
            workspace_id,
            proposed.id,
            PeerStatus::Rejected,
            Some("late"),
            None,
            &key("late-reject"),
            None,
        )
        .await
        .expect_err("a decided peer is final");
    assert!(matches!(err, ContentEngineError::InvalidTransition));

    // A rejection without its reason is refused — the reason is the record
    // that stops the same wrong name being proposed twice.
    let second_id = common::seed_peer(
        &pool,
        workspace_id.into_uuid(),
        &format!("Second {unique}"),
        "proposed",
        None,
    )
    .await?;
    let second = crowdrelay_domain::PeerId::from_uuid(second_id);
    let silent = repo
        .resolve_peer_operator(
            workspace_id,
            second,
            PeerStatus::Rejected,
            None,
            None,
            &key("silent"),
            None,
        )
        .await
        .expect_err("a reasonless rejection is refused");
    assert!(matches!(silent, ContentEngineError::MissingReason));
    let rejected = match repo
        .resolve_peer_operator(
            workspace_id,
            second,
            PeerStatus::Rejected,
            Some("wrong genre"),
            None,
            &key("reject"),
            None,
        )
        .await?
    {
        PeerOutcome::Applied(peer) => peer,
        PeerOutcome::Replayed(_) => panic!("a fresh key must apply, not replay"),
    };
    assert_eq!(rejected.rejection_reason.as_deref(), Some("wrong genre"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn observations_dedup_the_same_fact() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let peer_id: Uuid = common::seed_peer(
        &pool,
        workspace_id.into_uuid(),
        &format!("Observed {}", workspace_id.into_uuid().simple()),
        "confirmed",
        None,
    )
    .await?;

    let fact = NewPeerObservation {
        peer_id: crowdrelay_domain::PeerId::from_uuid(peer_id),
        observed_at: date(2026, 9, 10),
        platform: "youtube".to_owned(),
        kind: "post".to_owned(),
        fact: "posted a playthrough, 1.2M views in 9 days".to_owned(),
        url: Some("https://example.test/v/1".to_owned()),
        metrics: json!({"views": 1_200_000}),
    };
    let first = repo.record_observation(workspace_id, &fact).await?;
    let second = repo.record_observation(workspace_id, &fact).await?;
    assert!(first.is_some());
    assert_eq!(second, None, "a repeated sweep records the fact once");

    let stored: String = sqlx::query_scalar(
        "SELECT fact FROM peer_observations WHERE workspace_id = $1 ORDER BY observed_at DESC, id DESC LIMIT 1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored, fact.fact);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn upcoming_production_events_reads_the_schedule() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    sqlx::query(
        "INSERT INTO production_events (id, workspace_id, kind, title, scheduled_for)
         VALUES ($1, $2, 'shoot', 'video shoot', '2026-10-02')",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    let upcoming = repo
        .upcoming_production_events(workspace_id, date(2026, 9, 1))
        .await?;
    assert_eq!(upcoming.len(), 1);
    assert_eq!(upcoming[0].title, "video shoot");

    // A past-day boundary and a foreign workspace both see nothing.
    assert!(
        repo.upcoming_production_events(workspace_id, date(2026, 10, 3))
            .await?
            .is_empty()
    );
    let theirs = WorkspaceId::new();
    seed_workspace(&pool, theirs).await?;
    assert!(
        repo.upcoming_production_events(theirs, date(2026, 9, 1))
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn a_second_workspace_sees_nothing() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let ours = WorkspaceId::new();
    seed_workspace(&pool, ours).await?;
    // The foreign workspace exists — the resolve must still miss because the
    // peer id is ours, not because there is nowhere to write.
    let theirs = WorkspaceId::new();
    seed_workspace(&pool, theirs).await?;
    let peer_id = common::seed_peer(
        &pool,
        ours.into_uuid(),
        &format!("Scoped {}", ours.into_uuid().simple()),
        "proposed",
        None,
    )
    .await?;
    repo.record_observation(
        ours,
        &NewPeerObservation {
            peer_id: crowdrelay_domain::PeerId::from_uuid(peer_id),
            observed_at: date(2026, 9, 12),
            platform: "instagram".to_owned(),
            kind: "post".to_owned(),
            fact: "tour announce".to_owned(),
            url: None,
            metrics: json!({}),
        },
    )
    .await?;
    sqlx::query(
        "INSERT INTO content_suggestions (id, workspace_id, format_key, concept, status)
         VALUES ($1, $2, 'playthrough', 'a beat', 'raised')",
    )
    .bind(Uuid::now_v7())
    .bind(ours.into_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO arcs (id, workspace_id, title, spine)
         VALUES ($1, $2, 'season', '[]'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .bind(ours.into_uuid())
    .execute(&pool)
    .await?;

    assert!(repo.list_peers(theirs, None).await?.is_empty());
    assert!(
        repo.upcoming_production_events(theirs, date(2020, 1, 1))
            .await?
            .is_empty()
    );
    let foreign_rows: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM peer_observations WHERE workspace_id = $1)
              + (SELECT count(*) FROM content_suggestions WHERE workspace_id = $1)
              + (SELECT count(*) FROM arcs WHERE workspace_id = $1)",
    )
    .bind(theirs.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(foreign_rows, 0, "every content row is workspace-scoped");

    // And the other workspace cannot reach our rows by id either: a scoped
    // UPDATE matches nothing rather than answering a foreign peer.
    let resolved = match repo
        .resolve_peer_operator(
            theirs,
            crowdrelay_domain::PeerId::from_uuid(peer_id),
            PeerStatus::Rejected,
            Some("x"),
            None,
            &key("cross"),
            None,
        )
        .await
    {
        Err(ContentEngineError::InvalidTransition) => true,
        other => panic!("the workspace clause makes a foreign id a no-match: {other:?}"),
    };
    assert!(resolved);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn fan_observations_deduplicate_and_scope_to_the_place()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    // Fan rows hang off a discovery_places row — the community the posts
    // were seen in.
    let place_id = sqlx::query_scalar::<_, uuid::Uuid>(
        "INSERT INTO discovery_places (workspace_id, place_kind, platform, name, url)
         VALUES ($1, 'subreddit', 'reddit', 'r/testmetal', 'https://www.reddit.com/r/testmetal')
         RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;

    let post = NewFanObservation {
        place_id,
        observed_at: date(2026, 9, 14),
        platform: "reddit".to_owned(),
        kind: "post".to_owned(),
        fact: "What albums this week?".to_owned(),
        url: Some("https://www.reddit.com/comments/abc123".to_owned()),
        metrics: json!({"score": 412, "comments": 96}),
    };
    assert!(
        repo.record_fan_observation(workspace_id, &post)
            .await?
            .is_some()
    );
    // A post that stays hot reappears in every sweep; the dedup index, not
    // luck, is what keeps the second sighting from becoming a second row.
    assert!(
        repo.record_fan_observation(workspace_id, &post)
            .await?
            .is_none(),
        "the same fact at the same place+date must not record twice"
    );

    let stored: (String, Uuid, serde_json::Value) = sqlx::query_as(
        "SELECT fact, place_id, metrics FROM fan_observations
         WHERE workspace_id = $1 ORDER BY observed_at DESC, id DESC LIMIT 1",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(stored.0, "What albums this week?");
    assert_eq!(stored.1, place_id);
    assert_eq!(stored.2["score"], json!(412));

    // Another workspace sees none of it.
    let theirs = WorkspaceId::new();
    seed_workspace(&pool, theirs).await?;
    let foreign: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fan_observations WHERE workspace_id = $1")
            .bind(theirs.into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(foreign, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn trends_detect_corroboration_and_fade() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    // Two peers agreeing on a format inside the window is the confirmed
    // case — corroboration is the claim.
    let today = time::OffsetDateTime::now_utc().date();
    let recent = today - time::Duration::days(3);
    for name in ["Peer Alpha", "Peer Beta"] {
        let peer_id =
            common::seed_peer(&pool, workspace_id.into_uuid(), name, "confirmed", None).await?;
        for (day, fact) in [
            (recent, "new playthrough of the single"),
            (today, "another playthrough video"),
        ] {
            repo.record_observation(
                workspace_id,
                &NewPeerObservation {
                    peer_id: crowdrelay_domain::PeerId::from_uuid(peer_id),
                    observed_at: day,
                    platform: "youtube".to_owned(),
                    kind: "video".to_owned(),
                    fact: fact.to_owned(),
                    url: None,
                    metrics: json!({}),
                },
            )
            .await?;
        }
    }

    let live = repo.refresh_trends(workspace_id, today).await?;
    assert!(live > 0, "corroborating facts should produce trends");

    let trends = repo.list_trends(workspace_id).await?;
    let playthrough = trends
        .iter()
        .find(|t| t.pattern == "playthrough")
        .expect("the agreed format is a trend");
    assert_eq!(playthrough.status.as_str(), "confirmed");
    assert_eq!(playthrough.sources, 2);
    assert!(
        playthrough.evidence["peer"].as_array().unwrap().len() >= 3,
        "the evidence links are the claim's receipts"
    );

    // A second workspace shares nothing.
    let theirs = WorkspaceId::new();
    seed_workspace(&pool, theirs).await?;
    assert!(repo.list_trends(theirs).await?.is_empty());

    // When the facts age out of the window, the trend fades instead of
    // vanishing — fading is a fact, not a deletion.
    let later = today + time::Duration::days(60);
    assert_eq!(repo.refresh_trends(workspace_id, later).await?, 0);
    let faded = repo.list_trends(workspace_id).await?;
    assert!(
        faded.iter().all(|t| t.status.as_str() == "faded"),
        "a pattern that stopped appearing says so"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn capability_profile_reads_roster_and_material() -> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;

    // Empty roster, no material — nothing is covered.
    let bare = repo.capability_profile(workspace_id).await?;
    assert!(bare.skills.is_empty());
    assert!(!bare.has_release_material);
    assert!(!bare.has_show_material);

    // A member is the FK anchor for a team profile; the profile carries
    // the skills.
    let member_id: Uuid = sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
         VALUES ($1, $2, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "filmer-{}@example.test",
        workspace_id.into_uuid().simple()
    ))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, 'filmer', true, ARRAY['video','photography']::text[])",
    )
    .bind(workspace_id.into_uuid())
    .bind(member_id)
    .execute(&pool)
    .await?;

    // An inactive profile must not widen capability — someone who left is
    // not capacity.
    let gone_id: Uuid = sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
         VALUES ($1, $2, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "gone-{}@example.test",
        workspace_id.into_uuid().simple()
    ))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, 'gone', false, ARRAY['english_copy']::text[])",
    )
    .bind(workspace_id.into_uuid())
    .bind(gone_id)
    .execute(&pool)
    .await?;

    // The second switch: a member disabled at the identity level with an
    // active profile cannot receive work either — routing requires both.
    let disabled_id: Uuid = sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
         VALUES ($1, $2, 'staff', 'disabled') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!(
        "disabled-{}@example.test",
        workspace_id.into_uuid().simple()
    ))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, 'disabled', true, ARRAY['polish_copy']::text[])",
    )
    .bind(workspace_id.into_uuid())
    .bind(disabled_id)
    .execute(&pool)
    .await?;

    // A published future show and an active upcoming release are material;
    // a draft show and a spent release are not.
    let slug_suffix = workspace_id.into_uuid().simple().to_string();
    sqlx::query(
        "INSERT INTO events (workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1, $2, 'Cap Show', now() + interval '20 days', 'published', now())",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("cap-show-{slug_suffix}"))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO events (workspace_id, slug, title, starts_at, status)
         VALUES ($1, $2, 'Draft Show', now() + interval '20 days', 'draft')",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("cap-draft-{slug_suffix}"))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO release_plans (workspace_id, source_key, title, release_at, active)
         VALUES ($1, $2, 'Cap Release', now() + interval '14 days', true)",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("cap-rel-{slug_suffix}"))
    .execute(&pool)
    .await?;

    let capable = repo.capability_profile(workspace_id).await?;
    assert!(
        capable.skills.contains(&TeamSkill::Video)
            && capable.skills.contains(&TeamSkill::Photography),
        "the active roster's union is the capability: {capable:?}"
    );
    assert!(
        !capable.skills.contains(&TeamSkill::EnglishCopy),
        "an inactive profile's skill must not count"
    );
    assert!(
        !capable.skills.contains(&TeamSkill::PolishCopy),
        "a disabled member's skill must not count — routing cannot reach them either"
    );
    assert!(
        capable.has_show_material,
        "a published future show is material"
    );
    assert!(
        capable.has_release_material,
        "an active upcoming release is material"
    );

    // Isolation: another workspace reads its own empty truth.
    let other = WorkspaceId::new();
    seed_workspace(&pool, other).await?;
    let theirs = repo.capability_profile(other).await?;
    assert!(theirs.skills.is_empty());
    assert!(!theirs.has_show_material);
    assert!(!theirs.has_release_material);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn a_confirmed_peer_is_learning_evidence_not_distribution_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
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
            &key("misscore-peer"),
            None,
        )
        .await?;
    assert!(matches!(peer, PeerOutcome::Applied(_)));

    // Model the operator-visible bad task that already exists before this fix:
    // a raised collaboration promise names Misscore's audience even though no
    // distribution authority exists. Refresh must heal the queue as well as
    // preventing new bad suggestions.
    let stale_task = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO content_suggestions (
             id, workspace_id, format_key, concept, reason, evidence,
             distribution_promise, status, expires_at
         ) VALUES (
             $1,$2,'peer_cover','Promote Misscore Short on YouTube',
             'legacy peer-is-reach shortcut','{}',
             '{\"peer_audience\":[\"Misscore\"]}'::jsonb,
             'raised',now()+interval '7 days'
         )",
    )
    .bind(stale_task)
    .bind(beneficiary.into_uuid())
    .execute(&pool)
    .await?;

    // The visible task is not only the suggestion row: Autopilot already
    // materialised its approval action and handed it to a crew member. The
    // self-heal must retract that whole active chain while keeping the
    // immutable decision for audit.
    let stale_decision = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_decisions (
             id, workspace_id, decision_key, context, subject_kind, subject_id,
             decision_kind, confidence_basis_points, disposition, reason,
             input_snapshot, policy_snapshot, recommendation, trace_id
         ) VALUES (
             $1,$2,$3,'content_strategy','content_suggestion',$4,
             'raise_content_suggestion',9000,'require_approval',
             'legacy peer-is-reach shortcut','{}','{}','{}',$1
         )",
    )
    .bind(stale_decision)
    .bind(beneficiary.into_uuid())
    .bind(format!("legacy-peer-task-{stale_task}"))
    .bind(stale_task)
    .execute(&pool)
    .await?;
    let stale_action = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions (
             id, workspace_id, decision_id, context, action_kind,
             subject_kind, subject_id, idempotency_key, payload, status,
             approval_expires_at
         ) VALUES (
             $1,$2,$3,'content_strategy','content.suggestion.raise',
             'content_suggestion',$4,$5,$6,'awaiting_approval',
             now()+interval '7 days'
         )",
    )
    .bind(stale_action)
    .bind(beneficiary.into_uuid())
    .bind(stale_decision)
    .bind(stale_task)
    .bind(format!("legacy-peer-action-{stale_action}"))
    .bind(json!({
        "kind": "raise_content_suggestion",
        "suggestion_id": stale_task,
        "format_key": "peer_cover",
        "concept": "Promote Misscore Short on YouTube",
        "reason": "legacy peer-is-reach shortcut",
        "distribution_promise": {"peer_audience": ["Misscore"]}
    }))
    .execute(&pool)
    .await?;
    let stale_assignment = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO team_assignments (
             id, workspace_id, action_id, source_kind, source_id, source_ref,
             assignee_member_id, required_skill, status, due_at,
             next_reminder_at
         ) VALUES (
             $1,$2,$3,'autopilot_action',$4,NULL,$5,'video','open',
             now()+interval '7 days',now()+interval '1 day'
         )",
    )
    .bind(stale_assignment)
    .bind(beneficiary.into_uuid())
    .bind(stale_action)
    .bind(stale_task)
    .bind(member_id)
    .execute(&pool)
    .await?;

    let today = time::OffsetDateTime::now_utc().date();
    let without_consent = repo.refresh_suggestions(beneficiary, today).await?;
    assert!(
        without_consent.is_empty(),
        "a confirmed peer alone must not manufacture a distribution promise: {without_consent:?}"
    );
    let (stale_status, stale_reason): (String, Option<String>) = sqlx::query_as(
        "SELECT suggestion.status, outcome.reason
         FROM content_suggestions AS suggestion
         LEFT JOIN suggestion_outcomes AS outcome
           ON outcome.workspace_id=suggestion.workspace_id
          AND outcome.suggestion_id=suggestion.id
         WHERE suggestion.workspace_id=$1 AND suggestion.id=$2",
    )
    .bind(beneficiary.into_uuid())
    .bind(stale_task)
    .fetch_one(&pool)
    .await?;
    assert_eq!(stale_status, "expired");
    assert_eq!(
        stale_reason.as_deref(),
        Some("peer audience is no longer an executable consented route"),
        "the existing nonsense task must self-retire instead of staying in the operator queue"
    );
    let (action_status, action_error): (String, Option<String>) = sqlx::query_as(
        "SELECT status, last_error_kind
         FROM autopilot_actions
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(beneficiary.into_uuid())
    .bind(stale_action)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (action_status.as_str(), action_error.as_deref()),
        ("cancelled", Some("peer_reach_unavailable")),
        "the already-materialised approval task must disappear with its invalid subject"
    );
    let assignment_state: (String, Option<time::OffsetDateTime>) = sqlx::query_as(
        "SELECT status, next_reminder_at
         FROM team_assignments
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(beneficiary.into_uuid())
    .bind(stale_assignment)
    .fetch_one(&pool)
    .await?;
    assert_eq!(assignment_state.0, "cancelled");
    assert!(
        assignment_state.1.is_none(),
        "a cancelled nonsense task must not keep reminding a crew member"
    );
    let decision_still_there: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM autopilot_decisions
             WHERE workspace_id=$1 AND id=$2
         )",
    )
    .bind(beneficiary.into_uuid())
    .bind(stale_decision)
    .fetch_one(&pool)
    .await?;
    assert!(
        decision_still_there,
        "the bad decision stays as audit evidence; only active work is retracted"
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
    sqlx::query(
        "UPDATE workspaces SET organization_id=$3 WHERE id IN ($1,$2)",
    )
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
                    audience
                        .as_array()
                        .is_some_and(|items| {
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

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn suggestion_engine_raises_only_what_the_band_can_do()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let suffix = workspace_id.into_uuid().simple().to_string();

    // A roster covering every catalogue skill — capability is then decided
    // by material alone, which the fixtures supply.
    let member_id: Uuid = sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
         VALUES ($1, $2, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("crew-{suffix}@example.test"))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, 'crew', true,
                 ARRAY['general','operations','booking','approval','technical',
                       'visual','video','photography','social','english_copy',
                       'polish_copy','people']::text[])",
    )
    .bind(workspace_id.into_uuid())
    .bind(member_id)
    .execute(&pool)
    .await?;

    // Material: an upcoming release and a published show.
    sqlx::query(
        "INSERT INTO release_plans (workspace_id, source_key, title, release_at, active)
         VALUES ($1, $2, 'Sug Release', now() + interval '14 days', true)",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("sug-rel-{suffix}"))
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO events (workspace_id, slug, title, starts_at, status, published_at)
         VALUES ($1, $2, 'Sug Show', now() + interval '30 days', 'published', now())",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("sug-show-{suffix}"))
    .execute(&pool)
    .await?;

    // A scheduled shoot — the harvest rule should price covered formats
    // marginally.
    sqlx::query(
        "INSERT INTO production_events (id, workspace_id, kind, title, scheduled_for)
         VALUES ($1, $2, 'shoot', 'Video day', current_date + 5)",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    // Reach: one admitted community, one consented fan, one press route.
    sqlx::query(
        "INSERT INTO discovery_places (workspace_id, place_kind, platform, name, url, status)
         VALUES ($1, 'subreddit', 'reddit', 'r/Metal', $2, 'active')",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("https://reddit.com/r/metal-{suffix}"))
    .execute(&pool)
    .await?;
    let fan_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fans (workspace_id, normalized_email, status)
         VALUES ($1, $2, 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("fan-{suffix}@example.test"))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents (workspace_id, fan_id, purpose, granted, policy_version, source)
         VALUES ($1, $2, 'marketing', true, 'v1', 'test')",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO outreach_candidates
             (workspace_id, target_kind, display_name, source, source_reference,
              evidence, route_kind, route_value, route_is_published, status)
         VALUES ($1, 'press', 'Test Zine', 'operator_import', 'fixture',
                 'named in fixture', 'email', 'zine@example.test', true, 'admitted')",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;

    // First-party learning fixture: one mature playthrough has been live for
    // three weeks and forty distinct fans converted through posting actions
    // that name it. No suggestion outcome reports `new_fans` — the ranker
    // has to learn this yield from conversion provenance alone.
    let source_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO content_sources
             (id, workspace_id, source_kind, source_key, title, occurred_at,
              expires_at, format_key)
         VALUES ($1,$2,'video',$3,'Measured playthrough',
                 now() - interval '30 days', now() + interval '30 days',
                 'playthrough')",
    )
    .bind(source_id)
    .bind(workspace_id.into_uuid())
    .bind(format!("yield-playthrough-{suffix}"))
    .execute(&pool)
    .await?;

    let decision_id: Uuid = sqlx::query_scalar(
        "INSERT INTO autopilot_decisions
             (id, workspace_id, decision_key, context, subject_kind, subject_id,
              decision_kind, confidence_basis_points, disposition, reason,
              input_snapshot, policy_snapshot, recommendation, evaluated_at,
              trace_id)
         VALUES ($1,$2,$3,'content_supply','content_source',$4,
                 'seed.format_yield',9000,'auto_execute','yield fixture',
                 '{}','{}','{}',now(),$5)
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(format!("yield-decision-{suffix}"))
    .bind(source_id)
    .bind(Uuid::now_v7())
    .fetch_one(&pool)
    .await?;
    let action_id: Uuid = sqlx::query_scalar(
        "INSERT INTO autopilot_actions
             (id, workspace_id, decision_id, context, action_kind, subject_kind,
              subject_id, idempotency_key, payload, status, finished_at)
         VALUES ($1,$2,$3,'content_supply','community.engage.request',
                 'content_source',$4,$5,$6,'succeeded',
                 now() - interval '21 days')
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(decision_id)
    .bind(source_id)
    .bind(format!("yield-action-{suffix}"))
    .bind(json!({
        "kind": "request_community_engagement",
        "source_id": source_id.to_string(),
        "platform": "reddit",
        "title": "measured playthrough",
        "body": "fixture",
    }))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO community_posts
             (id, workspace_id, action_id, subreddit, title, body, status,
              posted_at)
         VALUES ($1,$2,$3,'r/Metal','measured playthrough','fixture','posted',
                 now() - interval '21 days')",
    )
    .bind(Uuid::now_v7())
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        WITH seeded AS (
            INSERT INTO fans (
                workspace_id, normalized_email, status, created_at, updated_at
            )
            SELECT $1,
                   'yield-' || $2 || '-' || n::text || '@example.test',
                   'active',
                   now() - interval '20 days',
                   now() - interval '20 days'
            FROM generate_series(1, 40) AS n
            RETURNING id
        )
        INSERT INTO fan_provenance_events (
            workspace_id, fan_id, event_kind, channel, source_target,
            action_id, attribution_method, attribution_confidence, occurred_at,
            format_key
        )
        SELECT $1, id, 'conversion', 'reddit', 'fixture',
               $3, 'last_tracked_click', 1.0,
               now() - interval '20 days', 'playthrough'
        FROM seeded
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(&suffix)
    .bind(action_id)
    .execute(&pool)
    .await?;

    let today = time::OffsetDateTime::now_utc().date();
    let raised = repo.refresh_suggestions(workspace_id, today).await?;
    assert!(
        !raised.is_empty() && raised.len() <= 3,
        "the engine ranks then cuts to the vital few: {}",
        raised.len()
    );
    let playthrough = raised
        .iter()
        .find(|suggestion| suggestion.format_key.as_deref() == Some("playthrough"))
        .expect(
            "observed first-party fan yield should lift the mature playthrough into the vital few",
        );
    let learned_multiplier = playthrough.evidence["format_yield"]
        .as_f64()
        .expect("format-yield multiplier is auditable evidence");
    assert!(
        learned_multiplier > 1.0,
        "forty observed fans must lift the playthrough above its prior: {learned_multiplier}"
    );

    for suggestion in &raised {
        assert_eq!(suggestion.status.as_str(), "raised");
        assert!(
            !crowdrelay_domain::content_engine::distribution_promise_is_empty(
                &suggestion.distribution_promise
            ),
            "an empty promise must never reach the table: {}",
            suggestion.format_key.as_deref().unwrap_or("?")
        );
        assert!(
            suggestion.reason.contains("communities") || suggestion.reason.contains("fans"),
            "the reason names what it reaches: {}",
            suggestion.reason
        );
    }
    let has_community_clause = raised.iter().any(|s| {
        s.distribution_promise
            .get("communities")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
    });
    let has_fan_clause = raised
        .iter()
        .any(|s| s.distribution_promise.get("consented_fans").is_some());
    assert!(
        has_community_clause && has_fan_clause,
        "the promise names the real surfaces"
    );

    // Idempotent: open suggestions suppress re-raising the same format —
    // a second pass has nothing new to say.
    let again = repo.refresh_suggestions(workspace_id, today).await?;
    assert!(
        again.is_empty(),
        "open suggestions must not be re-raised, got {}",
        again.len()
    );

    // The video gap end to end: a workspace whose roster holds only
    // `social` never receives a video format.
    let other = WorkspaceId::new();
    seed_workspace(&pool, other).await?;
    let osuffix = other.into_uuid().simple().to_string();
    let omember: Uuid = sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
         VALUES ($1, $2, 'staff', 'active') RETURNING id",
    )
    .bind(other.into_uuid())
    .bind(format!("solo-{osuffix}@example.test"))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, 'solo', true, ARRAY['social']::text[])",
    )
    .bind(other.into_uuid())
    .bind(omember)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO discovery_places (workspace_id, place_kind, platform, name, url, status)
         VALUES ($1, 'subreddit', 'reddit', 'r/Metal', $2, 'active')",
    )
    .bind(other.into_uuid())
    .bind(format!("https://reddit.com/r/metal-{osuffix}"))
    .execute(&pool)
    .await?;
    let theirs = repo.refresh_suggestions(other, today).await?;
    const VIDEO_KEYS: &[&str] = &[
        "playthrough",
        "official_video",
        "making_of",
        "peer_cover",
        "live_session",
        "soundcheck_clip",
        "aftermovie",
        "gear_rundown",
        "rehearsal_clip",
        "old_material_reaction",
    ];
    for suggestion in &theirs {
        let key = suggestion.format_key.as_deref().unwrap_or_default();
        assert!(
            !VIDEO_KEYS.contains(&key),
            "a video suggestion reached a band with no filmmaker: {key}"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_lapsed_suggestion_expires_and_frees_its_queue_slot()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let workspace_id = WorkspaceId::new();
    seed_workspace(&pool, workspace_id).await?;
    let suffix = workspace_id.into_uuid().simple().to_string();
    let member_id: Uuid = sqlx::query_scalar(
        "INSERT INTO workspace_members (workspace_id, normalized_email, role, status)
         VALUES ($1, $2, 'staff', 'active') RETURNING id",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("lapse-{suffix}@example.test"))
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO team_profiles
             (workspace_id, member_id, member_key, active, skills)
         VALUES ($1, $2, 'crew', true, ARRAY['social']::text[])",
    )
    .bind(workspace_id.into_uuid())
    .bind(member_id)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO discovery_places (workspace_id, place_kind, platform, name, url, status)
         VALUES ($1, 'subreddit', 'reddit', 'r/Metal', $2, 'active')",
    )
    .bind(workspace_id.into_uuid())
    .bind(format!("https://reddit.com/r/metal-{suffix}"))
    .execute(&pool)
    .await?;

    // Three lapsed raised rows would fill the open queue forever: the
    // evaluator skips lapsed rows, nothing else resolves them, and headroom
    // never returns.
    for ordinal in 0..3 {
        sqlx::query(
            "INSERT INTO content_suggestions (
                 id, workspace_id, format_key, concept, reason, evidence,
                 distribution_promise, status, expires_at
             ) VALUES ($1,$2,'playthrough',$3,'stale ask','{}',
                       '{\"consented_fans\":4}','raised', now() - interval '2 days')",
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id.into_uuid())
        .bind(format!("Lapsed {ordinal}"))
        .execute(&pool)
        .await?;
    }

    let today = time::OffsetDateTime::now_utc().date();
    let raised = repo.refresh_suggestions(workspace_id, today).await?;

    let (expired, outcomes): (i64, i64) = sqlx::query_as(
        "SELECT
             (SELECT count(*) FROM content_suggestions
               WHERE workspace_id = $1 AND status = 'expired'),
             (SELECT count(*) FROM suggestion_outcomes
               WHERE workspace_id = $1 AND outcome = 'expired')",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (expired, outcomes),
        (3, 3),
        "every lapsed ask resolves with its outcome — the queue cannot leak"
    );
    assert!(
        !raised.is_empty(),
        "headroom returns the same pass — the engine must not stall on ghosts"
    );
    Ok(())
}
