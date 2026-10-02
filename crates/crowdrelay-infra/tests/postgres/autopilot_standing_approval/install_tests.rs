use super::*;
use crowdrelay_application::RepositoryError;
use crowdrelay_application::autopilot::AutopilotActionRepository;
use crowdrelay_domain::FanId;

const INSTALL_TEMPLATE: &str = "crowdrelay.fan.signal_install_ask.v1";
const INSTALL_TARGET: &str = "template:crowdrelay.fan.signal_install_ask.v1";

async fn install_fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let f = fixture(label).await?;
    let configured = sqlx::query(
        "UPDATE autopilot_policies SET enabled=true, autonomy_level='require_approval' WHERE workspace_id=$1 AND context='fan_lifecycle'",
    )
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await?;
    assert_eq!(configured.rows_affected(), 1);
    sqlx::query(
        "INSERT INTO tenant_settings(workspace_id,key,value) VALUES($1,'member_site_base_url','https://fans.example.test') ON CONFLICT(workspace_id,key) DO UPDATE SET value=EXCLUDED.value",
    )
    .bind(f.workspace_id.into_uuid())
    .execute(&f.pool)
    .await?;
    grant(
        &f.pool,
        f.workspace_id.into_uuid(),
        GrantRequest {
            action_kind: "fan.lifecycle.message.request",
            target_key: INSTALL_TARGET,
            class: ActionClass::OwnedAudience,
            granted_by: "operator:test",
            days: 30,
            note: None,
        },
        OffsetDateTime::now_utc(),
    )
    .await?;
    Ok(f)
}

async fn consented_fan(f: &Fixture) -> Result<FanId, Box<dyn std::error::Error>> {
    let fan = FanId::new();
    sqlx::query(
        "INSERT INTO fans(id,workspace_id,normalized_email,status) VALUES($1,$2,$3,'active')",
    )
    .bind(fan.into_uuid())
    .bind(f.workspace_id.into_uuid())
    .bind(format!("{fan}@example.test"))
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO fan_consents(workspace_id,fan_id,purpose,granted,policy_version,source) VALUES($1,$2,'marketing',true,'v1','install-test')",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(fan.into_uuid())
    .execute(&f.pool)
    .await?;
    Ok(fan)
}

async fn emission_count(f: &Fixture) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM outbox_events WHERE workspace_id=$1 AND event_type='crowdrelay.fan_lifecycle.message_requested'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
}

fn install_candidate(fan: FanId, key: &str) -> DecisionCandidate {
    let mut candidate = outreach_candidate(OutreachTargetId::new(), Uuid::now_v7());
    candidate.context = AutopilotContext::FanLifecycle;
    candidate.subject = ActionSubject::Fan(fan);
    candidate.decision_kind = "request_lifecycle_message";
    candidate.action = AutopilotActionPayload::RequestFanLifecycleMessage {
        fan_id: fan,
        template_key: key.to_owned(),
        show: None,
    };
    candidate
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn install_template_grant_covers_multiple_fans_but_revocation_prevents_emission()
-> Result<(), Box<dyn std::error::Error>> {
    let f = install_fixture("install-grant").await?;
    for _ in 0..2 {
        let fan = consented_fan(&f).await?;
        let candidate = install_candidate(fan, "crowdrelay.fan.signal_install_ask.v1");
        assert!(persist(&f, f.workspace_id, &candidate).await?);
        let state = action_state(&f, f.workspace_id, &candidate.action_idempotency_key)
            .await?
            .expect("action");
        assert_eq!(state.0, "queued");
        assert_eq!(state.1.as_deref(), Some("operator:standing_grant"));
        let future_fan = FanId::new();
        sqlx::query(
            "INSERT INTO fans(id,workspace_id,normalized_email,status) VALUES($1,$2,$3,'active')",
        )
        .bind(future_fan.into_uuid())
        .bind(f.workspace_id.into_uuid())
        .bind(format!("{future_fan}@example.test"))
        .execute(&f.pool)
        .await?;
        let future = install_candidate(future_fan, "crowdrelay.fan.signal_install_ask.v2");
        assert!(persist(&f, f.workspace_id, &future).await?);
        assert_eq!(
            action_state(&f, f.workspace_id, &future.action_idempotency_key)
                .await?
                .expect("future approval")
                .0,
            "awaiting_approval"
        );
    }
    let claims = f
        .repository
        .claim_due_actions(f.workspace_id, 10, OffsetDateTime::now_utc())
        .await?;
    assert_eq!(claims.len(), 2);
    revoke(
        &f.pool,
        f.workspace_id.into_uuid(),
        "fan.lifecycle.message.request",
        INSTALL_TARGET,
        "operator:test",
        OffsetDateTime::now_utc(),
    )
    .await?;
    for action in claims {
        assert!(matches!(
            f.repository
                .execute_action(f.workspace_id, &action, OffsetDateTime::now_utc())
                .await,
            Err(RepositoryError::ConflictBecause(
                "signal install standing approval expired or revoked"
            ))
        ));
    }
    assert_eq!(emission_count(&f).await?, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn live_install_grant_emits_one_message_with_an_action_owned_link()
-> Result<(), Box<dyn std::error::Error>> {
    let f = install_fixture("install-live").await?;
    let fan = consented_fan(&f).await?;
    let candidate = install_candidate(fan, INSTALL_TEMPLATE);
    assert!(persist(&f, f.workspace_id, &candidate).await?);
    let claims = f
        .repository
        .claim_due_actions(f.workspace_id, 10, OffsetDateTime::now_utc())
        .await?;
    assert_eq!(claims.len(), 1);
    let action = &claims[0];
    f.repository
        .execute_action(f.workspace_id, action, OffsetDateTime::now_utc())
        .await?;
    assert_eq!(emission_count(&f).await?, 1);
    let (owner, destination, url) = sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT link.action_id, link.destination_url, event.payload->'fan'->>'install_url' FROM smart_links link JOIN outbox_events event ON event.workspace_id=link.workspace_id AND event.action_id=link.action_id WHERE link.workspace_id=$1 AND link.action_id=$2 AND event.event_type='crowdrelay.fan_lifecycle.message_requested'",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(action.id.into_uuid())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(owner, action.id.into_uuid());
    assert!(destination.starts_with("https://fans.example.test/"));
    assert_eq!(
        url,
        format!(
            "https://fans.example.test/l/signal-install-{}",
            action.id.into_uuid().simple()
        )
    );
    // Re-running the finished claim must not mint another outward message.
    assert!(
        f.repository
            .execute_action(f.workspace_id, action, OffsetDateTime::now_utc())
            .await
            .is_err()
    );
    assert_eq!(emission_count(&f).await?, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn install_dispatch_rechecks_expiry_policy_installation_and_consent()
-> Result<(), Box<dyn std::error::Error>> {
    for change in [
        "expired",
        "observe",
        "recommend",
        "disabled",
        "installed",
        "withdrawn",
    ] {
        let f = install_fixture(change).await?;
        let fan = consented_fan(&f).await?;
        let candidate = install_candidate(fan, INSTALL_TEMPLATE);
        assert!(persist(&f, f.workspace_id, &candidate).await?);
        let claims = f
            .repository
            .claim_due_actions(f.workspace_id, 10, OffsetDateTime::now_utc())
            .await?;
        assert_eq!(claims.len(), 1);
        // The change happens AFTER evaluation and claim, before emission.
        let expected = match change {
            "expired" => {
                sqlx::query("UPDATE standing_approvals SET granted_at=now()-interval '40 days', expires_at=now()-interval '1 hour' WHERE workspace_id=$1 AND target_key=$2")
                    .bind(f.workspace_id.into_uuid()).bind(INSTALL_TARGET).execute(&f.pool).await?;
                RepositoryError::ConflictBecause(
                    "signal install standing approval expired or revoked",
                )
            }
            "observe" | "recommend" | "disabled" => {
                sqlx::query("UPDATE autopilot_policies SET autonomy_level=$2, enabled=$3 WHERE workspace_id=$1 AND context='fan_lifecycle'")
                    .bind(f.workspace_id.into_uuid())
                    .bind(if change == "disabled" { "require_approval" } else { change })
                    .bind(change != "disabled")
                    .execute(&f.pool).await?;
                RepositoryError::ConflictBecause(
                    "fan lifecycle policy no longer permits template approval",
                )
            }
            "installed" => {
                sqlx::query("INSERT INTO signal_installations(workspace_id,installation_id,platform,fan_id) VALUES($1,$2,'web',$3)")
                    .bind(f.workspace_id.into_uuid()).bind(Uuid::now_v7().to_string()).bind(fan.into_uuid()).execute(&f.pool).await?;
                RepositoryError::ConflictBecause("signal install ask no longer needed")
            }
            "withdrawn" => {
                sqlx::query("INSERT INTO fan_consents(workspace_id,fan_id,purpose,granted,policy_version,source,recorded_at) VALUES($1,$2,'marketing',false,'v1','install-test',now()+interval '1 second')")
                    .bind(f.workspace_id.into_uuid()).bind(fan.into_uuid()).execute(&f.pool).await?;
                RepositoryError::Conflict
            }
            _ => unreachable!(),
        };
        let result = f
            .repository
            .execute_action(f.workspace_id, &claims[0], OffsetDateTime::now_utc())
            .await;
        assert_eq!(result, Err(expected), "dispatch guard: {change}");
        assert_eq!(
            emission_count(&f).await?,
            0,
            "no outward emission after {change}"
        );
        let links: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM smart_links WHERE workspace_id=$1")
                .bind(f.workspace_id.into_uuid())
                .fetch_one(&f.pool)
                .await?;
        assert_eq!(links, 0, "refusal rolls back tracked link after {change}");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn show_recalls_own_their_links_without_inheriting_install_template_approval()
-> Result<(), Box<dyn std::error::Error>> {
    let f = install_fixture("recall-owner").await?;
    let mut expected_ids = Vec::new();
    for _ in 0..2 {
        let fan = consented_fan(&f).await?;
        let mut candidate = install_candidate(fan, "crowdrelay.fan.show_recall.v1");
        candidate.action = AutopilotActionPayload::RequestFanLifecycleMessage {
            fan_id: fan,
            template_key: "crowdrelay.fan.show_recall.v1".to_owned(),
            show: Some(crowdrelay_application::autopilot::LifecycleShowContext {
                event_slug: "same-show".to_owned(),
                event_title: "Same show".to_owned(),
                wants_install_url: true,
            }),
        };
        assert!(persist(&f, f.workspace_id, &candidate).await?);
        assert_eq!(
            action_state(&f, f.workspace_id, &candidate.action_idempotency_key)
                .await?
                .expect("recall approval")
                .0,
            "awaiting_approval"
        );
        // Fixture-only explicit approval: the install grant cannot answer a recall.
        let id: Uuid = sqlx::query_scalar("UPDATE autopilot_actions SET status='queued', approved_by='operator:test', approved_at=now() WHERE workspace_id=$1 AND idempotency_key=$2 RETURNING id")
            .bind(f.workspace_id.into_uuid()).bind(&candidate.action_idempotency_key).fetch_one(&f.pool).await?;
        expected_ids.push(id);
    }
    let claims = f
        .repository
        .claim_due_actions(f.workspace_id, 10, OffsetDateTime::now_utc())
        .await?;
    assert_eq!(claims.len(), 2);
    let installed_fan = match &claims[0].payload {
        AutopilotActionPayload::RequestFanLifecycleMessage { fan_id, .. } => *fan_id,
        _ => panic!("recall claim"),
    };
    // A web install can land between proposal/claim and the T+1 send. Keep
    // the thank-you, but drop its stale installation CTA for this person.
    sqlx::query("INSERT INTO signal_installations(workspace_id,installation_id,platform,fan_id) VALUES($1,$2,'web',$3)")
        .bind(f.workspace_id.into_uuid()).bind(Uuid::now_v7().to_string()).bind(installed_fan.into_uuid()).execute(&f.pool).await?;
    for action in &claims {
        f.repository
            .execute_action(f.workspace_id, action, OffsetDateTime::now_utc())
            .await?;
    }
    assert_eq!(emission_count(&f).await?, 2);
    let recalls = sqlx::query_as::<_, (Uuid, String, String)>("SELECT link.action_id, link.slug, link.destination_url FROM smart_links link WHERE workspace_id=$1 AND slug LIKE 'show-recall-%' ORDER BY slug")
        .bind(f.workspace_id.into_uuid()).fetch_all(&f.pool).await?;
    assert_eq!(recalls.len(), 2);
    assert_ne!(recalls[0].1, recalls[1].1);
    for (owner, slug, destination) in recalls {
        assert!(expected_ids.contains(&owner));
        assert_eq!(slug, format!("show-recall-{}", owner.simple()));
        assert!(destination.ends_with("/live/same-show/"));
        let url: String = sqlx::query_scalar("SELECT payload->'fan'->>'show_url' FROM outbox_events WHERE workspace_id=$1 AND action_id=$2 AND event_type='crowdrelay.fan_lifecycle.message_requested'")
            .bind(f.workspace_id.into_uuid()).bind(owner).fetch_one(&f.pool).await?;
        assert_eq!(url, format!("https://fans.example.test/l/{slug}"));
    }
    let ctas = sqlx::query_as::<_, (Uuid, Option<String>)>("SELECT (payload->>'fan_id')::uuid, payload->'fan'->>'install_url' FROM outbox_events WHERE workspace_id=$1 AND event_type='crowdrelay.fan_lifecycle.message_requested'")
        .bind(f.workspace_id.into_uuid()).fetch_all(&f.pool).await?;
    assert_eq!(ctas.len(), 2);
    for (fan, url) in ctas {
        assert_eq!(url.is_some(), fan != installed_fan.into_uuid());
    }
    Ok(())
}
