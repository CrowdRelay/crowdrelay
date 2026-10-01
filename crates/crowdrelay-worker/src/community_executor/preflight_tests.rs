#[cfg(test)]
mod promotion_preflight_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database"]
    async fn community_preflight_checks_membership_rules_and_credentials_without_sending()
    -> Result<(), Box<dyn std::error::Error>> {
        // A foreign-table shape is part of this proof, so give it a private
        // namespace. Other native fixtures may own an older public shape.
        let database_url = std::env::var("CROWDRELAY_TEST_DATABASE_URL")?;
        let admin = sqlx::PgPool::connect(&database_url).await?;
        let schema = format!("preflight_{}", Uuid::now_v7().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await?;
        let search_path = format!("{schema},public");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .after_connect(move |connection, _| {
                let search_path = search_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(search_path)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await?;
        let result = async {

        let ws = WorkspaceId::new();
        let place = Uuid::now_v7();
        let target = Uuid::now_v7();
        sqlx::query("INSERT INTO workspaces(id,slug,name) VALUES($1,$2,'Preflight proof')")
            .bind(ws.into_uuid())
            .bind(format!("preflight-{}", ws.into_uuid().simple()))
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO discovery_places(id,workspace_id,place_kind,platform,name,url,membership_state) VALUES($1,$2,'forum','forum','Forum','https://forum.example/music','joined')")
            .bind(place).bind(ws.into_uuid()).execute(&pool).await?;
        sqlx::query("INSERT INTO agent_outreach_targets(id,workspace_id,target_kind,display_name,platform,community_url,place_id,status,screening_verdict) VALUES($1,$2,'community','Forum','forum','https://forum.example/music',$3,'promoted','admitted')")
            .bind(target).bind(ws.into_uuid()).bind(place).execute(&pool).await?;
        let executor = CommunityExecutorWorker::new(
            pool.clone(),
            ws,
            Duration::from_secs(5),
            true,
            None,
            "http://127.0.0.1:9".to_owned(),
            None,
            None,
        )?;
        let mut action = ClaimedAction {
            id: Uuid::now_v7(),
            action_id: Uuid::now_v7(),
            claimed_from: "pending".to_owned(),
            target_id: Some(target),
            platform: "forum".to_owned(),
            subreddit: "Forum".to_owned(),
            place_url: Some("https://forum.example/music".to_owned()),
            title: "Video".to_owned(),
            body: "Source facts".to_owned(),
            smart_link: None,
            image_url: None,
            media_id: None,
            source_url: None,
            relay_source_id: None,
            trace_id: None,
            causation_id: None,
            decision_id: None,
        };
        assert_eq!(
            executor.preflight_community_send(&action).await?,
            Some("community_rules_need_manual_verification_or_approval")
        );
        sqlx::query("INSERT INTO discovery_place_rules(place_id,verified_at,self_promo_ratio_percent) VALUES($1,now(),100)")
            .bind(place).execute(&pool).await?;
        // Optional foreign schema, with harmless test-only values. The
        // preflight reads presence and status, never encrypted_value.
        sqlx::query("CREATE TABLE IF NOT EXISTS agent_service_credentials(id uuid PRIMARY KEY DEFAULT gen_random_uuid(),workspace_id uuid NOT NULL,provider text NOT NULL,credential_type text NOT NULL,encrypted_value text NOT NULL,status text NOT NULL DEFAULT 'active',UNIQUE(workspace_id,provider))")
            .execute(&pool).await?;
        assert_eq!(
            executor.preflight_community_send(&action).await?,
            Some("community_credential_missing")
        );
        sqlx::query("INSERT INTO agent_service_credentials(workspace_id,provider,credential_type,encrypted_value) VALUES($1,'forum:forum.example','test','test-only-not-a-credential')")
            .bind(ws.into_uuid()).execute(&pool).await?;
        assert_eq!(executor.preflight_community_send(&action).await?, None);
        sqlx::query("UPDATE discovery_place_rules SET requires_approval=true WHERE place_id=$1")
            .bind(place)
            .execute(&pool)
            .await?;
        assert_eq!(
            executor.preflight_community_send(&action).await?,
            Some("community_rules_need_manual_verification_or_approval")
        );
        sqlx::query("UPDATE discovery_place_rules SET requires_approval=false,self_promo_ratio_percent=0 WHERE place_id=$1")
            .bind(place).execute(&pool).await?;
        let refusal = executor
            .preflight_community_send(&action)
            .await?
            .expect("promotion refused");
        assert!(community_preflight_refused(refusal));
        sqlx::query(
            "UPDATE discovery_place_rules SET self_promo_ratio_percent=100 WHERE place_id=$1",
        )
        .bind(place)
        .execute(&pool)
        .await?;
        sqlx::query("UPDATE discovery_places SET membership_state='not_a_fit' WHERE workspace_id=$1 AND id=$2")
            .bind(ws.into_uuid()).bind(place).execute(&pool).await?;
        assert_eq!(
            executor.preflight_community_send(&action).await?,
            Some("community_membership_not_joined")
        );
        action.platform = "reddit".to_owned();
        assert_eq!(
            executor.preflight_community_send(&action).await?,
            Some("community_readiness_missing"),
            "a queued Reddit post also respects a later fit refusal"
        );
        Ok::<(), Box<dyn std::error::Error>>(())
        }.await;
        pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await?;
        result
    }
}
