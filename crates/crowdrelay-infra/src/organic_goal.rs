//! An observed monthly acquisition goal. Configuration never counts as progress.
use crowdrelay_domain::{growth_metrics::MetricDirection, objectives::*};
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

mod mutations;
pub use mutations::{GoalError, GoalMutation, declare, exclude};

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct OrganicGoal {
    #[serde(with = "time::serde::rfc3339")]
    pub period_start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub deadline: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub baseline_at: OffsetDateTime,
    pub baseline_fans: i64,
    pub target: i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct OrganicGoalCounts {
    pub confirmed: i64,
    pub verified_signups: i64,
    pub awaiting_confirmation: i64,
    pub excluded: i64,
    pub unverified_arrivals: i64,
    pub activated: i64,
}

#[derive(Debug, Serialize)]
pub struct OrganicGoalView {
    pub goal: OrganicGoal,
    pub counts: OrganicGoalCounts,
    pub assessment: ObjectiveState,
    pub remaining: i64,
    /// Required pace, not a traffic/conversion forecast.
    pub required_per_day: Option<i64>,
}

/// Only the worker renews an enrolled goal. Reads do not enrol tenants or
/// roll a missed target forward. Renewal preserves the prior month's row.
pub async fn renew(pool: &PgPool, workspace: Uuid, now: OffsetDateTime) -> Result<(), sqlx::Error> {
    sqlx::query(RENEW_SQL)
        .bind(workspace)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn read(
    pool: &PgPool,
    workspace: Uuid,
    now: OffsetDateTime,
) -> Result<Option<OrganicGoalView>, sqlx::Error> {
    let goal = sqlx::query_as::<_, OrganicGoal>(
        "SELECT period_start,deadline,baseline_at,baseline_fans,target FROM organic_fan_goals WHERE workspace_id=$1 AND period_start<=$2 ORDER BY period_start DESC LIMIT 1",
    ).bind(workspace).bind(now).fetch_optional(pool).await?;
    let Some(goal) = goal else {
        return Ok(None);
    };
    let counts = sqlx::query_as::<_, OrganicGoalCounts>(COUNTS_SQL)
        .bind(workspace)
        .bind(goal.baseline_at)
        .bind(goal.deadline.min(now + time::Duration::microseconds(1)))
        .bind(now)
        .fetch_one(pool)
        .await?;
    let assessment = assess_objective(
        &GrowthObjective {
            platform: "signal".to_owned(),
            metric_key: "verified_organic_acquisitions".to_owned(),
            scope: ObjectiveScope::Workspace,
            direction: MetricDirection::HigherIsBetter,
            baseline_value: 0,
            target_value: goal.target,
            declared_at: goal.baseline_at,
            deadline: goal.deadline,
        },
        Some((counts.confirmed, now.min(goal.deadline))),
        ObjectivePolicy::default(),
        now,
    );
    let remaining = goal.target.saturating_sub(counts.confirmed).max(0);
    let seconds_left = (goal.deadline - now).whole_seconds();
    let required_per_day = (seconds_left > 0).then(|| {
        let days = (seconds_left + 86399) / 86400;
        (remaining + days - 1) / days
    });
    Ok(Some(OrganicGoalView {
        goal,
        counts,
        assessment,
        remaining,
        required_per_day,
    }))
}

const COUNTS_SQL: &str = r#"
SELECT COUNT(*) FILTER(WHERE verified AND contactable AND NOT excluded)::bigint AS confirmed,
 COUNT(*) FILTER(WHERE verified AND NOT excluded)::bigint AS verified_signups,
 COUNT(*) FILTER(WHERE verified AND NOT contactable AND NOT excluded)::bigint AS awaiting_confirmation,
 COUNT(*) FILTER(WHERE excluded)::bigint AS excluded,
 COUNT(*) FILTER(WHERE NOT verified AND NOT excluded)::bigint AS unverified_arrivals,
 COUNT(*) FILTER(WHERE verified AND contactable AND NOT excluded
   AND fan_has_engagement_between($1,cohort.fan_id,fan.normalized_email,
     acquired_at+INTERVAL '1 microsecond',LEAST(acquired_at+INTERVAL '7 days',$4::timestamptz+INTERVAL '1 microsecond')))::bigint AS activated
FROM organic_fan_cohort($1,$2,$3,$4) cohort
JOIN fans fan ON fan.workspace_id=$1 AND fan.id=cohort.fan_id
"#;

const RENEW_SQL: &str = r#"
WITH previous AS (
 SELECT target FROM organic_fan_goals WHERE workspace_id=$1 AND period_start<=$2
 ORDER BY period_start DESC LIMIT 1
), period AS (
 SELECT date_trunc('month',$2::timestamptz AT TIME ZONE 'Europe/Warsaw') AS local_start
)
INSERT INTO organic_fan_goals(workspace_id,period_start,deadline,baseline_at,baseline_fans,target,declared_by)
SELECT $1,local_start AT TIME ZONE 'Europe/Warsaw',(local_start+INTERVAL '1 month') AT TIME ZONE 'Europe/Warsaw',
 local_start AT TIME ZONE 'Europe/Warsaw',
 (SELECT COUNT(*) FROM fans WHERE workspace_id=$1 AND merged_into_fan_id IS NULL AND created_at<local_start AT TIME ZONE 'Europe/Warsaw'),
 previous.target,'system:monthly_renewal'
FROM previous CROSS JOIN period ON CONFLICT(workspace_id,period_start) DO NOTHING
"#;
