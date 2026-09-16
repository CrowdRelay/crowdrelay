//! The arc engine's repository side — gather the calendar, propose the
//! campaign, advance its lifecycle.
//!
//! `crowdrelay-brain::content_arcs` is pure; this module is where its inputs
//! come from and where its proposals persist. One open arc at a time is the
//! anti-noise rule in concrete form: a band staring at two campaigns sees
//! neither. Proposals land `proposed` and the autopilot queue carries the
//! ask — the band approves the arc, and every beat inside it inherits that
//! answer instead of asking again.

use crowdrelay_brain::content_arcs::{
    ArcAnchor, ArcAnchorKind, ArcInputs, ArcTrendSupport, propose_arc,
};
use crowdrelay_brain::content_suggestions::format_keys_for_pattern;
use crowdrelay_domain::{
    ArcId, WorkspaceId,
    content_engine::{Arc, TrendDimension, TrendStatus},
};
use time::Date;
use uuid::Uuid;

use crate::content_engine::{ArcRow, NewArc, PostgresContentEngineRepository, Result};

/// How long a retired arc's anchor stays off-limits — a "no" is still a no
/// for two weeks, after which new evidence may fairly re-ask the question.
const DECLINED_ANCHOR_COOLDOWN_DAYS: i64 = 14;

impl PostgresContentEngineRepository {
    /// Dated material worth building a campaign around: the next release,
    /// the next published show, the next production day. The far bound is a
    /// read cap, not the arc window — the proposer still rejects anchors too
    /// far out to plan against.
    async fn arc_anchors(&self, workspace_id: WorkspaceId, today: Date) -> Result<Vec<ArcAnchor>> {
        let ws = workspace_id.into_uuid();
        let far = today + time::Duration::days(120);
        let mut anchors: Vec<ArcAnchor> = sqlx::query_as::<_, (Uuid, String, Date)>(
            r#"
            SELECT id, title, release_at::date AS date
            FROM viryaos_release_plans
            WHERE workspace_id = $1 AND active AND release_at::date >= $2
              AND release_at::date <= $3
            "#,
        )
        .bind(ws)
        .bind(today)
        .bind(far)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|(id, name, date)| ArcAnchor {
            kind: ArcAnchorKind::Release,
            id: id.to_string(),
            name,
            date,
        })
        .collect();
        anchors.extend(
            sqlx::query_as::<_, (Uuid, String, Date)>(
                r#"
                SELECT id, title, starts_at::date AS date
                FROM events
                WHERE workspace_id = $1 AND status = 'published'
                  AND starts_at::date >= $2 AND starts_at::date <= $3
                "#,
            )
            .bind(ws)
            .bind(today)
            .bind(far)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|(id, name, date)| ArcAnchor {
                kind: ArcAnchorKind::Show,
                id: id.to_string(),
                name,
                date,
            }),
        );
        anchors.extend(
            sqlx::query_as::<_, (Uuid, String, Date)>(
                r#"
                SELECT id, title, scheduled_for AS date
                FROM viryaos_production_events
                WHERE workspace_id = $1 AND status = 'scheduled'
                  AND scheduled_for >= $2 AND scheduled_for <= $3
                "#,
            )
            .bind(ws)
            .bind(today)
            .bind(far)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|(id, name, date)| ArcAnchor {
                kind: ArcAnchorKind::ProductionEvent,
                id: id.to_string(),
                name,
                date,
            }),
        );
        Ok(anchors)
    }

    /// Anchor ids a recently-retired arc already pointed at — the cooldown
    /// the proposer honours so a declined campaign is not re-asked weekly.
    /// A show and the production day linked to it are one piece of material
    /// wearing two ids, so the decline follows `event_id` both directions:
    /// a "no" to the gig is a "no" to its shoot, and back.
    async fn declined_anchor_ids(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<String>> {
        let rows = sqlx::query_scalar::<_, Option<String>>(
            r#"
            WITH declined AS (
                SELECT evidence->'anchor'->>'id' AS anchor_id
                FROM viryaos_arcs
                WHERE workspace_id = $1 AND status = 'retired'
                  AND updated_at >= now() - make_interval(days => $2)
            )
            SELECT anchor_id FROM declined
            UNION
            SELECT pe.id::text
            FROM viryaos_production_events AS pe
            JOIN declined ON declined.anchor_id = pe.event_id::text
            WHERE pe.workspace_id = $1
            UNION
            SELECT pe.event_id::text
            FROM viryaos_production_events AS pe
            JOIN declined ON declined.anchor_id = pe.id::text
            WHERE pe.workspace_id = $1 AND pe.event_id IS NOT NULL
            "#,
        )
        .bind(workspace_id.into_uuid())
        .bind(DECLINED_ANCHOR_COOLDOWN_DAYS as i32)
        .fetch_all(&mut **tx)
        .await?;
        Ok(rows.into_iter().flatten().collect())
    }

    /// Advance the lifecycle, then propose at most one new arc.
    ///
    /// Approved arcs go active when their horizon opens; active arcs
    /// complete when it closes. A workspace holding any open arc —
    /// proposed, approved or active — gets no new proposal: the queue's job
    /// is one plan at a time, and the open arc is that plan.
    ///
    /// Reads happen before the transaction (the inputs are slow-moving);
    /// the dedup read and the insert happen inside it under the advisory
    /// lock, so two sweeps cannot both see "no open arc" and propose twice.
    pub async fn refresh_arcs(&self, workspace_id: WorkspaceId, today: Date) -> Result<Vec<Arc>> {
        let ws = workspace_id.into_uuid();

        let profile = self.capability_profile(workspace_id).await?;
        let formats = self.list_format_entries().await?;
        let trends = self.list_trends(workspace_id).await?;
        let anchors = self.arc_anchors(workspace_id, today).await?;

        let mut feasible: Vec<_> = formats
            .into_iter()
            .filter(|entry| entry.capability_gap(&profile).is_none())
            .collect();
        let trend_support: Vec<ArcTrendSupport> = trends
            .iter()
            .filter(|trend| {
                trend.dimension == TrendDimension::Format && trend.status != TrendStatus::Faded
            })
            .map(|trend| ArcTrendSupport {
                format_keys: std::iter::once(trend.pattern.clone())
                    .chain(
                        format_keys_for_pattern(&trend.pattern)
                            .iter()
                            .map(|key| (*key).to_owned()),
                    )
                    .collect(),
                trend_ids: vec![trend.id.into_uuid().to_string()],
            })
            .collect();

        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
            .bind(format!("{ws}:arcs"))
            .execute(&mut *tx)
            .await?;

        // The lifecycle does not wait for a proposal pass to move: an arc
        // the band approved goes live when its horizon opens, and an
        // active one closes when it ends.
        sqlx::query(
            "UPDATE viryaos_arcs SET status = 'active', updated_at = now() \
             WHERE workspace_id = $1 AND status = 'approved' \
               AND horizon_start IS NOT NULL AND horizon_start <= $2",
        )
        .bind(ws)
        .bind(today)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE viryaos_arcs SET status = 'completed', updated_at = now() \
             WHERE workspace_id = $1 AND status = 'active' \
               AND horizon_end IS NOT NULL AND horizon_end < $2",
        )
        .bind(ws)
        .bind(today)
        .execute(&mut *tx)
        .await?;
        // A proposal nobody could ever answer — policy off, ask never
        // emitted, ask dead in the queue — still counts as open. Left alone
        // it would block every future season behind a question nobody is
        // looking at. Its window closing is the honest answer: the season
        // it argued for is gone, so it retires rather than lingers.
        sqlx::query(
            "UPDATE viryaos_arcs SET status = 'retired', updated_at = now() \
             WHERE workspace_id = $1 AND status = 'proposed' \
               AND horizon_end IS NOT NULL AND horizon_end < $2",
        )
        .bind(ws)
        .bind(today)
        .execute(&mut *tx)
        .await?;

        let open = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM viryaos_arcs \
             WHERE workspace_id = $1 AND status IN ('proposed','approved','active'))",
        )
        .bind(ws)
        .fetch_one(&mut *tx)
        .await?;
        if open {
            tx.commit().await?;
            return Ok(Vec::new());
        }

        let declined = self.declined_anchor_ids(&mut tx, workspace_id).await?;
        // A declined format is taste, not timing — a spine built on it
        // would re-ask the same "not for us" inside a season's clothing.
        let declined_formats = self.declined_format_keys(&mut tx, workspace_id).await?;
        feasible.retain(|entry| !declined_formats.contains(&entry.key));
        let proposed = propose_arc(&ArcInputs {
            anchors: &anchors,
            feasible_formats: &feasible,
            trend_support: &trend_support,
            declined_anchor_ids: &declined,
            today,
        });
        let Some(proposal) = proposed else {
            tx.commit().await?;
            return Ok(Vec::new());
        };

        let arc = self
            .create_arc_in(
                &mut tx,
                workspace_id,
                &NewArc {
                    title: proposal.title,
                    summary: proposal.summary,
                    horizon_start: Some(proposal.horizon_start),
                    horizon_end: Some(proposal.horizon_end),
                    spine: proposal.spine,
                    evidence: proposal.evidence,
                },
            )
            .await?;
        tx.commit().await?;
        Ok(vec![arc])
    }

    /// `create_arc` inside the proposal transaction — the row and the dedup
    /// decision commit or roll back together.
    async fn create_arc_in(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace_id: WorkspaceId,
        arc: &NewArc,
    ) -> Result<Arc> {
        let row = sqlx::query_as::<_, ArcRow>(
            r#"
            INSERT INTO viryaos_arcs (
                id, workspace_id, title, summary, horizon_start, horizon_end,
                spine, evidence
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            RETURNING *
            "#,
        )
        .bind(ArcId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&arc.title)
        .bind(&arc.summary)
        .bind(arc.horizon_start)
        .bind(arc.horizon_end)
        .bind(&arc.spine)
        .bind(&arc.evidence)
        .fetch_one(&mut **tx)
        .await?;
        Arc::try_from(row)
    }
}
