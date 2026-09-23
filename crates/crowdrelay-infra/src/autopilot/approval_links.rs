// §6-C: the reads and mutations behind the mailed one-click approval links.
//
// The links render and decide through the same `control_action_transition`
// the admin API runs — the only differences are who the ledger says answered
// (`email-link`, not `operator:admin_api_key`) and that a skip stamps
// `last_error_kind = 'skipped_by_email'` so the queue can tell a declined ask
// from a cancelled one. Everything else — the operator_actions audit row, the
// idempotent replay, the outward hold — is the admin path verbatim.

/// What the public approval page needs and nothing else: the action's kind
/// and payload to describe the ask, its status to decide whether the page is
/// still live, and the expiry the link was minted against.
pub struct ApprovalLinkView {
    pub action_kind: String,
    pub payload: serde_json::Value,
    pub status: String,
    pub approval_expires_at: Option<OffsetDateTime>,
}

impl PostgresAutopilotRepository {
    /// Loads the action a token points at. `None` is "no such action", which
    /// the route renders as 404 — a token can only ever exist for an action
    /// that did, so an absent row means a forged or stale id, not a viewer.
    pub async fn approval_link_view(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
    ) -> Result<Option<ApprovalLinkView>, RepositoryError> {
        self.bounded(async {
            sqlx::query_as::<_, (String, serde_json::Value, String, Option<OffsetDateTime>)>(
                r#"
                SELECT action_kind, payload, status, approval_expires_at
                FROM autopilot_actions
                WHERE workspace_id = $1 AND id = $2
                "#,
            )
            .bind(workspace_id.into_uuid())
            .bind(action_id.into_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)
            .map(|row| {
                row.map(
                    |(action_kind, payload, status, approval_expires_at)| ApprovalLinkView {
                        action_kind,
                        payload,
                        status,
                        approval_expires_at,
                    },
                )
            })
        })
        .await
    }

    /// Approves through the identical transition the admin API runs, stamped
    /// `email-link` so the ledger records how the answer arrived.
    pub async fn approve_action_from_email_link(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.control_action_transition(
            workspace_id,
            action_id,
            idempotency_key,
            request_id,
            "approve_autopilot_action",
            "queued",
            None,
            "email-link",
            None,
        )
        .await
    }

    /// Skips through the same transition the admin cancel runs, with
    /// `skipped_by_email` marking it a declined ask rather than a withdrawn
    /// one — the briefing's dropped-ask accounting reads that difference.
    pub async fn skip_action_from_email_link(
        &self,
        workspace_id: WorkspaceId,
        action_id: AutopilotActionId,
        idempotency_key: &IdempotencyKey,
        request_id: Option<&RequestId>,
    ) -> Result<AutopilotControlMutation, RepositoryError> {
        self.control_action_transition(
            workspace_id,
            action_id,
            idempotency_key,
            request_id,
            "skip_autopilot_action",
            "cancelled",
            None,
            "email-link",
            Some("skipped_by_email"),
        )
        .await
    }
}
