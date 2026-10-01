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
