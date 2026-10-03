use super::*;

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn organic_funnel_requires_deliberate_action_current_consent_and_unambiguous_credit() {
    let f = setup().await.expect("fixture");
    let posted = f.now - time::Duration::days(10);
    let action = insert_dispatch(&f, "organic-funnel", posted).await;
    let link = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO smart_links(id,workspace_id,slug,destination_url,channel_source) VALUES($1,$2,'organic-funnel','https://example.test/signal','telegram')")
        .bind(link).bind(f.workspace_id.into_uuid()).execute(&f.pool).await.expect("link");
    live_post(&f, action, "organic-funnel", posted).await;
    let visitor = uuid::Uuid::now_v7();
    for _ in 0..2 {
        sqlx::query("INSERT INTO click_events(workspace_id,smart_link_id,anonymous_visitor_id,occurred_at) VALUES($1,$2,$3,$4)")
        .bind(f.workspace_id.into_uuid()).bind(link).bind(visitor).bind(posted+time::Duration::hours(1)).execute(&f.pool).await.expect("click");
    }
    let acquired = posted + time::Duration::hours(2);
    let fan = converted_fan(&f, action, acquired, acquired, "active").await;
    marketing_consent(&f, fan, true, acquired).await;
    sqlx::query("UPDATE fan_provenance_events SET source_target='organic-funnel' WHERE workspace_id=$1 AND fan_id=$2")
        .bind(f.workspace_id.into_uuid()).bind(fan).execute(&f.pool).await.expect("credit");
    meaningful_session(&f, fan, acquired + time::Duration::hours(1)).await;
    let read = || {
        crowdrelay_infra::organic_funnel::read(
            &f.pool,
            f.workspace_id.into_uuid(),
            Some(action),
            None,
            90,
            100,
            f.now,
        )
    };
    let rows = read().await.expect("funnel");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].unique_visitors,
            rows[0].signups,
            rows[0].confirmed,
            rows[0].activated
        ),
        (1, 1, 1, 0)
    );
    assert_eq!(rows[0].activation_mature, 1);
    assert_eq!(rows[0].retention_mature, 0);
    let event = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO events(id,workspace_id,slug,title,starts_at,status,published_at) VALUES($1,$2,$3,'Real show',$4,'published',$5)")
        .bind(event).bind(f.workspace_id.into_uuid()).bind(format!("funnel-{}",event.simple())).bind(f.now+time::Duration::days(10)).bind(posted).execute(&f.pool).await.expect("event");
    sqlx::query(
        "INSERT INTO event_interests(workspace_id,event_id,fan_id,created_at) VALUES($1,$2,$3,$4)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(event)
    .bind(fan)
    .bind(acquired + time::Duration::days(1))
    .execute(&f.pool)
    .await
    .expect("interest");
    assert_eq!(read().await.expect("activation")[0].activated_mature, 1);
    marketing_consent(&f, fan, false, f.now - time::Duration::hours(1)).await;
    let rows = read().await.expect("withdrawal");
    assert_eq!(rows[0].signups, 1);
    assert_eq!((rows[0].confirmed, rows[0].activated), (0, 0));
    assert!(
        crowdrelay_infra::organic_funnel::read(
            &f.pool,
            uuid::Uuid::now_v7(),
            Some(action),
            None,
            90,
            100,
            f.now
        )
        .await
        .expect("foreign")
        .is_empty()
    );
    let other = insert_dispatch(&f, "shared-link-owner", posted + time::Duration::days(1)).await;
    live_post(
        &f,
        other,
        "organic-funnel",
        posted + time::Duration::days(1),
    )
    .await;
    let rows = read().await.expect("ambiguous");
    assert!(rows[0].ambiguous_owner);
    assert_eq!(rows[0].signups, 0);
    assert_eq!(rows[0].diagnosis, "ambiguous_link_owner");
}


#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn organic_funnel_control_moves_the_autopilot_to_the_first_real_leak() {
    use crowdrelay_application::autopilot::OrganicFunnelDirective;

    let f = setup().await.expect("fixture");
    let posted = f.now - time::Duration::days(10);
    let action = insert_dispatch(&f, "organic-control", posted).await;
    live_post(&f, action, "organic-control", posted).await;

    let control = || {
        crowdrelay_infra::organic_funnel::control(
            &f.pool,
            f.workspace_id.into_uuid(),
            f.now,
        )
    };

    assert_eq!(
        control().await.expect("control").map(|c| c.directive),
        Some(OrganicFunnelDirective::ExpandReach)
    );

    let link: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM smart_links WHERE workspace_id=$1 AND slug='organic-control'",
    )
    .bind(f.workspace_id.into_uuid())
    .fetch_one(&f.pool)
    .await
    .expect("link");
    sqlx::query(
        "INSERT INTO click_events(workspace_id,smart_link_id,anonymous_visitor_id,occurred_at)
         VALUES($1,$2,$3,$4)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(link)
    .bind(uuid::Uuid::now_v7())
    .bind(posted + time::Duration::hours(1))
    .execute(&f.pool)
    .await
    .expect("click");

    assert_eq!(
        control().await.expect("control").map(|c| c.directive),
        Some(OrganicFunnelDirective::RepairConversion)
    );

    let acquired = posted + time::Duration::hours(2);
    let fan = converted_fan(&f, action, acquired, acquired, "pending").await;
    sqlx::query(
        "UPDATE fan_provenance_events
         SET source_target='organic-control'
         WHERE workspace_id=$1 AND fan_id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(fan)
    .execute(&f.pool)
    .await
    .expect("credit");

    assert_eq!(
        control().await.expect("control").map(|c| c.directive),
        Some(OrganicFunnelDirective::RepairConfirmation)
    );

    sqlx::query("UPDATE fans SET status='active' WHERE workspace_id=$1 AND id=$2")
        .bind(f.workspace_id.into_uuid())
        .bind(fan)
        .execute(&f.pool)
        .await
        .expect("activate account");
    marketing_consent(&f, fan, true, acquired).await;

    assert_eq!(
        control().await.expect("control").map(|c| c.directive),
        Some(OrganicFunnelDirective::ActivateFans)
    );

    let activation_event = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events(id,workspace_id,slug,title,starts_at,status,published_at)
         VALUES($1,$2,$3,'Activation show',$4,'published',$5)",
    )
    .bind(activation_event)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("activation-{}", activation_event.simple()))
    .bind(acquired + time::Duration::days(10))
    .bind(posted)
    .execute(&f.pool)
    .await
    .expect("activation event");
    sqlx::query(
        "INSERT INTO event_interests(workspace_id,event_id,fan_id,created_at)
         VALUES($1,$2,$3,$4)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(activation_event)
    .bind(fan)
    .bind(acquired + time::Duration::days(1))
    .execute(&f.pool)
    .await
    .expect("activation interest");

    assert_eq!(
        control().await.expect("control"),
        None,
        "a ten-day cohort has passed activation but is not mature enough for retention"
    );

    // Add a separate D30-mature acquisition. Old history must inform retention
    // without being allowed to hide the fresh top-of-funnel checks above.
    let old_posted = f.now - time::Duration::days(45);
    let old_action = insert_dispatch(&f, "organic-control-old", old_posted).await;
    live_post(&f, old_action, "organic-control-old", old_posted).await;
    let old_acquired = old_posted + time::Duration::hours(2);
    let old_fan = converted_fan(&f, old_action, old_acquired, old_acquired, "active").await;
    marketing_consent(&f, old_fan, true, old_acquired).await;
    sqlx::query(
        "UPDATE fan_provenance_events
         SET source_target='organic-control-old'
         WHERE workspace_id=$1 AND fan_id=$2",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(old_fan)
    .execute(&f.pool)
    .await
    .expect("old credit");

    let old_activation_event = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events(id,workspace_id,slug,title,starts_at,status,published_at)
         VALUES($1,$2,$3,'Old activation show',$4,'published',$5)",
    )
    .bind(old_activation_event)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("old-activation-{}", old_activation_event.simple()))
    .bind(old_acquired + time::Duration::days(10))
    .bind(old_posted)
    .execute(&f.pool)
    .await
    .expect("old activation event");
    sqlx::query(
        "INSERT INTO event_interests(workspace_id,event_id,fan_id,created_at)
         VALUES($1,$2,$3,$4)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(old_activation_event)
    .bind(old_fan)
    .bind(old_acquired + time::Duration::days(1))
    .execute(&f.pool)
    .await
    .expect("old activation interest");

    assert_eq!(
        control().await.expect("control").map(|c| c.directive),
        Some(OrganicFunnelDirective::RetainFans)
    );

    let return_event = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO events(id,workspace_id,slug,title,starts_at,status,published_at)
         VALUES($1,$2,$3,'Return show',$4,'published',$5)",
    )
    .bind(return_event)
    .bind(f.workspace_id.into_uuid())
    .bind(format!("return-{}", return_event.simple()))
    .bind(f.now + time::Duration::days(10))
    .bind(f.now - time::Duration::days(2))
    .execute(&f.pool)
    .await
    .expect("return event");
    sqlx::query(
        "INSERT INTO event_interests(workspace_id,event_id,fan_id,created_at)
         VALUES($1,$2,$3,$4)",
    )
    .bind(f.workspace_id.into_uuid())
    .bind(return_event)
    .bind(old_fan)
    .bind(f.now - time::Duration::days(1))
    .execute(&f.pool)
    .await
    .expect("return interest");
    assert_eq!(control().await.expect("control"), None);
}
