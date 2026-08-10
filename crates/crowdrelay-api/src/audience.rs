//! First-party fan intelligence, segmentation, analytics and communication intent.
//!
//! This plane is intentionally read-heavy. It never sends provider mail in the
//! request path. Scheduling inserts one durable outbox event whose `available_at`
//! is the requested send time; downstream adapters resolve recipients only after
//! the event becomes due.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, Postgres, Transaction};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{Problem, request_id};

const PRIVATE_NO_STORE: &str = "private, no-store";
const MAX_LIST_LIMIT: i64 = 200;
const MAX_DELIVERY_PLAN_LIMIT: i64 = 500;

#[derive(Debug, Serialize, FromRow)]
pub struct AudienceOverview {
    active_fans: i64,
    marketing_consented_fans: i64,
    ticket_buyers: i64,
    attendees: i64,
    synesthesia_participants: i64,
    qualified_referrals: i64,
    paid_ticket_orders: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanListQuery {
    limit: Option<i64>,
    search: Option<String>,
    city_slug: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct FanCard {
    id: Uuid,
    email: String,
    display_name: Option<String>,
    locale: Option<String>,
    status: String,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    qualified_referrals: i64,
    event_interests: i64,
    attended_events: i64,
    paid_ticket_orders: i64,
    synesthesia_entries: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct AcquisitionTouch {
    source: String,
    campaign_name: Option<String>,
    occurred_at: OffsetDateTime,
}

#[derive(Debug, Serialize, FromRow)]
pub struct EventInterestTouch {
    event_slug: String,
    event_title: String,
    created_at: OffsetDateTime,
}

#[derive(Debug, Serialize, FromRow)]
pub struct AttendanceTouch {
    event_slug: String,
    event_title: String,
    status: String,
    redeemed_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct TicketPurchase {
    order_reference: String,
    event_slug: String,
    event_title: String,
    status: String,
    currency: String,
    amount_gross_minor: i64,
    amount_refunded_minor: i64,
    paid_at: Option<OffsetDateTime>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct RewardTouch {
    reward_name: String,
    reward_type: String,
    status: String,
    created_at: OffsetDateTime,
}

#[derive(Debug, Serialize, FromRow)]
pub struct SynesthesiaTouch {
    campaign_slug: String,
    entered_at: OffsetDateTime,
    completed_at: Option<OffsetDateTime>,
    client_total_elapsed_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct FanDetail {
    fan: FanCard,
    acquisitions: Vec<AcquisitionTouch>,
    event_interests: Vec<EventInterestTouch>,
    attendance: Vec<AttendanceTouch>,
    ticket_purchases: Vec<TicketPurchase>,
    rewards: Vec<RewardTouch>,
    synesthesia: Vec<SynesthesiaTouch>,
    tags: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudienceFilter {
    statuses: Vec<String>,
    city_slugs: Vec<String>,
    min_qualified_referrals: Option<i64>,
    interested_event_slugs: Vec<String>,
    attended_event_slugs: Vec<String>,
    purchased_event_slugs: Vec<String>,
    excluded_purchased_event_slugs: Vec<String>,
    synesthesia_completed: Option<bool>,
    marketing_consent: Option<bool>,
    tags_all: Vec<String>,
}

impl AudienceFilter {
    fn validate(&self) -> bool {
        self.statuses.iter().all(|value| {
            matches!(
                value.as_str(),
                "pending" | "active" | "unsubscribed" | "suppressed"
            )
        }) && self.city_slugs.iter().all(|value| valid_slug(value))
            && self
                .interested_event_slugs
                .iter()
                .all(|value| valid_slug(value))
            && self
                .attended_event_slugs
                .iter()
                .all(|value| valid_slug(value))
            && self
                .purchased_event_slugs
                .iter()
                .all(|value| valid_slug(value))
            && self
                .excluded_purchased_event_slugs
                .iter()
                .all(|value| valid_slug(value))
            && self
                .min_qualified_referrals
                .is_none_or(|value| (0..=1_000_000).contains(&value))
            && self.tags_all.iter().all(|value| valid_tag(value))
            && self.statuses.len() <= 4
            && self.city_slugs.len() <= 50
            && self.interested_event_slugs.len() <= 50
            && self.attended_event_slugs.len() <= 50
            && self.purchased_event_slugs.len() <= 50
            && self.excluded_purchased_event_slugs.len() <= 50
            && self.tags_all.len() <= 50
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSegmentRequest {
    slug: String,
    name: String,
    description: Option<String>,
    #[serde(default)]
    filter: AudienceFilter,
}

#[derive(Debug, Serialize, FromRow)]
pub struct AudienceSegment {
    id: Uuid,
    slug: String,
    name: String,
    description: Option<String>,
    filter: Value,
    active: bool,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewQuery {
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct SegmentPreview {
    segment: AudienceSegment,
    total: i64,
    sample: Vec<FanCard>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagRequest {
    tag: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCommunicationCampaignRequest {
    slug: String,
    name: String,
    channel: String,
    segment_slug: String,
    template_key: String,
    subject: Option<String>,
    #[serde(default)]
    content: Value,
}

#[derive(Debug, Serialize, FromRow)]
pub struct CommunicationCampaign {
    id: Uuid,
    slug: String,
    name: String,
    channel: String,
    segment_slug: String,
    template_key: String,
    subject: Option<String>,
    content: Value,
    status: String,
    scheduled_at: Option<OffsetDateTime>,
    dispatch_event_id: Option<Uuid>,
    recipient_count: Option<i32>,
    delivered_count: Option<i32>,
    failed_count: Option<i32>,
    completed_at: Option<OffsetDateTime>,
    cancelled_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleCampaignRequest {
    scheduled_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteCampaignRequest {
    recipient_count: i32,
    delivered_count: i32,
    failed_count: i32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryPlanQuery {
    limit: Option<i64>,
    after_fan_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct DeliveryPlan {
    campaign: CommunicationCampaign,
    recipients: Vec<DeliveryRecipient>,
    next_after_fan_id: Option<Uuid>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct DeliveryRecipient {
    fan_id: Uuid,
    email: String,
    display_name: Option<String>,
    locale: Option<String>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct FunnelRow {
    source: String,
    acquired_fans: i64,
    active_fans: i64,
    ticket_buyers: i64,
    attendees: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct RevenueRow {
    currency: String,
    paid_orders: i64,
    gross_paid_minor: i64,
    refunded_minor: i64,
    after_refunds_minor: i64,
}

pub async fn overview(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let result = sqlx::query_as::<_, AudienceOverview>(
        r#"
        SELECT
            (SELECT count(*)::bigint FROM fans f
             WHERE f.workspace_id = $1 AND f.status = 'active') AS active_fans,
            (SELECT count(*)::bigint
             FROM fans f
             WHERE f.workspace_id = $1
               AND f.status = 'active'
               AND EXISTS (
                   SELECT 1
                   FROM fan_consents fc
                   WHERE fc.workspace_id = f.workspace_id
                     AND fc.fan_id = f.id
                     AND fc.purpose = 'marketing'
                     AND fc.granted
                     AND fc.id = (
                         SELECT newest.id
                         FROM fan_consents newest
                         WHERE newest.workspace_id = fc.workspace_id
                           AND newest.fan_id = fc.fan_id
                           AND newest.purpose = fc.purpose
                         ORDER BY newest.recorded_at DESC, newest.id DESC
                         LIMIT 1
                     )
               )) AS marketing_consented_fans,
            (SELECT count(DISTINCT f.id)::bigint
             FROM fans f
             JOIN ticket_orders orders
               ON orders.workspace_id = f.workspace_id
              AND orders.buyer_email = f.normalized_email
             WHERE f.workspace_id = $1
               AND orders.status IN ('paid', 'partially_refunded', 'refunded')) AS ticket_buyers,
            (SELECT count(DISTINCT passes.fan_id)::bigint
             FROM admission_passes passes
             WHERE passes.workspace_id = $1
               AND passes.status = 'redeemed') AS attendees,
            (SELECT count(DISTINCT entries.fan_id)::bigint
             FROM synesthesia_reward_entries entries
             WHERE entries.workspace_id = $1) AS synesthesia_participants,
            (SELECT count(*)::bigint
             FROM referral_attributions referrals
             WHERE referrals.workspace_id = $1
               AND referrals.status = 'qualified') AS qualified_referrals,
            (SELECT count(*)::bigint
             FROM ticket_orders orders
             WHERE orders.workspace_id = $1
               AND orders.status IN ('paid', 'partially_refunded', 'refunded')) AS paid_ticket_orders
        "#,
    )
    .bind(workspace_id)
    .fetch_one(&state.database)
    .await;
    private_json(result, &headers)
}

pub async fn list_fans(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    Query(query): Query<FanListQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(50);
    if !(1..=MAX_LIST_LIMIT).contains(&limit) {
        return bad_request(&headers);
    }
    let search = query
        .search
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if search
        .as_ref()
        .is_some_and(|value| value.chars().count() > 160)
    {
        return bad_request(&headers);
    }
    let city_slug = query
        .city_slug
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if city_slug.as_ref().is_some_and(|value| !valid_slug(value)) {
        return bad_request(&headers);
    }

    let result = load_fan_cards(
        &state,
        state.ticketing.workspace_id().into_uuid(),
        search.as_deref(),
        city_slug.as_deref(),
        limit,
    )
    .await;
    private_json(result, &headers)
}

pub async fn fan_detail(
    State(state): State<crate::AppState>,
    Path(fan_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let fan = load_fan_card(&state, workspace_id, fan_id).await;
    let fan = match fan {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not load fan 360 card");
            return unavailable(&headers);
        }
    };

    let email = fan.email.clone();
    let (acquisitions, event_interests, attendance, ticket_purchases, rewards, synesthesia, tags) = tokio::join!(
        sqlx::query_as::<_, AcquisitionTouch>(
            r#"
            SELECT acquisition.source, campaign.name AS campaign_name, acquisition.occurred_at
            FROM fan_acquisition_events acquisition
            LEFT JOIN campaigns campaign
              ON campaign.workspace_id = acquisition.workspace_id
             AND campaign.id = acquisition.campaign_id
            WHERE acquisition.workspace_id = $1 AND acquisition.fan_id = $2
            ORDER BY acquisition.occurred_at DESC, acquisition.id DESC
            LIMIT 100
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
        sqlx::query_as::<_, EventInterestTouch>(
            r#"
            SELECT event.slug AS event_slug, event.title AS event_title, interest.created_at
            FROM event_interests interest
            JOIN events event
              ON event.workspace_id = interest.workspace_id
             AND event.id = interest.event_id
            WHERE interest.workspace_id = $1 AND interest.fan_id = $2
            ORDER BY interest.created_at DESC, event.id DESC
            LIMIT 100
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
        sqlx::query_as::<_, AttendanceTouch>(
            r#"
            SELECT event.slug AS event_slug, event.title AS event_title,
                   pass.status, pass.redeemed_at
            FROM admission_passes pass
            JOIN events event
              ON event.workspace_id = pass.workspace_id
             AND event.id = pass.event_id
            WHERE pass.workspace_id = $1 AND pass.fan_id = $2
            ORDER BY COALESCE(pass.redeemed_at, pass.issued_at) DESC, pass.id DESC
            LIMIT 100
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
        sqlx::query_as::<_, TicketPurchase>(
            r#"
            SELECT orders.public_reference AS order_reference,
                   event.slug AS event_slug,
                   event.title AS event_title,
                   orders.status,
                   orders.currency::text AS currency,
                   orders.amount_gross_minor,
                   orders.amount_refunded_minor,
                   orders.paid_at
            FROM ticket_orders orders
            JOIN ticket_sales sale
              ON sale.workspace_id = orders.workspace_id
             AND sale.id = orders.ticket_sale_id
            JOIN events event
              ON event.workspace_id = sale.workspace_id
             AND event.id = sale.event_id
            WHERE orders.workspace_id = $1 AND orders.buyer_email = $2
            ORDER BY orders.created_at DESC, orders.id DESC
            LIMIT 100
            "#,
        )
        .bind(workspace_id)
        .bind(&email)
        .fetch_all(&state.database),
        sqlx::query_as::<_, RewardTouch>(
            r#"
            SELECT rule.name AS reward_name, rule.reward_type, grant.status, grant.created_at
            FROM reward_grants grant
            JOIN reward_rules rule
              ON rule.workspace_id = grant.workspace_id
             AND rule.id = grant.reward_rule_id
            WHERE grant.workspace_id = $1 AND grant.fan_id = $2
            ORDER BY grant.created_at DESC, grant.id DESC
            LIMIT 100
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
        sqlx::query_as::<_, SynesthesiaTouch>(
            r#"
            SELECT entry.campaign_slug, entry.entered_at, run.completed_at, run.client_total_elapsed_ms
            FROM synesthesia_reward_entries entry
            JOIN synesthesia_runs run
              ON run.workspace_id = entry.workspace_id
             AND run.id = entry.run_id
            WHERE entry.workspace_id = $1 AND entry.fan_id = $2
            ORDER BY entry.entered_at DESC, entry.id DESC
            LIMIT 50
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT tag
            FROM fan_audience_tags
            WHERE workspace_id = $1 AND fan_id = $2
            ORDER BY tag
            "#,
        )
        .bind(workspace_id)
        .bind(fan_id)
        .fetch_all(&state.database),
    );

    let detail = match (
        acquisitions,
        event_interests,
        attendance,
        ticket_purchases,
        rewards,
        synesthesia,
        tags,
    ) {
        (Ok(a), Ok(i), Ok(att), Ok(t), Ok(r), Ok(synesthesia), Ok(tags)) => FanDetail {
            fan,
            acquisitions: a,
            event_interests: i,
            attendance: att,
            ticket_purchases: t,
            rewards: r,
            synesthesia,
            tags,
        },
        _ => return unavailable(&headers),
    };

    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(detail),
    )
        .into_response()
}

pub async fn add_tag(
    State(state): State<crate::AppState>,
    Path(fan_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<TagRequest>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let tag = payload.tag.trim().to_ascii_lowercase();
    if !valid_tag(&tag) {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not begin fan tag transaction");
            return unavailable(&headers);
        }
    };
    let result = sqlx::query_as::<_, (bool, bool)>(
        r#"
        WITH target AS (
            SELECT id
            FROM fans
            WHERE workspace_id = $1 AND id = $2
        ), inserted AS (
            INSERT INTO fan_audience_tags (workspace_id, fan_id, tag, source)
            SELECT $1, target.id, $3, 'operator'
            FROM target
            ON CONFLICT (workspace_id, fan_id, tag) DO NOTHING
            RETURNING 1
        )
        SELECT EXISTS (SELECT 1 FROM target), EXISTS (SELECT 1 FROM inserted)
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(&tag)
    .fetch_one(&mut *transaction)
    .await;
    let (exists, inserted) = match result {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not tag fan");
            return unavailable(&headers);
        }
    };
    if !exists {
        return Problem::not_found(request_id_value)
            .private()
            .into_response();
    }
    if inserted
        && append_audit(
            &mut transaction,
            workspace_id,
            "audience.tag.added",
            "fan",
            &fan_id.to_string(),
            request_id_value.as_deref(),
            serde_json::json!({ "tag": tag.clone() }),
        )
        .await
        .is_err()
    {
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, %fan_id, "could not commit fan tag transaction");
        return unavailable(&headers);
    }
    (
        if inserted {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(serde_json::json!({ "fan_id": fan_id, "tag": tag })),
    )
        .into_response()
}

pub async fn remove_tag(
    State(state): State<crate::AppState>,
    Path((fan_id, tag)): Path<(Uuid, String)>,
    headers: HeaderMap,
) -> Response {
    let tag = tag.trim().to_ascii_lowercase();
    if !valid_tag(&tag) {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not begin fan untag transaction");
            return unavailable(&headers);
        }
    };
    let deleted = match sqlx::query(
        "DELETE FROM fan_audience_tags WHERE workspace_id = $1 AND fan_id = $2 AND tag = $3",
    )
    .bind(workspace_id)
    .bind(fan_id)
    .bind(&tag)
    .execute(&mut *transaction)
    .await
    {
        Ok(value) => value.rows_affected() == 1,
        Err(error) => {
            tracing::warn!(%error, %fan_id, "could not untag fan");
            return unavailable(&headers);
        }
    };
    if deleted
        && append_audit(
            &mut transaction,
            workspace_id,
            "audience.tag.removed",
            "fan",
            &fan_id.to_string(),
            request_id_value.as_deref(),
            serde_json::json!({ "tag": tag }),
        )
        .await
        .is_err()
    {
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, %fan_id, "could not commit fan untag transaction");
        return unavailable(&headers);
    }
    (StatusCode::NO_CONTENT, [(CACHE_CONTROL, PRIVATE_NO_STORE)]).into_response()
}

pub async fn list_segments(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let result = sqlx::query_as::<_, AudienceSegment>(
        r#"
        SELECT id, slug, name, description, filter, active, created_at, updated_at
        FROM audience_segments
        WHERE workspace_id = $1
        ORDER BY active DESC, name, id
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(&state.database)
    .await;
    private_json(result, &headers)
}

pub async fn create_segment(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateSegmentRequest>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let slug = payload.slug.trim().to_ascii_lowercase();
    let name = payload.name.trim();
    if !valid_slug(&slug)
        || name.is_empty()
        || name.chars().count() > 160
        || payload
            .description
            .as_ref()
            .is_some_and(|value| value.chars().count() > 1000)
        || !payload.filter.validate()
    {
        return bad_request(&headers);
    }
    let filter = match serde_json::to_value(&payload.filter) {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "could not begin audience segment transaction");
            return unavailable(&headers);
        }
    };
    let result = sqlx::query_as::<_, AudienceSegment>(
        r#"
        INSERT INTO audience_segments (workspace_id, slug, name, description, filter)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, slug, name, description, filter, active, created_at, updated_at
        "#,
    )
    .bind(workspace_id)
    .bind(&slug)
    .bind(name)
    .bind(payload.description.as_deref())
    .bind(filter)
    .fetch_one(&mut *transaction)
    .await;
    let segment = match result {
        Ok(value) => value,
        Err(error) if database_conflict(&error) => {
            return Problem::conflict(request_id_value)
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "could not create audience segment");
            return unavailable(&headers);
        }
    };
    if let Err(error) = append_audit(
        &mut transaction,
        workspace_id,
        "audience.segment.created",
        "audience_segment",
        &segment.id.to_string(),
        request_id_value.as_deref(),
        serde_json::json!({ "slug": segment.slug.clone(), "name": segment.name.clone() }),
    )
    .await
    {
        tracing::warn!(%error, "could not audit audience segment creation");
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, "could not commit audience segment transaction");
        return unavailable(&headers);
    }
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(segment),
    )
        .into_response()
}

pub async fn preview_segment(
    State(state): State<crate::AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Query(query): Query<PreviewQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(25);
    if !(1..=MAX_LIST_LIMIT).contains(&limit) || !valid_slug(&slug) {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let segment = match load_segment(&state, workspace_id, &slug).await {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, slug, "could not load audience segment");
            return unavailable(&headers);
        }
    };
    let filter = match serde_json::from_value::<AudienceFilter>(segment.filter.clone()) {
        Ok(value) if value.validate() => value,
        _ => return unavailable(&headers),
    };
    let result = match segment_members(&state, workspace_id, &filter, limit).await {
        Ok((total, sample)) => SegmentPreview {
            segment,
            total,
            sample,
        },
        Err(error) => {
            tracing::warn!(%error, slug, "could not preview audience segment");
            return unavailable(&headers);
        }
    };
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(result),
    )
        .into_response()
}

pub async fn list_campaigns(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let sql =
        campaign_select_sql("WHERE campaign.workspace_id = $1 ORDER BY campaign.created_at DESC");
    let result = sqlx::query_as::<_, CommunicationCampaign>(&sql)
        .bind(state.ticketing.workspace_id().into_uuid())
        .fetch_all(&state.database)
        .await;
    private_json(result, &headers)
}

pub async fn create_campaign(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    payload: Result<Json<CreateCommunicationCampaignRequest>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let slug = payload.slug.trim().to_ascii_lowercase();
    let segment_slug = payload.segment_slug.trim().to_ascii_lowercase();
    let channel = payload.channel.trim().to_ascii_lowercase();
    let name = payload.name.trim();
    let template_key = payload.template_key.trim();
    if !valid_slug(&slug)
        || !valid_slug(&segment_slug)
        || name.is_empty()
        || name.chars().count() > 160
        || template_key.is_empty()
        || template_key.chars().count() > 160
        || !matches!(channel.as_str(), "email" | "push" | "in_app")
        || payload
            .subject
            .as_ref()
            .is_some_and(|value| value.chars().count() > 240)
        || !payload.content.is_object()
    {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "could not begin communication campaign transaction");
            return unavailable(&headers);
        }
    };
    let result = sqlx::query_as::<_, CommunicationCampaign>(
        r#"
        WITH selected_segment AS (
            SELECT id, slug
            FROM audience_segments
            WHERE workspace_id = $1 AND slug = $2 AND active
        ), inserted AS (
            INSERT INTO communication_campaigns (
                workspace_id, segment_id, slug, name, channel,
                template_key, subject, content
            )
            SELECT $1, selected_segment.id, $3, $4, $5, $6, $7, $8
            FROM selected_segment
            RETURNING *
        )
        SELECT inserted.id, inserted.slug, inserted.name, inserted.channel,
               selected_segment.slug AS segment_slug,
               inserted.template_key, inserted.subject, inserted.content,
               inserted.status, inserted.scheduled_at, inserted.dispatch_event_id,
               inserted.recipient_count, inserted.delivered_count, inserted.failed_count,
               inserted.completed_at, inserted.cancelled_at,
               inserted.created_at, inserted.updated_at
        FROM inserted
        JOIN selected_segment ON selected_segment.id = inserted.segment_id
        "#,
    )
    .bind(workspace_id)
    .bind(&segment_slug)
    .bind(&slug)
    .bind(name)
    .bind(&channel)
    .bind(template_key)
    .bind(payload.subject.as_deref())
    .bind(payload.content)
    .fetch_optional(&mut *transaction)
    .await;
    let campaign = match result {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Problem::not_found(request_id_value)
                .private()
                .into_response();
        }
        Err(error) if database_conflict(&error) => {
            return Problem::conflict(request_id_value)
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "could not create communication campaign");
            return unavailable(&headers);
        }
    };
    if let Err(error) = append_audit(
        &mut transaction,
        workspace_id,
        "communication.campaign.created",
        "communication_campaign",
        &campaign.id.to_string(),
        request_id_value.as_deref(),
        serde_json::json!({
            "slug": campaign.slug.clone(),
            "channel": campaign.channel.clone(),
            "segment_slug": campaign.segment_slug.clone(),
            "template_key": campaign.template_key.clone(),
        }),
    )
    .await
    {
        tracing::warn!(%error, "could not audit communication campaign creation");
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, "could not commit communication campaign transaction");
        return unavailable(&headers);
    }
    (
        StatusCode::CREATED,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(campaign),
    )
        .into_response()
}

pub async fn schedule_campaign(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<ScheduleCampaignRequest>, JsonRejection>,
) -> Response {
    match crate::ecosystem::feature_enabled(&state, "communication_campaigns_enabled").await {
        Ok(true) => {}
        Ok(false) => {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "could not read communication campaign feature flag");
            return unavailable(&headers);
        }
    }
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    let scheduled_at = match OffsetDateTime::parse(payload.scheduled_at.trim(), &Rfc3339) {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    if scheduled_at < OffsetDateTime::now_utc() - time::Duration::minutes(1) {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "could not begin campaign scheduling transaction");
            return unavailable(&headers);
        }
    };

    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Uuid,
            String,
            String,
            Option<OffsetDateTime>,
        ),
    >(
        r#"
        SELECT campaign.id, campaign.slug, campaign.channel,
               campaign.segment_id, campaign.template_key,
               campaign.status, campaign.scheduled_at
        FROM communication_campaigns campaign
        WHERE campaign.workspace_id = $1 AND campaign.id = $2
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .fetch_optional(&mut *transaction)
    .await;
    let (id, slug, channel, segment_id, template_key, status, existing_schedule) = match row {
        Ok(Some(value)) => value,
        Ok(None) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not lock communication campaign");
            return unavailable(&headers);
        }
    };
    if status == "scheduled" && existing_schedule == Some(scheduled_at) {
        drop(transaction);
        return campaign_response(&state, workspace_id, campaign_id, &headers).await;
    }
    if status != "draft" {
        return Problem::conflict(request_id(&headers))
            .private()
            .into_response();
    }

    let event_id = match sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO outbox_events (
            workspace_id, event_type, event_version, payload, available_at, request_id
        )
        VALUES (
            $1,
            'communication.campaign_due',
            1,
            jsonb_build_object(
                'campaign_id', $2::uuid,
                'campaign_slug', $3::text,
                'channel', $4::text,
                'segment_id', $5::uuid,
                'template_key', $6::text
            ),
            $7,
            $8
        )
        RETURNING id
        "#,
    )
    .bind(workspace_id)
    .bind(id)
    .bind(&slug)
    .bind(&channel)
    .bind(segment_id)
    .bind(&template_key)
    .bind(scheduled_at)
    .bind(request_id_value.as_deref())
    .fetch_one(&mut *transaction)
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not enqueue communication campaign");
            return unavailable(&headers);
        }
    };

    if let Err(error) = sqlx::query(
        r#"
        UPDATE communication_campaigns
        SET status = 'scheduled', scheduled_at = $3, dispatch_event_id = $4
        WHERE workspace_id = $1 AND id = $2 AND status = 'draft'
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(scheduled_at)
    .bind(event_id)
    .execute(&mut *transaction)
    .await
    {
        tracing::warn!(%error, %campaign_id, "could not persist campaign schedule");
        return unavailable(&headers);
    }

    if let Err(error) = append_audit(
        &mut transaction,
        workspace_id,
        "communication.campaign.scheduled",
        "communication_campaign",
        &campaign_id.to_string(),
        request_id_value.as_deref(),
        serde_json::json!({ "dispatch_event_id": event_id, "scheduled_at": scheduled_at }),
    )
    .await
    {
        tracing::warn!(%error, %campaign_id, "could not audit campaign schedule");
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, %campaign_id, "could not commit campaign schedule");
        return unavailable(&headers);
    }
    campaign_response(&state, workspace_id, campaign_id, &headers).await
}

pub async fn cancel_campaign(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not begin campaign cancellation transaction");
            return unavailable(&headers);
        }
    };
    let cancelled = match sqlx::query_as::<_, (String, Option<Uuid>)>(
        r#"
        UPDATE communication_campaigns
        SET status = 'cancelled', cancelled_at = now()
        WHERE workspace_id = $1
          AND id = $2
          AND status IN ('draft', 'scheduled')
        RETURNING slug, dispatch_event_id
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .fetch_optional(&mut *transaction)
    .await
    {
        Ok(Some(value)) => value,
        Ok(None) => {
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM communication_campaigns WHERE workspace_id = $1 AND id = $2)",
            )
            .bind(workspace_id)
            .bind(campaign_id)
            .fetch_one(&mut *transaction)
            .await
            .unwrap_or(false);
            return if exists {
                Problem::conflict(request_id_value)
                    .private()
                    .into_response()
            } else {
                Problem::not_found(request_id_value)
                    .private()
                    .into_response()
            };
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not cancel communication campaign");
            return unavailable(&headers);
        }
    };
    if let Err(error) = append_audit(
        &mut transaction,
        workspace_id,
        "communication.campaign.cancelled",
        "communication_campaign",
        &campaign_id.to_string(),
        request_id_value.as_deref(),
        serde_json::json!({ "slug": cancelled.0, "dispatch_event_id": cancelled.1 }),
    )
    .await
    {
        tracing::warn!(%error, %campaign_id, "could not audit campaign cancellation");
        return unavailable(&headers);
    }
    if let Err(error) = transaction.commit().await {
        tracing::warn!(%error, %campaign_id, "could not commit campaign cancellation");
        return unavailable(&headers);
    }
    campaign_response(&state, workspace_id, campaign_id, &headers).await
}

pub async fn delivery_plan(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
    Query(query): Query<DeliveryPlanQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(250);
    if !(1..=MAX_DELIVERY_PLAN_LIMIT).contains(&limit) {
        return bad_request(&headers);
    }
    match crate::ecosystem::feature_enabled(&state, "communication_campaigns_enabled").await {
        Ok(true) => {}
        Ok(false) => {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not read communication campaign feature flag");
            return unavailable(&headers);
        }
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let campaign = match load_campaign(&state, workspace_id, campaign_id).await {
        Ok(Some(value)) if value.status == "scheduled" => value,
        Ok(Some(_)) => {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        Ok(None) => {
            return Problem::not_found(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not load campaign delivery plan");
            return unavailable(&headers);
        }
    };
    if campaign
        .scheduled_at
        .is_none_or(|value| value > OffsetDateTime::now_utc() + time::Duration::minutes(1))
    {
        return Problem::conflict(request_id(&headers))
            .private()
            .into_response();
    }
    if campaign.channel == "email" {
        match crate::ecosystem::feature_enabled(&state, "mailer_enabled").await {
            Ok(true) => {}
            Ok(false) => {
                return Problem::conflict(request_id(&headers))
                    .private()
                    .into_response();
            }
            Err(error) => {
                tracing::warn!(%error, %campaign_id, "could not read mailer feature flag");
                return unavailable(&headers);
            }
        }
    }
    let segment = match load_segment(&state, workspace_id, &campaign.segment_slug).await {
        Ok(Some(value)) if value.active => value,
        Ok(_) => {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not load campaign segment");
            return unavailable(&headers);
        }
    };
    let filter = match serde_json::from_value::<AudienceFilter>(segment.filter) {
        Ok(value) if value.validate() => value,
        _ => return unavailable(&headers),
    };
    match ensure_recipient_snapshot(
        &state,
        workspace_id,
        campaign_id,
        &filter,
        campaign.channel.as_str(),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Problem::conflict(request_id(&headers))
                .private()
                .into_response();
        }
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not snapshot campaign recipients");
            return unavailable(&headers);
        }
    }
    let mut recipients = match delivery_recipients(
        &state,
        workspace_id,
        campaign_id,
        campaign.channel.as_str(),
        query.after_fan_id,
        limit + 1,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not resolve campaign recipients");
            return unavailable(&headers);
        }
    };
    let next_after_fan_id = if i64::try_from(recipients.len()).unwrap_or(i64::MAX) > limit {
        recipients.pop();
        recipients.last().map(|recipient| recipient.fan_id)
    } else {
        None
    };
    (
        StatusCode::OK,
        [(CACHE_CONTROL, PRIVATE_NO_STORE)],
        Json(DeliveryPlan {
            campaign,
            recipients,
            next_after_fan_id,
        }),
    )
        .into_response()
}

pub async fn complete_campaign(
    State(state): State<crate::AppState>,
    Path(campaign_id): Path<Uuid>,
    headers: HeaderMap,
    payload: Result<Json<CompleteCampaignRequest>, JsonRejection>,
) -> Response {
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(_) => return bad_request(&headers),
    };
    if !(0..=10_000_000).contains(&payload.recipient_count)
        || !(0..=10_000_000).contains(&payload.delivered_count)
        || !(0..=10_000_000).contains(&payload.failed_count)
        || i64::from(payload.delivered_count) + i64::from(payload.failed_count)
            != i64::from(payload.recipient_count)
    {
        return bad_request(&headers);
    }
    let workspace_id = state.ticketing.workspace_id().into_uuid();
    let request_id_value = request_id(&headers);
    let mut transaction = match state.database.begin().await {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not begin campaign completion transaction");
            return unavailable(&headers);
        }
    };
    let updated = match sqlx::query(
        r#"
        UPDATE communication_campaigns
        SET status = 'completed',
            recipient_count = $3,
            delivered_count = $4,
            failed_count = $5,
            completed_at = now()
        WHERE workspace_id = $1
          AND id = $2
          AND status = 'scheduled'
          AND recipient_snapshot_at IS NOT NULL
          AND recipient_snapshot_count IS NOT NULL
          AND $3 <= recipient_snapshot_count
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(payload.recipient_count)
    .bind(payload.delivered_count)
    .bind(payload.failed_count)
    .execute(&mut *transaction)
    .await
    {
        Ok(value) => value.rows_affected() == 1,
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not complete communication campaign");
            return unavailable(&headers);
        }
    };
    if updated {
        if let Err(error) = append_audit(
            &mut transaction,
            workspace_id,
            "communication.campaign.completed",
            "communication_campaign",
            &campaign_id.to_string(),
            request_id_value.as_deref(),
            serde_json::json!({
                "recipient_count": payload.recipient_count,
                "delivered_count": payload.delivered_count,
                "failed_count": payload.failed_count,
            }),
        )
        .await
        {
            tracing::warn!(%error, %campaign_id, "could not audit campaign completion");
            return unavailable(&headers);
        }
        if let Err(error) = transaction.commit().await {
            tracing::warn!(%error, %campaign_id, "could not commit campaign completion");
            return unavailable(&headers);
        }
        return campaign_response(&state, workspace_id, campaign_id, &headers).await;
    }
    drop(transaction);
    match load_campaign(&state, workspace_id, campaign_id).await {
        Ok(Some(existing))
            if existing.status == "completed"
                && existing.recipient_count == Some(payload.recipient_count)
                && existing.delivered_count == Some(payload.delivered_count)
                && existing.failed_count == Some(payload.failed_count) =>
        {
            (
                StatusCode::OK,
                [(CACHE_CONTROL, PRIVATE_NO_STORE)],
                Json(existing),
            )
                .into_response()
        }
        Ok(Some(_)) => Problem::conflict(request_id_value)
            .private()
            .into_response(),
        Ok(None) => Problem::not_found(request_id_value)
            .private()
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not verify campaign completion replay");
            unavailable(&headers)
        }
    }
}

pub async fn funnel(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let result = sqlx::query_as::<_, FunnelRow>(
        r#"
        WITH first_touch AS (
            SELECT DISTINCT ON (acquisition.fan_id)
                   acquisition.fan_id,
                   COALESCE(campaign.name, acquisition.source) AS source
            FROM fan_acquisition_events acquisition
            LEFT JOIN campaigns campaign
              ON campaign.workspace_id = acquisition.workspace_id
             AND campaign.id = acquisition.campaign_id
            WHERE acquisition.workspace_id = $1
            ORDER BY acquisition.fan_id, acquisition.occurred_at, acquisition.id
        ), fan_rollup AS (
            SELECT
                first_touch.source,
                fan.id AS fan_id,
                fan.status,
                EXISTS (
                    SELECT 1 FROM ticket_orders orders
                    WHERE orders.workspace_id = fan.workspace_id
                      AND orders.buyer_email = fan.normalized_email
                      AND orders.status IN ('paid', 'partially_refunded', 'refunded')
                ) AS bought,
                EXISTS (
                    SELECT 1 FROM admission_passes pass
                    WHERE pass.workspace_id = fan.workspace_id
                      AND pass.fan_id = fan.id
                      AND pass.status = 'redeemed'
                ) AS attended
            FROM first_touch
            JOIN fans fan
              ON fan.workspace_id = $1
             AND fan.id = first_touch.fan_id
        )
        SELECT source,
               count(*)::bigint AS acquired_fans,
               count(*) FILTER (WHERE status = 'active')::bigint AS active_fans,
               count(*) FILTER (WHERE bought)::bigint AS ticket_buyers,
               count(*) FILTER (WHERE attended)::bigint AS attendees
        FROM fan_rollup
        GROUP BY source
        ORDER BY acquired_fans DESC, source
        LIMIT 100
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(&state.database)
    .await;
    private_json(result, &headers)
}

pub async fn revenue(State(state): State<crate::AppState>, headers: HeaderMap) -> Response {
    let result = sqlx::query_as::<_, RevenueRow>(
        r#"
        SELECT orders.currency::text AS currency,
               count(*)::bigint AS paid_orders,
               sum(orders.amount_gross_minor)::bigint AS gross_paid_minor,
               sum(orders.amount_refunded_minor)::bigint AS refunded_minor,
               sum(orders.amount_gross_minor - orders.amount_refunded_minor)::bigint
                   AS after_refunds_minor
        FROM ticket_orders orders
        WHERE orders.workspace_id = $1
          AND orders.status IN ('paid', 'partially_refunded', 'refunded')
        GROUP BY orders.currency
        ORDER BY orders.currency
        "#,
    )
    .bind(state.ticketing.workspace_id().into_uuid())
    .fetch_all(&state.database)
    .await;
    private_json(result, &headers)
}

async fn load_fan_cards(
    state: &crate::AppState,
    workspace_id: Uuid,
    search: Option<&str>,
    city_slug: Option<&str>,
    limit: i64,
) -> Result<Vec<FanCard>, sqlx::Error> {
    sqlx::query_as::<_, FanCard>(
        r#"
        SELECT
            fan.id,
            fan.normalized_email AS email,
            fan.display_name,
            fan.locale,
            fan.status,
            fan.created_at,
            fan.updated_at,
            (SELECT count(*)::bigint FROM referral_attributions referral
             WHERE referral.workspace_id = fan.workspace_id
               AND referral.referrer_fan_id = fan.id
               AND referral.status = 'qualified') AS qualified_referrals,
            (SELECT count(*)::bigint FROM event_interests interest
             WHERE interest.workspace_id = fan.workspace_id
               AND interest.fan_id = fan.id) AS event_interests,
            (SELECT count(DISTINCT pass.event_id)::bigint FROM admission_passes pass
             WHERE pass.workspace_id = fan.workspace_id
               AND pass.fan_id = fan.id
               AND pass.status = 'redeemed') AS attended_events,
            (SELECT count(*)::bigint FROM ticket_orders orders
             WHERE orders.workspace_id = fan.workspace_id
               AND orders.buyer_email = fan.normalized_email
               AND orders.status IN ('paid', 'partially_refunded', 'refunded')) AS paid_ticket_orders,
            (SELECT count(*)::bigint FROM synesthesia_reward_entries entry
             WHERE entry.workspace_id = fan.workspace_id
               AND entry.fan_id = fan.id) AS synesthesia_entries
        FROM fans fan
        WHERE fan.workspace_id = $1
          AND ($2::text IS NULL
               OR fan.normalized_email ILIKE '%' || $2 || '%'
               OR COALESCE(fan.display_name, '') ILIKE '%' || $2 || '%')
          AND ($3::text IS NULL OR EXISTS (
              SELECT 1
              FROM fan_city_interests city_interest
              JOIN cities city ON city.id = city_interest.city_id
              WHERE city_interest.workspace_id = fan.workspace_id
                AND city_interest.fan_id = fan.id
                AND city.slug = $3
          ))
        ORDER BY fan.updated_at DESC, fan.id DESC
        LIMIT $4
        "#,
    )
    .bind(workspace_id)
    .bind(search)
    .bind(city_slug)
    .bind(limit)
    .fetch_all(&state.database)
    .await
}

async fn load_fan_card(
    state: &crate::AppState,
    workspace_id: Uuid,
    fan_id: Uuid,
) -> Result<Option<FanCard>, sqlx::Error> {
    sqlx::query_as::<_, FanCard>(
        r#"
        SELECT
            fan.id,
            fan.normalized_email AS email,
            fan.display_name,
            fan.locale,
            fan.status,
            fan.created_at,
            fan.updated_at,
            (SELECT count(*)::bigint FROM referral_attributions referral
             WHERE referral.workspace_id = fan.workspace_id
               AND referral.referrer_fan_id = fan.id
               AND referral.status = 'qualified') AS qualified_referrals,
            (SELECT count(*)::bigint FROM event_interests interest
             WHERE interest.workspace_id = fan.workspace_id
               AND interest.fan_id = fan.id) AS event_interests,
            (SELECT count(DISTINCT pass.event_id)::bigint FROM admission_passes pass
             WHERE pass.workspace_id = fan.workspace_id
               AND pass.fan_id = fan.id
               AND pass.status = 'redeemed') AS attended_events,
            (SELECT count(*)::bigint FROM ticket_orders orders
             WHERE orders.workspace_id = fan.workspace_id
               AND orders.buyer_email = fan.normalized_email
               AND orders.status IN ('paid', 'partially_refunded', 'refunded')) AS paid_ticket_orders,
            (SELECT count(*)::bigint FROM synesthesia_reward_entries entry
             WHERE entry.workspace_id = fan.workspace_id
               AND entry.fan_id = fan.id) AS synesthesia_entries
        FROM fans fan
        WHERE fan.workspace_id = $1 AND fan.id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(fan_id)
    .fetch_optional(&state.database)
    .await
}

async fn load_segment(
    state: &crate::AppState,
    workspace_id: Uuid,
    slug: &str,
) -> Result<Option<AudienceSegment>, sqlx::Error> {
    sqlx::query_as::<_, AudienceSegment>(
        r#"
        SELECT id, slug, name, description, filter, active, created_at, updated_at
        FROM audience_segments
        WHERE workspace_id = $1 AND slug = $2
        "#,
    )
    .bind(workspace_id)
    .bind(slug)
    .fetch_optional(&state.database)
    .await
}

async fn segment_members(
    state: &crate::AppState,
    workspace_id: Uuid,
    filter: &AudienceFilter,
    limit: i64,
) -> Result<(i64, Vec<FanCard>), sqlx::Error> {
    let ids = segment_member_ids(state, workspace_id, filter, limit).await?;
    let total = segment_member_count(state, workspace_id, filter).await?;
    if ids.is_empty() {
        return Ok((total, Vec::new()));
    }
    let sample = sqlx::query_as::<_, FanCard>(
        r#"
        SELECT
            fan.id,
            fan.normalized_email AS email,
            fan.display_name,
            fan.locale,
            fan.status,
            fan.created_at,
            fan.updated_at,
            (SELECT count(*)::bigint FROM referral_attributions referral
             WHERE referral.workspace_id = fan.workspace_id
               AND referral.referrer_fan_id = fan.id
               AND referral.status = 'qualified') AS qualified_referrals,
            (SELECT count(*)::bigint FROM event_interests interest
             WHERE interest.workspace_id = fan.workspace_id
               AND interest.fan_id = fan.id) AS event_interests,
            (SELECT count(DISTINCT pass.event_id)::bigint FROM admission_passes pass
             WHERE pass.workspace_id = fan.workspace_id
               AND pass.fan_id = fan.id
               AND pass.status = 'redeemed') AS attended_events,
            (SELECT count(*)::bigint FROM ticket_orders orders
             WHERE orders.workspace_id = fan.workspace_id
               AND orders.buyer_email = fan.normalized_email
               AND orders.status IN ('paid', 'partially_refunded', 'refunded')) AS paid_ticket_orders,
            (SELECT count(*)::bigint FROM synesthesia_reward_entries entry
             WHERE entry.workspace_id = fan.workspace_id
               AND entry.fan_id = fan.id) AS synesthesia_entries
        FROM fans fan
        WHERE fan.workspace_id = $1 AND fan.id = ANY($2::uuid[])
        ORDER BY fan.updated_at DESC, fan.id DESC
        "#,
    )
    .bind(workspace_id)
    .bind(ids)
    .fetch_all(&state.database)
    .await?;
    Ok((total, sample))
}

async fn segment_member_count(
    state: &crate::AppState,
    workspace_id: Uuid,
    filter: &AudienceFilter,
) -> Result<i64, sqlx::Error> {
    let sql = format!(
        "SELECT count(*)::bigint FROM fans fan WHERE {}",
        segment_predicate()
    );
    sqlx::query_scalar::<_, i64>(&sql)
        .bind(workspace_id)
        .bind(&filter.statuses)
        .bind(&filter.city_slugs)
        .bind(filter.min_qualified_referrals)
        .bind(&filter.interested_event_slugs)
        .bind(&filter.attended_event_slugs)
        .bind(&filter.purchased_event_slugs)
        .bind(&filter.excluded_purchased_event_slugs)
        .bind(filter.synesthesia_completed)
        .bind(filter.marketing_consent)
        .bind(&filter.tags_all)
        .fetch_one(&state.database)
        .await
}

async fn segment_member_ids(
    state: &crate::AppState,
    workspace_id: Uuid,
    filter: &AudienceFilter,
    limit: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let sql = format!(
        "SELECT fan.id FROM fans fan WHERE {} ORDER BY fan.updated_at DESC, fan.id DESC LIMIT $12",
        segment_predicate()
    );
    sqlx::query_scalar::<_, Uuid>(&sql)
        .bind(workspace_id)
        .bind(&filter.statuses)
        .bind(&filter.city_slugs)
        .bind(filter.min_qualified_referrals)
        .bind(&filter.interested_event_slugs)
        .bind(&filter.attended_event_slugs)
        .bind(&filter.purchased_event_slugs)
        .bind(&filter.excluded_purchased_event_slugs)
        .bind(filter.synesthesia_completed)
        .bind(filter.marketing_consent)
        .bind(&filter.tags_all)
        .bind(limit)
        .fetch_all(&state.database)
        .await
}

async fn ensure_recipient_snapshot(
    state: &crate::AppState,
    workspace_id: Uuid,
    campaign_id: Uuid,
    filter: &AudienceFilter,
    channel: &str,
) -> Result<bool, sqlx::Error> {
    let require_marketing = matches!(channel, "email" | "push");
    let mut transaction = state.database.begin().await?;
    let snapshot_at = sqlx::query_scalar::<_, Option<OffsetDateTime>>(
        r#"
        SELECT recipient_snapshot_at
        FROM communication_campaigns
        WHERE workspace_id = $1 AND id = $2 AND status = 'scheduled'
        FOR UPDATE
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(snapshot_at) = snapshot_at else {
        return Ok(false);
    };
    if snapshot_at.is_some() {
        transaction.commit().await?;
        return Ok(true);
    }

    let sql = format!(
        r#"
        INSERT INTO communication_campaign_recipients (workspace_id, campaign_id, fan_id)
        SELECT $1, $12, fan.id
        FROM fans fan
        WHERE {}
          AND fan.status = 'active'
          AND ($13::boolean = false OR EXISTS (
              SELECT 1
              FROM fan_consents consent
              WHERE consent.workspace_id = fan.workspace_id
                AND consent.fan_id = fan.id
                AND consent.purpose = 'marketing'
                AND consent.granted
                AND consent.id = (
                    SELECT newest.id
                    FROM fan_consents newest
                    WHERE newest.workspace_id = consent.workspace_id
                      AND newest.fan_id = consent.fan_id
                      AND newest.purpose = consent.purpose
                    ORDER BY newest.recorded_at DESC, newest.id DESC
                    LIMIT 1
                )
          ))
        ON CONFLICT (workspace_id, campaign_id, fan_id) DO NOTHING
        "#,
        segment_predicate()
    );
    sqlx::query(&sql)
        .bind(workspace_id)
        .bind(&filter.statuses)
        .bind(&filter.city_slugs)
        .bind(filter.min_qualified_referrals)
        .bind(&filter.interested_event_slugs)
        .bind(&filter.attended_event_slugs)
        .bind(&filter.purchased_event_slugs)
        .bind(&filter.excluded_purchased_event_slugs)
        .bind(filter.synesthesia_completed)
        .bind(filter.marketing_consent)
        .bind(&filter.tags_all)
        .bind(campaign_id)
        .bind(require_marketing)
        .execute(&mut *transaction)
        .await?;

    let snapshot_count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)::bigint
        FROM communication_campaign_recipients
        WHERE workspace_id = $1 AND campaign_id = $2
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .fetch_one(&mut *transaction)
    .await?;
    let snapshot_count = i32::try_from(snapshot_count).unwrap_or(i32::MAX);
    let updated = sqlx::query(
        r#"
        UPDATE communication_campaigns
        SET recipient_snapshot_at = now(), recipient_snapshot_count = $3
        WHERE workspace_id = $1
          AND id = $2
          AND status = 'scheduled'
          AND recipient_snapshot_at IS NULL
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(snapshot_count)
    .execute(&mut *transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Ok(false);
    }
    transaction.commit().await?;
    Ok(true)
}

async fn delivery_recipients(
    state: &crate::AppState,
    workspace_id: Uuid,
    campaign_id: Uuid,
    channel: &str,
    after_fan_id: Option<Uuid>,
    limit: i64,
) -> Result<Vec<DeliveryRecipient>, sqlx::Error> {
    let require_marketing = matches!(channel, "email" | "push");
    sqlx::query_as::<_, DeliveryRecipient>(
        r#"
        SELECT fan.id AS fan_id,
               fan.normalized_email AS email,
               fan.display_name,
               fan.locale
        FROM communication_campaign_recipients snapshot
        JOIN fans fan
          ON fan.workspace_id = snapshot.workspace_id
         AND fan.id = snapshot.fan_id
        WHERE snapshot.workspace_id = $1
          AND snapshot.campaign_id = $2
          AND fan.status = 'active'
          AND ($3::boolean = false OR EXISTS (
              SELECT 1
              FROM fan_consents consent
              WHERE consent.workspace_id = fan.workspace_id
                AND consent.fan_id = fan.id
                AND consent.purpose = 'marketing'
                AND consent.granted
                AND consent.id = (
                    SELECT newest.id
                    FROM fan_consents newest
                    WHERE newest.workspace_id = consent.workspace_id
                      AND newest.fan_id = consent.fan_id
                      AND newest.purpose = consent.purpose
                    ORDER BY newest.recorded_at DESC, newest.id DESC
                    LIMIT 1
                )
          ))
          AND ($4::uuid IS NULL OR fan.id > $4)
        ORDER BY fan.id
        LIMIT $5
        "#,
    )
    .bind(workspace_id)
    .bind(campaign_id)
    .bind(require_marketing)
    .bind(after_fan_id)
    .bind(limit)
    .fetch_all(&state.database)
    .await
}

fn segment_predicate() -> &'static str {
    r#"
        fan.workspace_id = $1
        AND (cardinality($2::text[]) = 0 OR fan.status = ANY($2::text[]))
        AND (cardinality($3::text[]) = 0 OR EXISTS (
            SELECT 1
            FROM fan_city_interests city_interest
            JOIN cities city ON city.id = city_interest.city_id
            WHERE city_interest.workspace_id = fan.workspace_id
              AND city_interest.fan_id = fan.id
              AND city.slug = ANY($3::text[])
        ))
        AND ($4::bigint IS NULL OR (
            SELECT count(*)::bigint
            FROM referral_attributions referral
            WHERE referral.workspace_id = fan.workspace_id
              AND referral.referrer_fan_id = fan.id
              AND referral.status = 'qualified'
        ) >= $4)
        AND (cardinality($5::text[]) = 0 OR EXISTS (
            SELECT 1
            FROM event_interests interest
            JOIN events event
              ON event.workspace_id = interest.workspace_id
             AND event.id = interest.event_id
            WHERE interest.workspace_id = fan.workspace_id
              AND interest.fan_id = fan.id
              AND event.slug = ANY($5::text[])
        ))
        AND (cardinality($6::text[]) = 0 OR EXISTS (
            SELECT 1
            FROM admission_passes pass
            JOIN events event
              ON event.workspace_id = pass.workspace_id
             AND event.id = pass.event_id
            WHERE pass.workspace_id = fan.workspace_id
              AND pass.fan_id = fan.id
              AND pass.status = 'redeemed'
              AND event.slug = ANY($6::text[])
        ))
        AND (cardinality($7::text[]) = 0 OR EXISTS (
            SELECT 1
            FROM ticket_orders orders
            JOIN ticket_sales sale
              ON sale.workspace_id = orders.workspace_id
             AND sale.id = orders.ticket_sale_id
            JOIN events event
              ON event.workspace_id = sale.workspace_id
             AND event.id = sale.event_id
            WHERE orders.workspace_id = fan.workspace_id
              AND orders.buyer_email = fan.normalized_email
              AND orders.status IN ('paid', 'partially_refunded', 'refunded')
              AND event.slug = ANY($7::text[])
        ))
        AND (cardinality($8::text[]) = 0 OR NOT EXISTS (
            SELECT 1
            FROM ticket_orders excluded_orders
            JOIN ticket_sales excluded_sale
              ON excluded_sale.workspace_id = excluded_orders.workspace_id
             AND excluded_sale.id = excluded_orders.ticket_sale_id
            JOIN events excluded_event
              ON excluded_event.workspace_id = excluded_sale.workspace_id
             AND excluded_event.id = excluded_sale.event_id
            WHERE excluded_orders.workspace_id = fan.workspace_id
              AND excluded_orders.buyer_email = fan.normalized_email
              AND excluded_orders.status IN ('paid', 'partially_refunded')
              AND excluded_event.slug = ANY($8::text[])
        ))
        AND ($9::boolean IS NULL OR EXISTS (
            SELECT 1
            FROM synesthesia_reward_entries entry
            WHERE entry.workspace_id = fan.workspace_id
              AND entry.fan_id = fan.id
        ) = $9)
        AND ($10::boolean IS NULL OR EXISTS (
            SELECT 1
            FROM fan_consents consent
            WHERE consent.workspace_id = fan.workspace_id
              AND consent.fan_id = fan.id
              AND consent.purpose = 'marketing'
              AND consent.granted
              AND consent.id = (
                  SELECT newest.id
                  FROM fan_consents newest
                  WHERE newest.workspace_id = consent.workspace_id
                    AND newest.fan_id = consent.fan_id
                    AND newest.purpose = consent.purpose
                  ORDER BY newest.recorded_at DESC, newest.id DESC
                  LIMIT 1
              )
        ) = $10)
        AND (cardinality($11::text[]) = 0 OR NOT EXISTS (
            SELECT 1
            FROM unnest($11::text[]) required(tag)
            WHERE NOT EXISTS (
                SELECT 1
                FROM fan_audience_tags assigned
                WHERE assigned.workspace_id = fan.workspace_id
                  AND assigned.fan_id = fan.id
                  AND assigned.tag = required.tag
            )
        ))
    "#
}

async fn load_campaign(
    state: &crate::AppState,
    workspace_id: Uuid,
    campaign_id: Uuid,
) -> Result<Option<CommunicationCampaign>, sqlx::Error> {
    let sql = campaign_select_sql("WHERE campaign.workspace_id = $1 AND campaign.id = $2");
    sqlx::query_as::<_, CommunicationCampaign>(&sql)
        .bind(workspace_id)
        .bind(campaign_id)
        .fetch_optional(&state.database)
        .await
}

async fn campaign_response(
    state: &crate::AppState,
    workspace_id: Uuid,
    campaign_id: Uuid,
    headers: &HeaderMap,
) -> Response {
    match load_campaign(state, workspace_id, campaign_id).await {
        Ok(Some(value)) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(value),
        )
            .into_response(),
        Ok(None) => Problem::not_found(request_id(headers))
            .private()
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, %campaign_id, "could not reload communication campaign");
            unavailable(headers)
        }
    }
}

fn campaign_select_sql(where_clause: &str) -> String {
    format!(
        r#"
        SELECT campaign.id, campaign.slug, campaign.name, campaign.channel,
               segment.slug AS segment_slug,
               campaign.template_key, campaign.subject, campaign.content,
               campaign.status, campaign.scheduled_at, campaign.dispatch_event_id,
               campaign.recipient_count, campaign.delivered_count, campaign.failed_count,
               campaign.completed_at, campaign.cancelled_at,
               campaign.created_at, campaign.updated_at
        FROM communication_campaigns campaign
        JOIN audience_segments segment
          ON segment.workspace_id = campaign.workspace_id
         AND segment.id = campaign.segment_id
        {where_clause}
        "#
    )
}

async fn append_audit(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    action: &str,
    target_type: &str,
    target_id: &str,
    request_id_value: Option<&str>,
    metadata: Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO audit_events (
            workspace_id, actor_kind, action, target_type, target_id, request_id, metadata
        )
        VALUES ($1, 'service', $2, $3, $4, $5, $6)
        "#,
    )
    .bind(workspace_id)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(request_id_value)
    .bind(metadata)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn valid_slug(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    value.len() <= 128
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-')
        })
}

fn valid_tag(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    value.len() <= 64
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, ':' | '_' | '-')
        })
}

fn database_conflict(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .is_some_and(|code| matches!(code.as_ref(), "23505" | "23514" | "23503"))
}

fn bad_request(headers: &HeaderMap) -> Response {
    Problem::bad_request(request_id(headers))
        .private()
        .into_response()
}

fn unavailable(headers: &HeaderMap) -> Response {
    Problem::service_unavailable(request_id(headers))
        .private()
        .into_response()
}

fn private_json<T: Serialize>(result: Result<T, sqlx::Error>, headers: &HeaderMap) -> Response {
    match result {
        Ok(value) => (
            StatusCode::OK,
            [(CACHE_CONTROL, PRIVATE_NO_STORE)],
            Json(value),
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "audience intelligence query failed");
            unavailable(headers)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AudienceFilter, valid_slug, valid_tag};

    #[test]
    fn segment_filter_accepts_bounded_artist_segments() {
        let filter = AudienceFilter {
            statuses: vec!["active".to_owned()],
            city_slugs: vec!["wroclaw".to_owned()],
            min_qualified_referrals: Some(2),
            interested_event_slugs: vec!["gorzow-guest-list-2026".to_owned()],
            attended_event_slugs: Vec::new(),
            purchased_event_slugs: Vec::new(),
            excluded_purchased_event_slugs: Vec::new(),
            synesthesia_completed: Some(true),
            marketing_consent: Some(true),
            tags_all: vec!["ambassador".to_owned()],
        };
        assert!(filter.validate());
    }

    #[test]
    fn identifiers_remain_url_and_index_safe() {
        assert!(valid_slug("gorzow-guest-list-2026"));
        assert!(valid_tag("fan:core"));
        assert!(!valid_slug("Gorzów"));
        assert!(!valid_tag("fan core"));
    }
}
