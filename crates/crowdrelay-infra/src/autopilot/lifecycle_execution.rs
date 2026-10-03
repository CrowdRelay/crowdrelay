//! Ordinary consented-fan lifecycle execution.
//!
//! Kept separate from the transactional confirmation-recovery path: the latter
//! is authentication/double-opt-in recovery, while this module owns actual
//! marketing lifecycle messages and their tracked CTAs.

use super::*;

pub(super) async fn execute(
    repo: &PostgresAutopilotRepository,
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    action: &ClaimedAutopilotAction,
    fan_id: FanId,
    template_key: &str,
    show: &Option<crowdrelay_application::autopilot::LifecycleShowContext>,
    now: OffsetDateTime,
) -> Result<(), RepositoryError> {
    ensure_marketing_eligible(transaction, workspace_id, fan_id).await?;
    let fan = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT normalized_email, display_name, locale FROM fans WHERE workspace_id=$1 AND id=$2 AND status='active' FOR SHARE",
    )
    .bind(workspace_id.into_uuid())
    .bind(fan_id.into_uuid())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::Conflict)?;
    let wordmark = sqlx::query_scalar::<_, String>(
        "SELECT crowdrelay_workspace_wordmark($1)",
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&mut *transaction)
    .await
    .map_err(map_sqlx)?;
    let activation = if template_key == WELCOME_V2_TEMPLATE {
        if !executor_capability_available(transaction, workspace_id, WELCOME_V2_CAPABILITY).await? {
            return Err(RepositoryError::Unavailable);
        }
        let brand = crate::tenant_settings::TenantSettingsRepository::new(repo.pool.clone())
            .brand_settings(workspace_id.into_uuid()).await.map_err(map_sqlx)?;
        lifecycle_activation::prepare(
            transaction, &brand,
            lifecycle_activation::WelcomeRequest {
                workspace_id, action_id: action.id, fan_id: fan_id,
                locale: fan.2.as_deref().unwrap_or_default(), now,
            },
        ).await?
    } else { None };
    // The referral invite is the fan→fan growth loop: the
    // executor receives a complete first-party URL rather
    // than reconstructing one from the code, and a missing
    // code or site is terminal for this message.
    let (referral_code, referral_url) =
        if template_key == "crowdrelay.fan.referral_invite.v1" {
            let code = sqlx::query_scalar::<_, Option<String>>(
                "SELECT code FROM referral_codes WHERE workspace_id=$1 AND fan_id=$2 AND active",
            )
            .bind(workspace_id.into_uuid())
            .bind(fan_id.into_uuid())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            .flatten()
            .ok_or(RepositoryError::ConflictBecause(
                "referral invite refused: fan has no active referral code",
            ))?;
            let brand = crate::tenant_settings::TenantSettingsRepository::new(
                repo.pool.clone(),
            )
            .brand_settings(workspace_id.into_uuid())
            .await
            .map_err(map_sqlx)?;
            let url = brand.referral_url(&code).ok_or(
                RepositoryError::ConflictBecause(
                    "referral invite refused: tenant has no member site URL",
                ),
            )?;
            (Some(code), Some(url))
        } else {
            (None, None)
        };
    // The install ask carries a tracked link to the Signal
    // page — the click is the only measurement the ask has.
    // A linkless ask is refused rather than sent, for the same
    // reason a referral invite without a URL is. The recall
    // mints the same link when the decision flagged the fan
    // as install-less: for somebody whose only footprint is
    // a door scan, "you were there" and "open Signal" are one
    // ask, not two.
    let wants_install_url = template_key
        == "crowdrelay.fan.signal_install_ask.v1"
        || (template_key == "crowdrelay.fan.show_recall.v1"
            && show.as_ref().is_some_and(|show| show.wants_install_url));
    let install_url =
        // The fan may have opened Signal since the recall's
        // decision. Keep the recall, drop the stale install CTA.
        if wants_install_url && !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM signal_installations WHERE workspace_id=$1 AND fan_id=$2)",
        )
        .bind(workspace_id.into_uuid())
        .bind(fan_id.into_uuid())
        .fetch_one(&mut *transaction)
        .await
        .map_err(map_sqlx)? {
            let brand = crate::tenant_settings::TenantSettingsRepository::new(
                repo.pool.clone(),
            )
            .brand_settings(workspace_id.into_uuid())
            .await
            .map_err(map_sqlx)?;
            let locale = fan.2.as_deref().unwrap_or_default();
            let destination = brand.signal_page_url(locale).ok_or(
                RepositoryError::ConflictBecause(
                    "signal install ask refused: tenant has no member site URL",
                ),
            )?;
            let slug = format!("signal-install-{}",action.id.into_uuid().simple());
            // Channel identity is safe to set: this slug is the
            // ask's own, shared with no other surface.
            let link = crate::tracked_links::ensure_smart_link_in_tx(
                transaction,
                workspace_id.into_uuid(),
                &slug,
                &destination,
                brand.site_root(),
                Some("email"),
                Some("signal-install-ask"),
            )
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::ConflictBecause(
                "signal install ask refused: destination is not a printable URL",
            ))?;
            let bound=sqlx::query("UPDATE smart_links SET action_id=$3 WHERE workspace_id=$1 AND slug=$2 AND (action_id IS NULL OR action_id=$3)")
                .bind(workspace_id.into_uuid()).bind(&slug).bind(action.id.into_uuid())
                .execute(&mut *transaction).await.map_err(map_sqlx)?;
            if bound.rows_affected()!=1 {return Err(RepositoryError::ConflictBecause("install ask link belongs to another action"));}
            Some(link.as_str().to_owned())
        } else {
            None
        };
    // Each recall owns its redirect. A show-wide redirect
    // would pool different recipients' clicks rather than
    // attributing them to the message that carried the link.
    // Historical redirects are left untouched.
    let show_url = if let Some(show) = show.as_ref() {
        let brand = crate::tenant_settings::TenantSettingsRepository::new(
            repo.pool.clone(),
        )
        .brand_settings(workspace_id.into_uuid())
        .await
        .map_err(map_sqlx)?;
        let locale = fan.2.as_deref().unwrap_or_default();
        let destination = brand
            .event_page_url(locale, &show.event_slug)
            .ok_or(RepositoryError::ConflictBecause(
                "show recall refused: tenant has no member site URL",
            ))?;
        let slug = format!("show-recall-{}", action.id.into_uuid().simple());
        let link = crate::tracked_links::ensure_smart_link_in_tx(
            transaction,
            workspace_id.into_uuid(),
            &slug,
            &destination,
            brand.site_root(),
            Some("email"),
            Some("show-recall"),
        )
        .await
        .map_err(map_sqlx)?
        .ok_or(RepositoryError::ConflictBecause(
            "show recall refused: event page is not a printable URL",
        ))?;
        let bound = sqlx::query(
            "UPDATE smart_links SET action_id=$3 WHERE workspace_id=$1 AND slug=$2 AND (action_id IS NULL OR action_id=$3)",
        )
        .bind(workspace_id.into_uuid())
        .bind(&slug)
        .bind(action.id.into_uuid())
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx)?;
        if bound.rows_affected() != 1 {
            return Err(RepositoryError::ConflictBecause(
                "show recall link belongs to another action",
            ));
        }
        Some(link.as_str().to_owned())
    } else {
        None
    };
    emit_outward_action(
        transaction,
        workspace_id,
        action.id,
        "crowdrelay.fan_lifecycle.message_requested",
        format!("fan-lifecycle:{fan_id}"),
        format!(
            "consented fan, lifecycle template {template_key} — marketing consent is re-checked at send"
        ),
        json!({
            "action_id": action.id,
            "fan_id": fan_id,
            "template_key": template_key,
            "brand": {
                "wordmark": wordmark,
            },
            "fan": {
                "email": fan.0,
                "display_name": fan.1,
                "locale": fan.2,
                "referral_code": referral_code,
                "referral_url": referral_url,
                "install_url": install_url,
                "activation": activation,
                // The night the recall names — the template's
                // only subject. Both are None for every other
                // lifecycle key.
                "show_title": show.as_ref().map(|show| show.event_title.as_str()),
                "show_url": show_url,
            },
        }),
    )
    .await?;
    Ok(())
}
