//! The counts behind K, read from first-party rows only.
//!
//! The windows are staggered on purpose, so a referral has time to prove itself
//! before it is counted as retained:
//!
//! ```text
//!   now-90d ─── cohort active ─── now-60d ─── referrals qualify ─── now-30d ─── retained? ─── now
//! ```
//!
//! A fan is in the **cohort** if they did something meaningful in the first
//! window (the canonical `fan_has_meaningful_action_between`, so "activated"
//! cannot drift from the counter). Their **qualified referrals** are
//! attributions that qualified in the second window. A referral is **retained**
//! if the referred fan was meaningfully active in the third. K therefore lags the
//! present by two months; that is the price of not reporting a referral as
//! retained before there has been time to retain.
//!
//! The Latarnik series restricts the cohort to fans who held an active role by
//! the start of the referral window, and counts only referrals that qualified
//! while they held it.

use crowdrelay_domain::viral_coefficient::KCounts;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// The two series' counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KSeries {
    pub fans: KCounts,
    pub latarnik: KCounts,
}

fn clamp(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

/// # Errors
///
/// Propagates the database error.
pub async fn k_counts(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<KSeries, sqlx::Error> {
    let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64)>(
        r#"
        WITH cohort AS (
            SELECT f.id AS fan_id
            FROM fans f
            WHERE f.workspace_id = $1
              AND f.status = 'active' AND f.deleted_at IS NULL AND f.merged_into_fan_id IS NULL
              AND fan_has_meaningful_action_between(
                    f.workspace_id, f.id, f.normalized_email,
                    $2 - interval '90 days', $2 - interval '60 days')
        ), latarniks AS (
            SELECT fan.id AS fan_id, lr.activated_at
            FROM latarnik_roles lr
            JOIN person_identities pi
              ON pi.workspace_id = lr.workspace_id AND pi.person_id = lr.person_id
             AND pi.kind = 'email' AND pi.platform IS NULL
            JOIN fans fan
              ON fan.workspace_id = pi.workspace_id AND fan.normalized_email = pi.value
             AND fan.status = 'active' AND fan.deleted_at IS NULL
             AND fan.merged_into_fan_id IS NULL
            WHERE lr.workspace_id = $1
              AND lr.status = 'active'
              AND lr.activated_at <= $2 - interval '60 days'
        ), fan_refs AS (
            SELECT ra.referred_fan_id
            FROM referral_attributions ra
            JOIN cohort c ON c.fan_id = ra.referrer_fan_id
            WHERE ra.workspace_id = $1
              AND ra.status = 'qualified'
              AND ra.qualified_at >= $2 - interval '60 days'
              AND ra.qualified_at <  $2 - interval '30 days'
        ), lat_refs AS (
            SELECT ra.referred_fan_id
            FROM referral_attributions ra
            JOIN latarniks l ON l.fan_id = ra.referrer_fan_id
            WHERE ra.workspace_id = $1
              AND ra.status = 'qualified'
              AND ra.qualified_at >= GREATEST(l.activated_at, $2 - interval '60 days')
              AND ra.qualified_at <  $2 - interval '30 days'
        )
        SELECT
          (SELECT count(*) FROM cohort),
          (SELECT count(*) FROM fan_refs),
          (SELECT count(*) FROM fan_refs r
             JOIN fans f ON f.workspace_id = $1 AND f.id = r.referred_fan_id
            WHERE f.status = 'active' AND f.deleted_at IS NULL
              AND fan_has_meaningful_action_between(
                    f.workspace_id, f.id, f.normalized_email,
                    $2 - interval '30 days', $2 + interval '1 second')),
          (SELECT count(*) FROM latarniks),
          (SELECT count(*) FROM lat_refs),
          (SELECT count(*) FROM lat_refs r
             JOIN fans f ON f.workspace_id = $1 AND f.id = r.referred_fan_id
            WHERE f.status = 'active' AND f.deleted_at IS NULL
              AND fan_has_meaningful_action_between(
                    f.workspace_id, f.id, f.normalized_email,
                    $2 - interval '30 days', $2 + interval '1 second'))
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(KSeries {
        fans: KCounts {
            cohort: clamp(row.0),
            qualified_referrals: clamp(row.1),
            retained_referrals: clamp(row.2),
        },
        latarnik: KCounts {
            cohort: clamp(row.3),
            qualified_referrals: clamp(row.4),
            retained_referrals: clamp(row.5),
        },
    })
}
