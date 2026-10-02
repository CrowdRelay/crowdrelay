//! FAN SCOUT next-best-action decisions against the real prospect schema.

use crate::common;

use crowdrelay_domain::fan_prospect::{ObservationKind, ProspectSource};
use crowdrelay_infra::fan_prospects::{ObservedPerson, next_actions, observe};
use time::OffsetDateTime;
use uuid::Uuid;

async fn workspace(
    pool: &sqlx::PgPool,
    label: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1,$2,$3)")
        .bind(id)
        .bind(format!("{label}-{}", id.simple()))
        .bind(label)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn seen(
    pool: &sqlx::PgPool,
    ws: Uuid,
    handle: &str,
    kind: ObservationKind,
    source_ref: &str,
) -> Result<Uuid, Box<dyn std::error::Error>> {
    let at = OffsetDateTime::now_utc();
    let outcome = observe(
        pool,
        ws,
        &ObservedPerson {
            source: ProspectSource::OwnComments,
            platform: "instagram",
            platform_user_id: None,
            handle: Some(handle),
            display_identity: handle,
            display_name: None,
            profile_url: None,
            kind,
            source_ref,
            source_url: Some("https://example.test/post"),
            observed_at: at,
            evidence: "kiedy gracie / jak was śledzić?",
            confidence_basis_points: 8_000,
        },
    )
    .await?;
    Ok(match outcome {
        crowdrelay_infra::fan_prospects::ObserveOutcome::Created { prospect_id }
        | crowdrelay_infra::fan_prospects::ObserveOutcome::Known { prospect_id, .. }
        | crowdrelay_infra::fan_prospects::ObserveOutcome::NotCollected { prospect_id } => {
            prospect_id
        }
        other => return Err(format!("prospect not recorded: {other:?}").into()),
    })
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn warm_people_are_engaged_but_only_explicit_join_intent_is_invited()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "fan-prospect-actions").await?;
    sqlx::query(
        "INSERT INTO tenant_settings(workspace_id,key,value)
         VALUES($1,'member_site_base_url','https://fans.example.test')",
    )
    .bind(ws)
    .execute(&pool)
    .await?;

    let warm = seen(
        &pool,
        ws,
        "warm_fan",
        ObservationKind::ActiveUnderOurPost,
        "warm-comment",
    )
    .await?;
    let joining = seen(
        &pool,
        ws,
        "joining_fan",
        ObservationKind::AskedToJoinOrFollow,
        "join-comment",
    )
    .await?;
    let refused = seen(
        &pool,
        ws,
        "no_thanks",
        ObservationKind::AskedToJoinOrFollow,
        "refused-comment",
    )
    .await?;
    sqlx::query(
        "UPDATE fan_prospects
         SET status='refused', status_reason='asked us not to contact them'
         WHERE workspace_id=$1 AND id=$2",
    )
    .bind(ws)
    .bind(refused)
    .execute(&pool)
    .await?;

    let queue = next_actions(&pool, ws).await?;
    let action = |id: Uuid| {
        queue
            .iter()
            .find(|item| item.prospect_id == id)
            .unwrap_or_else(|| panic!("missing prospect {id}"))
    };

    assert_eq!(
        format!("{:?}", action(joining).action),
        "InviteToFanbase",
        "explicit join intent should be first-party invite eligible"
    );
    assert_eq!(
        format!("{:?}", action(warm).action),
        "EngageInContext",
        "warmth alone must not become an invitation"
    );
    assert_eq!(
        format!("{:?}", action(refused).action),
        "DoNotContact",
        "a refusal beats explicit growth intent"
    );
    assert_eq!(
        queue.first().map(|item| item.prospect_id),
        Some(joining),
        "invite-ready relationship should rank above engagement"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn explicit_join_intent_holds_when_the_tenant_has_no_owned_destination()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    let ws = workspace(&pool, "fan-prospect-actions-no-site").await?;
    let joining = seen(
        &pool,
        ws,
        "joining_fan",
        ObservationKind::AskedToJoinOrFollow,
        "join-comment",
    )
    .await?;

    let queue = next_actions(&pool, ws).await?;
    let item = queue
        .iter()
        .find(|item| item.prospect_id == joining)
        .ok_or("missing prospect")?;
    assert_eq!(format!("{:?}", item.action), "Hold");
    assert!(
        item.reason.contains("no configured first-party member site"),
        "{}",
        item.reason
    );
    Ok(())
}
