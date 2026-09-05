//! Reading an action's execution state, and deriving whether a persisted
//! `succeeded` has external confirmation.
//!
//! Its own module rather than helpers buried in `runtime.rs`: these are the
//! two facts the success invariant turns on, and they should be findable by
//! name.

use super::*;

/// What the locked `viryaos_autopilot_actions` row says about execution.
///
/// `Unreadable` is not a state — it is the absence of one, kept separate so a
/// caller cannot spend it as if it were `Running`.
pub(super) enum LockedActionState {
    /// The row exists and its status maps to a ledger state. `status` is the
    /// raw column value, carried so the caller can pin its UPDATE to the exact
    /// string the decision was made on.
    Readable { status: String, state: ActionState },
    /// The row is gone, or its status is a word this build does not know.
    Unreadable { status: Option<String> },
}

/// Locks the action row and reads its execution state.
///
/// `FOR UPDATE` because the state read here is the monotonicity guard: the
/// resolver decides from it and the caller then writes pinned to it, and both
/// have to see the same row.
///
/// `from_action_status`, not `parse`: this is the action table's lowercase
/// vocabulary, and `parse` reads the ledger's uppercase one. Feeding an action
/// status to `parse` returned `None` for every legal value, so a caller's
/// fallback became the resolver's only input.
///
/// There is no fallback now. The previous `unwrap_or(ActionState::Running)`
/// read an unknown status as mid-execution, which resolves to `Apply(Failed)`
/// on a failure receipt and `Apply(Succeeded)` on a success one — and the
/// UPDATE is pinned to the very status string that could not be read, so the
/// write lands. Vocabulary drift would have silently rewritten action rows, and
/// on the success arm committed the full set of success side effects, rather
/// than raising.
pub(super) async fn locked_action_state(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: WorkspaceId,
    action_id: AutopilotActionId,
) -> Result<LockedActionState, RepositoryError> {
    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM viryaos_autopilot_actions \
         WHERE workspace_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id.into_uuid())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(map_sqlx)?;

    Ok(match status {
        Some(status) => match ActionState::from_action_status(&status) {
            Some(state) => LockedActionState::Readable { status, state },
            None => LockedActionState::Unreadable {
                status: Some(status),
            },
        },
        None => LockedActionState::Unreadable { status: None },
    })
}

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
