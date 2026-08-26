//! Booking-candidate ingress and the human confirmation that promotes a
//! published route into somebody the agent may approach.
//!
//! Screening happens here, in this transaction, using the domain rule — an
//! adapter cannot pre-screen itself past `paid_to_apply`, and a refused
//! candidate stays refused so no sweep rediscovers it. Dedupe is contact
//! identity: one inbox is one prospect however many sources found it.

use super::*;

use crowdrelay_application::autopilot::{
    AutopilotBookingDiscoveryRepository, BookingCandidateIngestion, BookingCandidateView,
};
use crowdrelay_domain::{
    booking::BookingTargetKind,
    booking_discovery::{
        BookingCandidateInput, BookingDiscoveryPolicy, Screening, screen_candidate,
    },
};

const fn booking_target_kind_str(kind: BookingTargetKind) -> &'static str {
    match kind {
        BookingTargetKind::Venue => "venue",
        BookingTargetKind::Promoter => "promoter",
        BookingTargetKind::Festival => "festival",
    }
}

#[derive(Debug, FromRow)]
struct CandidateRow {
    id: Uuid,
    target_kind: String,
    display_name: String,
    city_slug: Option<String>,
    route_kind: String,
    route_value: String,
    source: String,
    fit_basis_points: i32,
    status: String,
    refusal_reason: Option<String>,
    booking_target_id: Option<Uuid>,
}

#[async_trait]
impl AutopilotBookingDiscoveryRepository for PostgresAutopilotRepository {
    async fn ingest_booking_candidates(
        &self,
        workspace_id: WorkspaceId,
        candidates: Vec<BookingCandidateInput>,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<BookingCandidateIngestion, RepositoryError> {
        self.bounded(async {
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            if let Some(_existing) = insert_operator_action(
                &mut tx,
                workspace_id,
                operation_id,
                "ingest_autopilot_booking_candidates",
                "booking_candidate",
                workspace_id.into_uuid(),
                "executor",
                idempotency_key,
                request_id,
                &json!({"reported": candidates.len()}),
            )
            .await?
            {
                // A replayed batch reports zeros: nothing new was screened.
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(BookingCandidateIngestion {
                    reported: 0,
                    admitted: 0,
                    refused: 0,
                    duplicates: 0,
                });
            }

            let mut admitted = 0u32;
            let mut refused = 0u32;
            let mut duplicates = 0u32;
            let policy = BookingDiscoveryPolicy::default();

            for input in &candidates {
                let (status, reason) = match screen_candidate(input, policy) {
                    Screening::Admit => ("admitted", None),
                    Screening::Refuse(reason) => ("refused", Some(reason.as_str())),
                };
                let inserted = sqlx::query(
                    r#"
                    INSERT INTO viryaos_booking_candidates (
                        workspace_id, target_kind, display_name, city_slug,
                        route_kind, route_value, source, source_reference,
                        evidence, fit_basis_points, capacity, status, refusal_reason
                    ) VALUES ($1,$2,$3,$4,$5,lower(btrim($6)),$7,$8,$9,$10,$11,$12,$13)
                    ON CONFLICT (workspace_id, route_kind, lower(btrim(route_value))) DO NOTHING
                    "#,
                )
                .bind(workspace_id.into_uuid())
                .bind(booking_target_kind_str(input.kind))
                .bind(input.display_name.trim())
                .bind(input.city_slug.as_deref())
                .bind(input.route_kind.as_str())
                .bind(&input.route_value)
                .bind(input.source.trim())
                .bind(input.source_reference.trim())
                .bind(input.evidence.as_deref())
                .bind(i32::from(input.fit_basis_points))
                .bind(input.capacity.map(|c| i32::try_from(c).unwrap_or(i32::MAX)))
                .bind(status)
                .bind(reason)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx)?;
                if inserted.rows_affected() == 0 {
                    duplicates += 1;
                } else if status == "admitted" {
                    admitted += 1;
                } else {
                    refused += 1;
                }
            }
            tx.commit().await.map_err(map_sqlx)?;
            Ok(BookingCandidateIngestion {
                reported: u32::try_from(candidates.len()).unwrap_or(u32::MAX),
                admitted,
                refused,
                duplicates,
            })
        })
        .await
    }

    async fn confirm_booking_candidate(
        &self,
        workspace_id: WorkspaceId,
        candidate_id: OutreachOpportunityId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.bounded(async {
            let mut tx = self.pool.begin().await.map_err(map_sqlx)?;
            let operation_id = Uuid::now_v7();
            if let Some(existing) = insert_operator_action(
                &mut tx,
                workspace_id,
                operation_id,
                "confirm_autopilot_booking_candidate",
                "booking_candidate",
                candidate_id.into_uuid(),
                "executor",
                idempotency_key,
                request_id,
                &json!({"candidate_id": candidate_id}),
            )
            .await?
            {
                tx.commit().await.map_err(map_sqlx)?;
                return Ok(AutopilotControlMutation {
                    operation_id: existing,
                    target_id: candidate_id.into_uuid(),
                    status: "candidate_confirmed".into(),
                    replayed: true,
                });
            }

            // Locked read: only an admitted email route with a resolvable city
            // promotes. Everything else is a NotFound with its own name.
            let row = sqlx::query_as::<_, (String, String, Option<String>, String, String)>(
                r#"
                SELECT target_kind, display_name, city_slug, route_kind, route_value
                FROM viryaos_booking_candidates
                WHERE workspace_id = $1 AND id = $2 AND status = 'admitted'
                FOR UPDATE
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(candidate_id.into_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?
            .ok_or(RepositoryError::NotFound)?;

            let kind = row.0.clone();
            let route_kind = row.3.clone();
            if route_kind != "email" {
                return Err(RepositoryError::Conflict);
            }
            let city_slug = row.2.clone().ok_or(RepositoryError::Conflict)?;
            let city_id = sqlx::query_scalar::<_, Uuid>("SELECT id FROM cities WHERE slug = $1")
                .bind(&city_slug)
                .fetch_optional(&mut *tx)
                .await
                .map_err(map_sqlx)?
                .ok_or(RepositoryError::NotFound)?;

            let target_id_opt = sqlx::query_scalar::<_, Uuid>(
                r#"
                INSERT INTO viryaos_booking_targets (
                    workspace_id, city_id, target_kind, display_name, contact_email
                ) VALUES ($1,$2,$3,$4,lower(btrim($5)))
                ON CONFLICT (workspace_id, city_id, contact_email) DO NOTHING
                RETURNING id
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(city_id)
            .bind(&kind)
            .bind(row.1.trim())
            .bind(&row.4)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx)?;
            // The relationship already existed: link to it rather than reset
            // its history. Promotion never resets anything.
            let target_id = match target_id_opt {
                Some(id) => id,
                None => sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM viryaos_booking_targets \
                     WHERE workspace_id=$1 AND city_id=$2 AND contact_email=lower(btrim($3))",
                )
                .bind(workspace_id.into_uuid())
                .bind(city_id)
                .bind(&row.4)
                .fetch_one(&mut *tx)
                .await
                .map_err(map_sqlx)?,
            };

            sqlx::query(
                r#"
                UPDATE viryaos_booking_candidates
                SET status = 'promoted', promoted_at = now(), booking_target_id = $3
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(candidate_id.into_uuid())
            .bind(target_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx)?;

            tx.commit().await.map_err(map_sqlx)?;
            Ok(AutopilotControlMutation {
                operation_id,
                target_id,
                status: format!("promoted:{target_id}"),
                replayed: false,
            })
        })
        .await
    }

    async fn list_booking_candidates(
        &self,
        workspace_id: WorkspaceId,
        status: Option<String>,
        limit: u32,
    ) -> Result<Vec<BookingCandidateView>, RepositoryError> {
        self.bounded(async {
            if let Some(status) = &status
                && !matches!(status.as_str(), "admitted" | "refused" | "promoted")
            {
                return Err(RepositoryError::Unexpected);
            }
            let rows = sqlx::query_as::<_, CandidateRow>(
                r#"
                SELECT id, target_kind, display_name, city_slug, route_kind,
                       route_value, source, fit_basis_points, status,
                       refusal_reason, booking_target_id
                FROM viryaos_booking_candidates
                WHERE workspace_id = $1
                  AND ($2::text IS NULL OR status = $2)
                ORDER BY created_at DESC, id DESC
                LIMIT $3
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(status.as_deref())
            .bind(i64::from(limit.clamp(1, 100)))
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx)?;
            rows.into_iter()
                .map(|row| {
                    Ok(BookingCandidateView {
                        candidate_id: row.id,
                        target_kind: row.target_kind,
                        display_name: row.display_name,
                        city_slug: row.city_slug,
                        route_kind: row.route_kind,
                        route_value: row.route_value,
                        source: row.source,
                        fit_basis_points: u16::try_from(row.fit_basis_points)
                            .map_err(|_| RepositoryError::Unexpected)?,
                        status: row.status,
                        refusal_reason: row.refusal_reason,
                        booking_target_id: row.booking_target_id,
                    })
                })
                .collect()
        })
        .await
    }
}
