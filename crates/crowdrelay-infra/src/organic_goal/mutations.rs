use super::*;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

#[derive(Debug, thiserror::Error)]
pub enum GoalError {
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("goal or idempotency key conflicts with existing facts")]
    Conflict,
    #[error("fan does not exist in this workspace")]
    NotFound,
}

#[derive(Debug, Serialize)]
pub struct GoalMutation {
    pub operation_id: Uuid,
    pub replayed: bool,
}

async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Uuid,
    target: Uuid,
    action: &str,
    key: &str,
    request: Option<&str>,
    details: Value,
) -> Result<GoalMutation, GoalError> {
    let operation_id = Uuid::now_v7();
    let inserted=sqlx::query_scalar::<_,Uuid>(
        "INSERT INTO operator_actions(id,workspace_id,action,target_type,target_id,idempotency_key,request_id,details) VALUES($1,$2,$3,'organic_growth',$4,$5,$6,$7) ON CONFLICT(workspace_id,idempotency_key) DO NOTHING RETURNING id",
    ).bind(operation_id).bind(workspace).bind(action).bind(target).bind(key).bind(request).bind(&details)
      .fetch_optional(&mut **tx).await?;
    if inserted.is_some() {
        return Ok(GoalMutation {
            operation_id,
            replayed: false,
        });
    }
    let (id,old_action,old_target,old_details)=sqlx::query_as::<_,(Uuid,String,Uuid,Value)>(
        "SELECT id,action,target_id,details FROM operator_actions WHERE workspace_id=$1 AND idempotency_key=$2",
    ).bind(workspace).bind(key).fetch_one(&mut **tx).await?;
    if old_action != action || old_target != target || old_details != details {
        return Err(GoalError::Conflict);
    }
    Ok(GoalMutation {
        operation_id: id,
        replayed: true,
    })
}

pub async fn declare(
    pool: &PgPool,
    workspace: Uuid,
    target: i64,
    actor: &str,
    key: &str,
    request: Option<&str>,
    now: OffsetDateTime,
) -> Result<GoalMutation, GoalError> {
    let mut tx = pool.begin().await?;
    let period:OffsetDateTime=sqlx::query_scalar("SELECT date_trunc('month',$1::timestamptz AT TIME ZONE 'Europe/Warsaw') AT TIME ZONE 'Europe/Warsaw'")
      .bind(now).fetch_one(&mut *tx).await?;
    let result=audit(&mut tx,workspace,workspace,"declare_organic_goal",key,request,
      json!({"target":target,"actor":actor,"period_start":period.format(&time::format_description::well_known::Rfc3339).ok()})).await?;
    if !result.replayed {
        let inserted=sqlx::query_scalar::<_,i64>(
            "INSERT INTO organic_fan_goals(workspace_id,period_start,deadline,baseline_at,baseline_fans,target,declared_by) SELECT $1,$2,(($2::timestamptz AT TIME ZONE 'Europe/Warsaw')+INTERVAL '1 month') AT TIME ZONE 'Europe/Warsaw',$3,(SELECT COUNT(*) FROM fans WHERE workspace_id=$1 AND merged_into_fan_id IS NULL AND created_at<$3),$4,$5 ON CONFLICT(workspace_id,period_start) DO NOTHING RETURNING target",
        ).bind(workspace).bind(period).bind(now).bind(target).bind(actor).fetch_optional(&mut *tx).await?;
        if inserted.is_some() {
            // Freeze the seeded audience once. Re-declaration never excludes
            // new organic people who arrived after the first declaration.
            sqlx::query("INSERT INTO organic_fan_exclusions(workspace_id,fan_id,reason,recorded_by,recorded_at) SELECT $1,id,'baseline',$2,$3 FROM fans WHERE workspace_id=$1 AND created_at<$3 ON CONFLICT(workspace_id,fan_id) DO NOTHING")
              .bind(workspace).bind(actor).bind(now).execute(&mut *tx).await?;
        } else {
            let stored: i64 = sqlx::query_scalar(
                "SELECT target FROM organic_fan_goals WHERE workspace_id=$1 AND period_start=$2",
            )
            .bind(workspace)
            .bind(period)
            .fetch_one(&mut *tx)
            .await?;
            if stored != target {
                return Err(GoalError::Conflict);
            }
        }
    }
    tx.commit().await?;
    Ok(result)
}

/// One operator exclusion of a fan from the verified-organic count: who, why.
pub struct Exclusion<'a> {
    pub fan: Uuid,
    pub reason: Option<&'a str>,
    pub actor: &'a str,
}

pub async fn exclude(
    pool: &PgPool,
    workspace: Uuid,
    exclusion: Exclusion<'_>,
    key: &str,
    request: Option<&str>,
    now: OffsetDateTime,
) -> Result<GoalMutation, GoalError> {
    let Exclusion { fan, reason, actor } = exclusion;
    let mut tx = pool.begin().await?;
    if sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM fans WHERE workspace_id=$1 AND id=$2 FOR SHARE",
    )
    .bind(workspace)
    .bind(fan)
    .fetch_optional(&mut *tx)
    .await?
    .is_none()
    {
        return Err(GoalError::NotFound);
    }
    let result = audit(
        &mut tx,
        workspace,
        fan,
        "set_organic_exclusion",
        key,
        request,
        json!({"reason":reason,"actor":actor}),
    )
    .await?;
    if !result.replayed {
        if let Some(reason) = reason {
            sqlx::query("INSERT INTO organic_fan_exclusions(workspace_id,fan_id,reason,recorded_by,recorded_at) VALUES($1,$2,$3,$4,$5) ON CONFLICT(workspace_id,fan_id) DO UPDATE SET reason=EXCLUDED.reason,recorded_by=EXCLUDED.recorded_by,recorded_at=EXCLUDED.recorded_at")
              .bind(workspace).bind(fan).bind(reason).bind(actor).bind(now).execute(&mut *tx).await?;
        } else {
            sqlx::query("DELETE FROM organic_fan_exclusions WHERE workspace_id=$1 AND fan_id=$2")
                .bind(workspace)
                .bind(fan)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(result)
}
