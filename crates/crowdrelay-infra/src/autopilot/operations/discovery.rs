//! Target discovery ingress: candidates in, screened, and promoted by hand.
//!
//! Screening happens here at write time rather than in a sweep, for one
//! reason: a refusal is only useful if it is durable. A candidate that arrives,
//! is judged and is stored with its verdict can never be rediscovered and
//! re-judged next week, which is what makes discovery cheap to run often.
//!
//! Nothing in this file contacts anybody. It decides what may later be
//! contacted, and by which route.

use super::*;

use async_trait::async_trait;
use crowdrelay_application::autopilot::{
    AutopilotTargetDiscoveryRepository, IngestOutreachCandidate, OutreachCandidateIngestion,
    OutreachCandidatePromotion, OutreachCandidateView, SubmissionChannelMutation,
    UpsertSubmissionChannel,
};
use sqlx::{Postgres, Transaction};

use crowdrelay_domain::target_discovery::{
    CandidateSnapshot, OutreachSupplySnapshot, RouteKind, ScreeningVerdict, TargetDiscoveryPolicy,
    promotes_to_target, screen_candidate,
};

/// One batch is bounded so an adapter cannot turn a discovery sweep into an
/// unbounded transaction.
const MAX_CANDIDATES_PER_BATCH: usize = 100;
const MAX_CANDIDATE_PAGE: u32 = 200;

/// What the pitcher has, and what the last sweeps produced.
///
/// `consecutive_barren_sweeps` is counted from emitted sweep requests rather
/// than from candidate rows, because the two failure modes it has to tell
/// apart look identical from the candidate table alone: a source that returns
/// refusals, and an adapter that never came back at all. Only the first should
/// stop the agent asking.
pub(in crate::autopilot) async fn load_outreach_supply_snapshot(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    _now: OffsetDateTime,
) -> Result<OutreachSupplySnapshot, RepositoryError> {
    let row = sqlx::query_as::<_, (i64, i64, Option<OffsetDateTime>, i64, i64)>(
        r#"
        WITH sweeps AS (
            SELECT action.created_at,
                   -- Each sweep owns the candidates that arrived before the
                   -- next one did; without the window every older sweep would
                   -- also be credited with every later candidate.
                   lead(action.created_at) OVER (ORDER BY action.created_at)
                       AS window_ends_at,
                   row_number() OVER (ORDER BY action.created_at DESC) AS recency
            FROM viryaos_autopilot_actions action
            WHERE action.workspace_id=$1
              AND action.action_kind='outreach.discovery.request'
              AND action.status IN ('queued','processing','succeeded')
        ),
        -- Whether the adapter answered at all is evidence in its own right, and
        -- it cannot be read from candidate rows: a sweep that reported "I found
        -- nothing" and a sweep that crashed both leave zero of them. The
        -- ingestion is the answer, so the operator-action ledger is what says
        -- one happened.
        answers AS (
            SELECT sweep.recency,
                   count(ingestion.id) AS ingestions,
                   count(candidate.id) FILTER (
                       WHERE candidate.status IN ('admitted','promoted')
                   ) AS survived
            FROM sweeps sweep
            LEFT JOIN operator_actions ingestion
              ON ingestion.workspace_id=$1
             AND ingestion.action='ingest_autopilot_outreach_candidates'
             AND ingestion.created_at >= sweep.created_at
             AND (sweep.window_ends_at IS NULL
                  OR ingestion.created_at < sweep.window_ends_at)
            LEFT JOIN viryaos_outreach_candidates candidate
              ON candidate.workspace_id=$1
             AND candidate.created_at >= sweep.created_at
             AND (sweep.window_ends_at IS NULL
                  OR candidate.created_at < sweep.window_ends_at)
            GROUP BY sweep.recency
        ),
        -- A sweep is barren when the adapter answered and nothing survived
        -- screening. A sweep nobody answered is an integration failure: an
        -- operator problem, not a dry source, and it must not make the agent
        -- stop asking.
        judged AS (
            SELECT recency,
                   ingestions AS arrived,
                   survived
            FROM answers
        )
        SELECT
            (SELECT count(*) FROM viryaos_outreach_targets target
             WHERE target.workspace_id=$1 AND target.active
               AND target.accepts_outreach AND NOT target.do_not_contact)::bigint,
            (SELECT count(*) FROM viryaos_outreach_candidates candidate
             WHERE candidate.workspace_id=$1 AND candidate.status='admitted')::bigint,
            (SELECT max(created_at) FROM sweeps),
            (SELECT count(*) FROM viryaos_outreach_candidates candidate
             WHERE candidate.workspace_id=$1
               AND candidate.created_at
                   >= coalesce((SELECT max(created_at) FROM sweeps),
                               '-infinity'::timestamptz))::bigint,
            -- The unbroken run of barren sweeps ending at the most recent one:
            -- everything more recent than the first sweep that was not barren.
            (SELECT count(*) FROM judged
             WHERE recency <= coalesce(
                 (SELECT min(recency) - 1 FROM judged
                  WHERE arrived = 0 OR survived > 0),
                 (SELECT max(recency) FROM judged)
             ))::bigint
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_one(&repo.pool)
    .await
    .map_err(map_sqlx)?;

    Ok(OutreachSupplySnapshot {
        pitchable_targets: u32::try_from(row.0).unwrap_or(u32::MAX),
        admitted_candidates: u32::try_from(row.1).unwrap_or(u32::MAX),
        last_sweep_requested_at: row.2,
        candidates_since_last_sweep: u32::try_from(row.3).unwrap_or(u32::MAX),
        consecutive_barren_sweeps: u16::try_from(row.4).unwrap_or(u16::MAX),
    })
}

#[derive(Debug, FromRow)]
struct CandidateRow {
    id: Uuid,
    target_kind: String,
    display_name: String,
    source: String,
    source_reference: String,
    route_kind: String,
    status: String,
    refusal_reason: Option<String>,
    pitch_class: Option<String>,
    fit_basis_points: i32,
    follower_count: Option<i32>,
}

#[async_trait]
impl AutopilotTargetDiscoveryRepository for PostgresAutopilotRepository {
    async fn ingest_outreach_candidates(
        &self,
        workspace_id: WorkspaceId,
        candidates: Vec<IngestOutreachCandidate>,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachCandidateIngestion, RepositoryError> {
        self.bounded(async {
            // An empty batch is a legitimate report: the adapter swept and found
            // nothing admissible. Recording it is what lets the supply rule tell
            // a dry source from an adapter that never came back, so refusing it
            // would leave the agent asking a dead source for ever.
            if candidates.len() > MAX_CANDIDATES_PER_BATCH {
                return Err(RepositoryError::Unexpected);
            }
            let received =
                u32::try_from(candidates.len()).map_err(|_| RepositoryError::Unexpected)?;
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            if let Some(existing) = super::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "ingest_autopilot_outreach_candidates",
                "outreach_candidate_batch",
                // A batch has no subject of its own, so the pool it feeds is
                // the subject. Both this and the details below have to be
                // identical across a replay, or the idempotency check reads a
                // retry as a different operation and refuses it.
                workspace_id.into_uuid(),
                idempotency_key,
                request_id,
                &json!({}),
            )
            .await?
            {
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(OutreachCandidateIngestion {
                    operation_id: existing,
                    received,
                    replayed: true,
                    ..OutreachCandidateIngestion::default()
                });
            }

            let policy = TargetDiscoveryPolicy::default();
            let mut report = OutreachCandidateIngestion {
                operation_id,
                received,
                ..OutreachCandidateIngestion::default()
            };
            for candidate in candidates {
                if candidate.display_name.trim().is_empty()
                    || candidate.route_value.trim().is_empty()
                    || candidate.source_reference.trim().is_empty()
                    || candidate.fit_basis_points > 10_000
                {
                    return Err(RepositoryError::Unexpected);
                }
                // The channel decides whether a pitch through this route is
                // contact or spend, so an unknown channel slug is a refusal to
                // guess rather than a default to free.
                let channel = match candidate.channel_slug.as_deref() {
                    Some(slug) => Some(load_channel(&mut transaction, workspace_id, slug).await?),
                    None => None,
                };
                let snapshot = CandidateSnapshot {
                    source: candidate.source,
                    route: candidate.route_kind,
                    channel_cost: channel.map(|(_, cost)| cost),
                    route_is_published: candidate.route_is_published,
                    has_evidence: candidate
                        .evidence
                        .as_ref()
                        .is_some_and(|evidence| !evidence.trim().is_empty()),
                    fit_basis_points: candidate.fit_basis_points,
                    follower_count: candidate.follower_count,
                    engagement_count: candidate.engagement_count,
                    sells_placement: candidate.sells_placement,
                    churns_indiscriminately: candidate.churns_indiscriminately,
                };
                let (status, refusal, class) = match screen_candidate(&snapshot, policy) {
                    ScreeningVerdict::Admit { class, .. } => {
                        ("admitted", None, Some(class.as_str()))
                    }
                    ScreeningVerdict::Refuse(reason) => ("refused", Some(reason.as_str()), None),
                };
                let inserted = sqlx::query(
                    r#"
                    INSERT INTO viryaos_outreach_candidates (
                        workspace_id, target_kind, display_name, source, source_reference,
                        evidence, route_kind, route_value, route_is_published, channel_id,
                        fit_basis_points, follower_count, engagement_count, sells_placement,
                        churns_indiscriminately, status, refusal_reason, pitch_class, screened_at
                    ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,now())
                    ON CONFLICT (workspace_id, route_kind, route_value) DO NOTHING
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(target_kind_str(candidate.target_kind))
                .bind(candidate.display_name.trim())
                .bind(candidate_source_str(candidate.source))
                .bind(candidate.source_reference.trim())
                .bind(candidate.evidence.as_deref().map(str::trim))
                .bind(route_kind_str(candidate.route_kind))
                .bind(candidate.route_value.trim())
                .bind(candidate.route_is_published)
                .bind(channel.map(|(id, _)| id))
                .bind(i32::from(candidate.fit_basis_points))
                .bind(candidate.follower_count.map(i64::from))
                .bind(candidate.engagement_count.map(i64::from))
                .bind(candidate.sells_placement)
                .bind(candidate.churns_indiscriminately)
                .bind(status)
                .bind(refusal)
                .bind(class)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;

                if inserted.rows_affected() == 0 {
                    report.duplicates += 1;
                } else if status == "admitted" {
                    report.admitted += 1;
                } else {
                    report.refused += 1;
                }
            }
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(report)
        })
        .await
    }

    async fn list_outreach_candidates(
        &self,
        workspace_id: WorkspaceId,
        status: Option<String>,
        limit: u32,
    ) -> Result<Vec<OutreachCandidateView>, RepositoryError> {
        self.bounded(async {
            let rows = sqlx::query_as::<_, CandidateRow>(
                r#"
                SELECT id, target_kind, display_name, source, source_reference, route_kind,
                       status, refusal_reason, pitch_class, fit_basis_points, follower_count
                FROM viryaos_outreach_candidates
                WHERE workspace_id = $1
                  AND ($2::text IS NULL OR status = $2)
                ORDER BY fit_basis_points DESC, created_at DESC
                LIMIT $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(status.as_deref())
            .bind(i64::from(limit.clamp(1, MAX_CANDIDATE_PAGE)))
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;

            Ok(rows
                .into_iter()
                .map(|row| OutreachCandidateView {
                    id: row.id,
                    target_kind: static_str(&row.target_kind),
                    display_name: row.display_name,
                    source: static_str(&row.source),
                    source_reference: row.source_reference,
                    route_kind: static_str(&row.route_kind),
                    // The route itself never travels in a list. An operator
                    // judging evidence does not need the address, and a
                    // screening queue is read far more often than it is acted
                    // on.
                    evidence: None,
                    status: row.status,
                    refusal_reason: row.refusal_reason,
                    pitch_class: row.pitch_class,
                    fit_basis_points: row.fit_basis_points,
                    follower_count: row.follower_count,
                })
                .collect())
        })
        .await
    }

    async fn confirm_outreach_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate_id: Uuid,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<OutreachCandidatePromotion, RepositoryError> {
        self.bounded(async {
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            if let Some(existing) = super::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "confirm_autopilot_outreach_candidate",
                "outreach_candidate",
                candidate_id,
                idempotency_key,
                request_id,
                &json!({ "candidate_id": candidate_id }),
            )
            .await?
            {
                let promoted = sqlx::query_scalar::<_, Option<Uuid>>(
                    "SELECT promoted_target_id FROM viryaos_outreach_candidates WHERE workspace_id=$1 AND id=$2",
                )
                .bind(workspace_id.into_uuid())
                .bind(candidate_id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(map_sqlx)?
                .flatten();
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(OutreachCandidatePromotion {
                    operation_id: existing,
                    candidate_id,
                    target_id: promoted.map(OutreachTargetId::from_uuid),
                    replayed: true,
                });
            }

            let row = sqlx::query_as::<_, (String, String, String, String)>(
                r#"
                SELECT status, route_kind, route_value, display_name
                FROM viryaos_outreach_candidates
                WHERE workspace_id = $1 AND id = $2
                FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(candidate_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::NotFound)?;

            // A refused candidate is refused for a reason that is recorded on
            // the row. Confirming one would be an operator overriding a scam
            // check by clicking, so it is a conflict rather than a warning.
            if row.0 != "admitted" {
                return Err(RepositoryError::Conflict);
            }
            let route = parse_route_kind(&row.1)?;
            if !promotes_to_target(route) {
                // A form or a handle is a real published route with no pitcher
                // yet. It stays admitted rather than being promoted into a
                // target row that has nowhere to put it.
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(OutreachCandidatePromotion {
                    operation_id,
                    candidate_id,
                    target_id: None,
                    replayed: false,
                });
            }

            let target_kind = sqlx::query_scalar::<_, String>(
                "SELECT target_kind FROM viryaos_outreach_candidates WHERE workspace_id=$1 AND id=$2",
            )
            .bind(workspace_id.into_uuid())
            .bind(candidate_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            // An address the band already holds stays the row it already is:
            // discovery adds supply, and must never quietly reset a
            // relationship's history, score or do-not-contact flag.
            let target_id = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_outreach_targets (
                    workspace_id, target_kind, display_name, contact_email,
                    verified, discovered_from_candidate_id
                ) VALUES ($1,$2,$3,$4,true,$5)
                ON CONFLICT (workspace_id, contact_email) DO UPDATE
                SET discovered_from_candidate_id =
                        COALESCE(viryaos_outreach_targets.discovered_from_candidate_id, $5)
                RETURNING id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(&target_kind)
            .bind(row.3.trim())
            .bind(row.2.trim())
            .bind(candidate_id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            sqlx::query(
                r#"
                UPDATE viryaos_outreach_candidates
                SET status = 'promoted', promoted_target_id = $3, promoted_at = now()
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(candidate_id)
            .bind(target_id)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(OutreachCandidatePromotion {
                operation_id,
                candidate_id,
                target_id: Some(OutreachTargetId::from_uuid(target_id)),
                replayed: false,
            })
        })
        .await
    }

    async fn upsert_submission_channel(
        &self,
        workspace_id: WorkspaceId,
        command: UpsertSubmissionChannel,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<SubmissionChannelMutation, RepositoryError> {
        self.bounded(async {
            if command.slug.trim().is_empty() || command.display_name.trim().is_empty() {
                return Err(RepositoryError::Unexpected);
            }
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            let details = json!({
                "slug": command.slug.trim(),
                "cost_model": channel_cost_str(command.cost_model),
                "active": command.active,
            });
            if let Some(existing) = super::insert_operator_action(
                &mut transaction,
                workspace_id,
                operation_id,
                "upsert_autopilot_submission_channel",
                "outreach_submission_channel",
                workspace_id.into_uuid(),
                idempotency_key,
                request_id,
                &details,
            )
            .await?
            {
                let (channel_id, version) = load_channel_identity(
                    &mut transaction,
                    workspace_id,
                    command.slug.trim(),
                )
                .await?;
                transaction.commit().await.map_err(map_sqlx)?;
                return Ok(SubmissionChannelMutation {
                    operation_id: existing,
                    channel_id,
                    version,
                    replayed: true,
                });
            }

            // The version moves when the channel does, never when it is written
            // again unchanged: a stored version that ticks on every sync is a
            // version nobody can reason about.
            let row = sqlx::query_as::<_, (Uuid, i64)>(
                r#"
                INSERT INTO viryaos_outreach_submission_channels (
                    workspace_id, slug, display_name, cost_model, submission_url, active
                ) VALUES ($1,$2,$3,$4,$5,$6)
                ON CONFLICT (workspace_id, slug) DO UPDATE
                SET display_name = EXCLUDED.display_name,
                    cost_model = EXCLUDED.cost_model,
                    submission_url = EXCLUDED.submission_url,
                    active = EXCLUDED.active,
                    version = viryaos_outreach_submission_channels.version + 1
                WHERE viryaos_outreach_submission_channels.display_name IS DISTINCT FROM EXCLUDED.display_name
                   OR viryaos_outreach_submission_channels.cost_model IS DISTINCT FROM EXCLUDED.cost_model
                   OR viryaos_outreach_submission_channels.submission_url IS DISTINCT FROM EXCLUDED.submission_url
                   OR viryaos_outreach_submission_channels.active IS DISTINCT FROM EXCLUDED.active
                RETURNING id, version
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(command.slug.trim())
            .bind(command.display_name.trim())
            .bind(channel_cost_str(command.cost_model))
            .bind(command.submission_url.as_deref().map(str::trim))
            .bind(command.active)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx)?;

            let (channel_id, version) = match row {
                Some(row) => row,
                None => {
                    load_channel_identity(&mut transaction, workspace_id, command.slug.trim())
                        .await?
                }
            };
            transaction.commit().await.map_err(map_sqlx)?;
            Ok(SubmissionChannelMutation {
                operation_id,
                channel_id,
                version,
                replayed: false,
            })
        })
        .await
    }
}

async fn load_channel(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    slug: &str,
) -> Result<(Uuid, crowdrelay_domain::target_discovery::ChannelCost), RepositoryError> {
    let row = sqlx::query_as::<_, (Uuid, String)>(
        r#"
        SELECT id, cost_model
        FROM viryaos_outreach_submission_channels
        WHERE workspace_id = $1 AND slug = $2 AND active
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(slug.trim())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::NotFound)?;
    Ok((row.0, parse_channel_cost(&row.1)?))
}

async fn load_channel_identity(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: WorkspaceId,
    slug: &str,
) -> Result<(Uuid, i64), RepositoryError> {
    sqlx::query_as::<_, (Uuid, i64)>(
        "SELECT id, version FROM viryaos_outreach_submission_channels WHERE workspace_id=$1 AND slug=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(slug)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?
    .ok_or(RepositoryError::NotFound)
}

const fn channel_cost_str(cost: crowdrelay_domain::target_discovery::ChannelCost) -> &'static str {
    use crowdrelay_domain::target_discovery::ChannelCost;
    match cost {
        ChannelCost::Free => "free",
        ChannelCost::Credit => "credit",
        ChannelCost::Fee => "fee",
        ChannelCost::PaidPlacement => "paid_placement",
    }
}

fn parse_channel_cost(
    value: &str,
) -> Result<crowdrelay_domain::target_discovery::ChannelCost, RepositoryError> {
    use crowdrelay_domain::target_discovery::ChannelCost;
    match value {
        "free" => Ok(ChannelCost::Free),
        "credit" => Ok(ChannelCost::Credit),
        "fee" => Ok(ChannelCost::Fee),
        "paid_placement" => Ok(ChannelCost::PaidPlacement),
        _ => Err(RepositoryError::Unexpected),
    }
}

const fn candidate_source_str(
    source: crowdrelay_domain::target_discovery::CandidateSource,
) -> &'static str {
    use crowdrelay_domain::target_discovery::CandidateSource;
    match source {
        CandidateSource::PlaylistDescription => "playlist_description",
        CandidateSource::CuratorSite => "curator_site",
        CandidateSource::SubmissionChannel => "submission_channel",
        CandidateSource::Reply => "reply",
        CandidateSource::OperatorImport => "operator_import",
        CandidateSource::SceneAdjacentPlaylist => "scene_adjacent_playlist",
    }
}

const fn route_kind_str(route: RouteKind) -> &'static str {
    match route {
        RouteKind::Email => "email",
        RouteKind::SubmissionForm => "submission_form",
        RouteKind::Handle => "handle",
    }
}

fn parse_route_kind(value: &str) -> Result<RouteKind, RepositoryError> {
    match value {
        "email" => Ok(RouteKind::Email),
        "submission_form" => Ok(RouteKind::SubmissionForm),
        "handle" => Ok(RouteKind::Handle),
        _ => Err(RepositoryError::Unexpected),
    }
}

const fn target_kind_str(kind: OutreachTargetKind) -> &'static str {
    match kind {
        OutreachTargetKind::Playlist => "playlist",
        OutreachTargetKind::Radio => "radio",
        OutreachTargetKind::Press => "press",
        OutreachTargetKind::Creator => "creator",
        OutreachTargetKind::SupportSlot => "support_slot",
        OutreachTargetKind::Endorsement => "endorsement",
        OutreachTargetKind::MediaPatronage => "media_patronage",
    }
}

/// Read models return the stored discriminant, and the stored set is closed by
/// a CHECK constraint. An unknown value means the constraint was changed
/// without this code, so it reads back as `unknown` rather than panicking on
/// data that is already durable.
fn static_str(value: &str) -> &'static str {
    match value {
        "playlist" => "playlist",
        "radio" => "radio",
        "press" => "press",
        "creator" => "creator",
        "support_slot" => "support_slot",
        "endorsement" => "endorsement",
        "media_patronage" => "media_patronage",
        "playlist_description" => "playlist_description",
        "curator_site" => "curator_site",
        "submission_channel" => "submission_channel",
        "reply" => "reply",
        "operator_import" => "operator_import",
        "scene_adjacent_playlist" => "scene_adjacent_playlist",
        "email" => "email",
        "submission_form" => "submission_form",
        "handle" => "handle",
        _ => "unknown",
    }
}
