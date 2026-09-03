//! Deriving whether a persisted `succeeded` has external confirmation.
//!
//! Its own module rather than a helper buried in `runtime.rs`: this is the
//! fact the success invariant turns on, and it should be findable by name.

use super::*;

/// Whether this action's persisted `succeeded` has external confirmation.
///
/// Loads the two facts [`SuccessEvidence`] is derived from. Neither is a new
/// column: `payload_requires_executor` is a pure function of the payload the
/// action already carries, and the confirming report is a row
/// `record_execution_report` already writes.
///
/// An unparseable payload is treated as requiring an executor. That is the
/// cautious direction only in one sense — it makes the success correctable
/// rather than protected — so it is logged: a row we cannot read is a row
/// whose confirmation status we are guessing at.
pub(super) async fn success_evidence_for(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    command: &RecordExecutionReport,
) -> Result<SuccessEvidence, RepositoryError> {
    let payload_value = sqlx::query_scalar::<_, Value>(
        "SELECT payload FROM viryaos_autopilot_actions WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace_id.into_uuid())
    .bind(command.action_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;

    let requires_executor = match payload_value
        .map(serde_json::from_value::<AutopilotActionPayload>)
        .transpose()
    {
        Ok(Some(payload)) => payload_requires_executor(&payload),
        Ok(None) => true,
        Err(error) => {
            tracing::warn!(
                error = %error,
                action_id = %command.action_id.into_uuid(),
                "stored autopilot action payload could not be parsed while deciding \
                 whether its success is provider-confirmed; assuming it needs an executor"
            );
            true
        }
    };
    if !requires_executor {
        return Ok(SuccessEvidence::ProviderConfirmed);
    }

    // Excluding this receipt is load-bearing. The report row is inserted
    // before the status is dispatched on, so without the exclusion a success
    // receipt would find itself and every success would look already
    // confirmed — collapsing the distinction this function exists to draw.
    let confirmed = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS ( \
           SELECT 1 FROM viryaos_autopilot_execution_reports \
           WHERE workspace_id=$1 AND action_id=$2 AND status='succeeded' \
             AND provider_reference IS NOT NULL \
             AND receipt_key <> $3 \
         )",
    )
    .bind(workspace_id.into_uuid())
    .bind(command.action_id.into_uuid())
    .bind(&command.receipt_key)
    .fetch_one(&mut **transaction)
    .await
    .map_err(map_sqlx)?;

    Ok(if confirmed {
        SuccessEvidence::ProviderConfirmed
    } else {
        SuccessEvidence::Premature
    })
}
