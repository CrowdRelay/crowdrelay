// Included by community_executor.rs. No credential values are read here.
fn community_preflight_refused(reason: &str) -> bool {
    matches!(
        reason,
        "community_readiness_missing"
            | "community_membership_not_joined"
            | "community_destination_invalid"
            | "community_destination_changed"
            | "community_self_promotion_not_allowed"
    )
}
impl CommunityExecutorWorker {
    async fn preflight_community_send(
        &self,
        action: &ClaimedAction,
    ) -> Result<Option<&'static str>, CommunityExecutorError> {
        if action.platform == "reddit" {
            // Reddit is not a special bypass around the community contract.
            // Before automation can publish, the target must still be admitted,
            // its place must still be usable, and the moderators' own rules
            // must have been measured recently. Missing/stale rules or a
            // manual-only condition (flair/mod approval) park the draft for a
            // person instead of gambling the account's standing.
            let state: Option<(bool, bool, bool)> = sqlx::query_as(
                r#"SELECT
                        (target.place_id IS NULL
                         OR (place.id IS NOT NULL
                             AND place.status='active'
                             AND place.membership_state NOT IN ('rejected','not_a_fit'))) AS place_ready,
                        COALESCE(rules.self_promo_ratio_percent, 100) > 0 AS promotion_allowed,
                        COALESCE(
                            rules.verified_at >= now() - interval '30 days'
                            AND NOT rules.requires_approval,
                            false
                        ) AS rules_ready
                   FROM agent_outreach_targets target
                   LEFT JOIN discovery_places place
                     ON place.id=target.place_id
                    AND place.workspace_id=target.workspace_id
                   LEFT JOIN discovery_place_rules rules
                     ON rules.place_id=target.place_id
                   WHERE target.workspace_id=$1 AND target.id=$2
                     AND target.status='promoted'
                     AND target.screening_verdict='admitted'
                     AND COALESCE(NULLIF(target.platform,''),'reddit')='reddit'"#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(action.target_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some((place_ready, promotion_allowed, rules_ready)) = state else {
                return Ok(Some("community_readiness_missing"));
            };
            if !place_ready {
                return Ok(Some("community_readiness_missing"));
            }
            if !promotion_allowed {
                return Ok(Some("community_self_promotion_not_allowed"));
            }
            return Ok(if rules_ready {
                None
            } else {
                Some("community_rules_need_manual_verification_or_approval")
            });
        }
        let place: Option<(String, String, bool, bool)> = sqlx::query_as(
            r#"SELECT place.membership_state,
                      COALESCE(NULLIF(target.community_url, ''), place.url),
                      COALESCE((rules.verified_at IS NOT NULL AND NOT rules.requires_approval),false),
                      COALESCE(rules.self_promo_ratio_percent,100)>0
               FROM agent_outreach_targets target
               JOIN discovery_places place ON place.id=target.place_id AND place.workspace_id=target.workspace_id
               LEFT JOIN discovery_place_rules rules ON rules.place_id=place.id
               WHERE target.workspace_id=$1 AND target.id=$2
                 AND target.status='promoted' AND target.screening_verdict='admitted'
                 AND place.status='active' AND target.platform=$3"#,
        ).bind(self.workspace_id.into_uuid()).bind(action.target_id).bind(&action.platform)
            .fetch_optional(&self.pool).await?;
        let Some((membership, address, rules_verified, promotion_allowed)) = place else {
            return Ok(Some("community_readiness_missing"));
        };
        if membership != "joined" {
            return Ok(Some("community_membership_not_joined"));
        }
        if action.place_url.as_deref() != Some(address.as_str()) {
            return Ok(Some("community_destination_changed"));
        }
        if !promotion_allowed {
            return Ok(Some("community_self_promotion_not_allowed"));
        }
        if !rules_verified {
            return Ok(Some("community_rules_need_manual_verification_or_approval"));
        }
        let provider = match action.platform.as_str() {
            "lemmy" => "lemmy".to_owned(),
            "telegram" => "telegram-user".to_owned(),
            "forum" => {
                let host = url::Url::parse(&address)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned));
                let Some(host) = host else {
                    return Ok(Some("community_destination_invalid"));
                };
                format!("forum:{host}")
            }
            _ => return Ok(Some("community_publisher_unavailable")),
        };
        // The agents schema is optional. Absence means manual work, never a
        // speculative publish attempt or a fabricated credential.
        let credential = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM agent_service_credentials WHERE workspace_id=$1 AND provider=$2 AND status='active')",
        ).bind(self.workspace_id.into_uuid()).bind(provider).fetch_one(&self.pool).await;
        match credential {
            Ok(true) => Ok(None),
            Ok(false) => Ok(Some("community_credential_missing")),
            Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("42P01") => {
                Ok(Some("community_credentials_unavailable"))
            }
            Err(error) => Err(error.into()),
        }
    }
}
