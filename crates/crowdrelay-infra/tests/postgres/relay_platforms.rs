#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database"]
async fn reddit_backlog_does_not_starve_joined_forums_or_discord()
-> Result<(), Box<dyn std::error::Error>> {
    let (repo, pool) = repository().await?;
    let ws = WorkspaceId::new();
    seed_workspace(&pool, ws).await?;
    for name in ["heldone", "heldtwo"] {
        let target = seed_community(&pool, ws, name, None).await?;
        seed_post(&pool, ws, target, name, "awaiting_manual_post", 1).await?;
    }
    let mut eligible = Vec::new();
    for (name, platform, membership, ratio) in [
        ("readyforum", "forum", "joined", 100_i16),
        ("joineddiscord", "discord", "joined", 100),
        ("unjoinedlemmy", "lemmy", "not_joined", 100),
        ("unfittelegram", "telegram", "not_a_fit", 100),
        ("nopromotion", "forum", "joined", 0),
    ] {
        let target = seed_community(&pool, ws, name, Some(("active", membership))).await?;
        sqlx::query("UPDATE agent_outreach_targets SET platform=$3,subreddit=NULL,community_url=$4 WHERE workspace_id=$1 AND id=$2")
            .bind(ws.into_uuid()).bind(target).bind(platform).bind(format!("https://{name}.example/community")).execute(&pool).await?;
        sqlx::query("UPDATE discovery_places SET platform=$3,place_kind='forum' WHERE workspace_id=$1 AND id=(SELECT place_id FROM agent_outreach_targets WHERE workspace_id=$1 AND id=$2)")
            .bind(ws.into_uuid()).bind(target).bind(platform).execute(&pool).await?;
        sqlx::query("INSERT INTO discovery_place_rules(place_id,self_promo_ratio_percent) SELECT place_id,$3 FROM agent_outreach_targets WHERE workspace_id=$1 AND id=$2")
            .bind(ws.into_uuid()).bind(target).bind(ratio).execute(&pool).await?;
        if matches!(platform, "forum" | "discord") && ratio > 0 {
            eligible.push(target)
        }
    }
    let targets = repo.load_relay_community_targets(ws).await?;
    assert_eq!(targets.len(), 2);
    for target in targets {
        assert!(eligible.contains(&target.target_id.into_uuid()));
        assert!(matches!(target.platform.as_str(), "forum" | "discord"));
        assert!(target.community_url.is_some());
    }
    Ok(())
}
