//! The measured half of the cross-act catalogue rotation (5.15,
//! §4h-7.4): which `catalogue_rotation` consent edges in one organisation
//! have headroom, what each beneficiary's catalogue holds, and what the
//! edge has already carried. The picking rule lives in
//! `crowdrelay_domain::roster_catalogue_rotation`; this module fetches and
//! dispatches, it does not decide.
//!
//! Everything here is read through the same tables the consent ledger and
//! each act's own release ladder keep: `amplification_consents` for the
//! grant, `amplification_deliveries` for what the edge already carried,
//! `viryaos_release_plans` for the catalogue. The dispatch itself is the
//! existing `run_amplification_campaign` — the monthly cap, the per-fan
//! cooldown and the dedupe all apply unchanged, which is the point: the
//! rotation spends from the attention budget rather than adding to it.
//! What is new is only the labelling — the campaign reference is
//! `catalogue:<release_id>` and the outbox payload carries
//! `kind: 'catalogue_rotation'` plus the release it featured.

use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::roster_catalogue_rotation::{
    CatalogueItem, CatalogueRotationPlan, RotationEdge, campaign_reference,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::portfolio::{PortfolioError, PostgresPortfolioRepository};

#[derive(Debug, sqlx::FromRow)]
struct EdgeRow {
    id: Uuid,
    from_workspace_id: Uuid,
    to_workspace_id: Uuid,
    from_act: String,
    to_act: String,
    max_campaigns_per_month: i16,
    campaigns_this_month: i64,
    reachable_fans: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct ReleaseRow {
    id: Uuid,
    title: String,
    release_at: OffsetDateTime,
    listen_url: Option<String>,
}

/// The plan the label reads: every active `catalogue_rotation` edge in the
/// organisation with cap headroom, the release it would feature next, and
/// the edges whose catalogues are finished.
///
/// # Errors
///
/// Propagates the database error as [`PortfolioError::Database`].
pub async fn catalogue_rotation_plan(
    pool: &PgPool,
    organization_id: Uuid,
    now: OffsetDateTime,
) -> Result<CatalogueRotationPlan, PortfolioError> {
    let edges = sqlx::query_as::<_, EdgeRow>(
        r#"
        SELECT consent.id, consent.from_workspace_id, consent.to_workspace_id,
               from_ws.name AS from_act, to_ws.name AS to_act,
               consent.max_campaigns_per_month,
               (SELECT count(DISTINCT ledger.campaign_reference)
                FROM amplification_deliveries AS ledger
                WHERE ledger.consent_id = consent.id
                  AND ledger.delivered_at >= date_trunc('month', now()))
                   AS campaigns_this_month,
               (SELECT count(*)
                FROM fans AS fan
                WHERE fan.workspace_id = consent.from_workspace_id
                  AND fan.status = 'active'
                  AND fan.deleted_at IS NULL)
                   AS reachable_fans
        FROM amplification_consents AS consent
        JOIN workspaces AS from_ws ON from_ws.id = consent.from_workspace_id
        JOIN workspaces AS to_ws ON to_ws.id = consent.to_workspace_id
        WHERE consent.organization_id = $1
          AND consent.purpose = 'catalogue_rotation'
          AND consent.status = 'active'
        ORDER BY from_ws.name, to_ws.name, consent.id
        "#,
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await
    .map_err(PortfolioError::Database)?;

    let mut assembled = Vec::with_capacity(edges.len());
    for edge in edges {
        // The beneficiary's catalogue: active, fan-communicable releases.
        // `release_at <= now` filtering lives in the domain pick so the
        // rule stays testable — the read delivers the whole active list.
        let catalogue = sqlx::query_as::<_, ReleaseRow>(
            r#"
            SELECT id, title, release_at, listen_url
            FROM viryaos_release_plans
            WHERE workspace_id = $1
              AND active
              AND communication_enabled
            ORDER BY release_at, id
            "#,
        )
        .bind(edge.to_workspace_id)
        .fetch_all(pool)
        .await
        .map_err(PortfolioError::Database)?
        .into_iter()
        .map(|row| CatalogueItem {
            release_id: row.id,
            title: row.title,
            release_at: row.release_at,
            listen_url: row.listen_url,
        })
        .collect();

        // Which releases this edge has already rotated — the campaign
        // reference is the ledger the rotation walks.
        let rotated = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT DISTINCT substring(ledger.campaign_reference from 11)::uuid
            FROM amplification_deliveries AS ledger
            WHERE ledger.consent_id = $1
              AND ledger.campaign_reference LIKE 'catalogue:%'
              AND ledger.campaign_reference ~ '^catalogue:[0-9a-f-]{36}$'
            "#,
        )
        .bind(edge.id)
        .fetch_all(pool)
        .await
        .map_err(PortfolioError::Database)?;

        assembled.push(RotationEdge {
            consent_id: edge.id,
            from_workspace_id: WorkspaceId::from_uuid(edge.from_workspace_id),
            from_act: edge.from_act,
            to_workspace_id: WorkspaceId::from_uuid(edge.to_workspace_id),
            to_act: edge.to_act,
            max_campaigns_per_month: u16::try_from(edge.max_campaigns_per_month)
                .unwrap_or(u16::MAX),
            campaigns_this_month: u16::try_from(edge.campaigns_this_month.max(0))
                .unwrap_or(u16::MAX),
            reachable_fans: u32::try_from(edge.reachable_fans.max(0)).unwrap_or(u32::MAX),
            catalogue,
            already_rotated: rotated.into_iter().collect(),
        });
    }

    Ok(crowdrelay_domain::roster_catalogue_rotation::compose(
        organization_id,
        now,
        assembled,
    ))
}

/// The result of one landed rotation — what the approval response reports.
#[derive(Debug)]
pub struct RotationRun {
    pub consent_id: Uuid,
    pub release_id: Uuid,
    pub campaign_reference: String,
    pub queued: i64,
}

/// Runs one rotation on an organisation's edge: recomputes the pick, then
/// dispatches through the capped campaign path with the catalogue label.
///
/// The pick is recomputed inside the call rather than trusted from the
/// proposal read — a stale screen cannot rotate a release that has since
/// been carried or cancelled.
///
/// # Errors
///
/// - [`PortfolioError::NotFound`]: no active `catalogue_rotation` edge with
///   that id inside this organisation.
/// - [`PortfolioError::CapReached`]: the edge's monthly cap is spent.
/// - [`PortfolioError::CatalogueExhausted`]: nothing left to rotate.
/// - [`PortfolioError::Database`]: the persistence failure.
pub async fn run_catalogue_rotation(
    pool: &PgPool,
    organization_id: Uuid,
    consent_id: Uuid,
    now: OffsetDateTime,
) -> Result<RotationRun, PortfolioError> {
    // Reload the edge under the organisation boundary — the id alone is not
    // authority, the membership is.
    let edge = sqlx::query_as::<_, (Uuid, String, String)>(
        r#"
        SELECT consent.from_workspace_id, from_ws.name, to_ws.name
        FROM amplification_consents AS consent
        JOIN workspaces AS from_ws ON from_ws.id = consent.from_workspace_id
        JOIN workspaces AS to_ws ON to_ws.id = consent.to_workspace_id
        WHERE consent.id = $1
          AND consent.organization_id = $2
          AND consent.purpose = 'catalogue_rotation'
          AND consent.status = 'active'
        "#,
    )
    .bind(consent_id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await
    .map_err(PortfolioError::Database)?
    .ok_or(PortfolioError::NotFound)?;
    let (from_workspace_id, _from_act, to_act) = edge;

    // The same pick the plan composed — recomputed against the live rows.
    let plan = catalogue_rotation_plan(pool, organization_id, now).await?;
    let proposal = plan
        .proposals
        .iter()
        .find(|proposal| proposal.consent_id == consent_id);
    let release = match proposal {
        Some(proposal) => proposal.release.clone(),
        None if plan
            .exhausted
            .iter()
            .any(|edge| edge.consent_id == consent_id) =>
        {
            return Err(PortfolioError::CatalogueExhausted);
        }
        // Not proposed and not exhausted: the cap is spent — the plan drops
        // spent edges before either list, which is exactly CapReached.
        None => return Err(PortfolioError::CapReached),
    };

    let reference = campaign_reference(release.release_id);
    let subject = format!("{to_act} — from the back catalogue");
    let text = match &release.listen_url {
        Some(url) => format!(
            "From {to_act}'s back catalogue: \"{}\" — {url}",
            release.title
        ),
        None => format!("From {to_act}'s back catalogue: \"{}\"", release.title),
    };
    let queued = PostgresPortfolioRepository::new(pool.clone())
        .run_amplification_campaign(
            from_workspace_id,
            consent_id,
            &reference,
            &subject,
            &text,
            2_000,
            serde_json::json!({
                "kind": "catalogue_rotation",
                "release": {
                    "id": release.release_id,
                    "title": release.title,
                    "listen_url": release.listen_url,
                },
            }),
        )
        .await?;
    Ok(RotationRun {
        consent_id,
        release_id: release.release_id,
        campaign_reference: reference,
        queued,
    })
}
