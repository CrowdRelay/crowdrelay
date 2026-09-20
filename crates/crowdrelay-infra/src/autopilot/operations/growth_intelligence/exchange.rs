//! The causal model's learned revenue-per-fan exchange rate, refreshed
//! from first-party sales each load.

use super::super::super::*;

/// Feeds the tenant's realized revenue-per-fan ratio into the causal model.
///
/// One row per complete day over the trailing 90 days: ticket-order gross
/// net of refunds plus merch goods gross, and the fans gained the same day.
/// `ValueExchange::observe` dedupes by day, so re-scanning the window every
/// load is idempotent — only days newer than the checkpoint's cursor move
/// the sums. Today's partial bucket is excluded: a half-finished day would
/// land in the rate with less revenue and fewer fans than it will end with,
/// and the exchange does not get to learn from days that have not closed.
///
/// A query failure degrades to a warn — the exchange stays at whatever the
/// checkpoint held, which is a rate from real history or no rate at all,
/// never a stalled pipeline's zero.
pub(super) async fn refresh_value_exchange(
    repo: &PostgresAutopilotRepository,
    workspace_id: WorkspaceId,
    model: &mut crowdrelay_brain::CausalModel,
) {
    let rows: Result<Vec<(time::Date, i64, i64)>, _> = sqlx::query_as(
        r#"
        SELECT day::date AS day,
               SUM(revenue_minor)::bigint AS revenue_minor,
               SUM(new_fans)::bigint AS new_fans
        FROM (
            SELECT date_trunc('day', o.paid_at) AS day,
                   o.amount_gross_minor - o.amount_refunded_minor AS revenue_minor,
                   0::bigint AS new_fans
            FROM ticket_orders o
            WHERE o.workspace_id = $1
              AND o.status IN ('paid','partially_refunded')
              AND o.paid_at > now() - interval '90 days'
            UNION ALL
            SELECT date_trunc('day', m.confirmed_at) AS day,
                   m.goods_gross_minor AS revenue_minor,
                   0::bigint AS new_fans
            FROM merch_order_facts m
            WHERE m.workspace_id = $1
              AND m.confirmed_at > now() - interval '90 days'
            UNION ALL
            SELECT date_trunc('day', f.created_at) AS day,
                   0::bigint AS revenue_minor,
                   1::bigint AS new_fans
            FROM fans f
            WHERE f.workspace_id = $1
              AND f.status != 'suppressed'
              AND f.created_at > now() - interval '90 days'
        ) buckets
        WHERE day < date_trunc('day', now())
        GROUP BY day
        ORDER BY day
        "#,
    )
    .bind(workspace_id.into_uuid())
    .fetch_all(&repo.pool)
    .await;

    match rows {
        Ok(rows) => {
            let before = model.value_exchange.days_observed;
            for (day, revenue_minor, new_fans) in rows {
                model.value_exchange.observe(
                    i64::from(day.to_julian_day()),
                    revenue_minor as f64,
                    new_fans as f64,
                );
            }
            let folded = model.value_exchange.days_observed - before;
            if folded > 0 {
                tracing::info!(
                    days_folded = folded,
                    minor_per_fan = ?model.value_exchange.minor_per_fan(),
                    "value exchange updated"
                );
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "value exchange refresh failed; using checkpointed rate");
        }
    }
}
