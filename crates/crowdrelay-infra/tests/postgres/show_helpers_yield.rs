// A previous community touch is traffic, not a second acquired fan.

async fn seed_earlier_community_touch(
    pool: &PgPool,
    workspace_id: Uuid,
    decision_id: Uuid,
    visitor_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    let target_id = community(pool, workspace_id, "PL", "r/earlier-touch", true).await?;
    let action_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO autopilot_actions
            (id, workspace_id, decision_id, context, action_kind, subject_kind,
             subject_id, idempotency_key, payload, status, finished_at)
         VALUES ($1,$2,$3,'growth_intelligence','community.engage.request',
                 'target_community',$4,$5,'{}','succeeded',now())",
    )
    .bind(action_id)
    .bind(workspace_id)
    .bind(decision_id)
    .bind(target_id)
    .bind(format!("earlier-touch-{action_id}"))
    .execute(pool)
    .await?;
    let link_id = Uuid::now_v7();
    let slug = format!("earlier-touch-{}", link_id.simple());
    sqlx::query(
        "INSERT INTO smart_links (id, workspace_id, slug, destination_url)
         VALUES ($1,$2,$3,'https://example.test/fan')",
    )
    .bind(link_id)
    .bind(workspace_id)
    .bind(&slug)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO community_posts
            (workspace_id, action_id, target_id, subreddit, title, body,
             smart_link, status, posted_at)
         VALUES ($1,$2,$3,'r/earlier-touch','show','body',$4,'posted',
                 now() - interval '52 days')",
    )
    .bind(workspace_id)
    .bind(action_id)
    .bind(target_id)
    .bind(format!("/l/{slug}"))
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO click_events
            (workspace_id, smart_link_id, anonymous_visitor_id, occurred_at)
         VALUES ($1,$2,$3,now() - interval '51 days')",
    )
    .bind(workspace_id)
    .bind(link_id)
    .bind(visitor_id)
    .execute(pool)
    .await?;
    Ok(())
}
