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

include!("audience/engagement_handlers.rs");
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
