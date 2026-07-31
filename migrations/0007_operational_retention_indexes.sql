-- Keep bounded retention work index-driven as the operational tables grow.

CREATE INDEX fan_sessions_terminal_retention_idx
    ON fan_sessions (
        LEAST(expires_at, COALESCE(revoked_at, expires_at)),
        id
    );

CREATE INDEX pass_sessions_terminal_retention_idx
    ON pass_sessions (
        LEAST(expires_at, COALESCE(revoked_at, expires_at)),
        id
    );

CREATE INDEX workspace_member_sessions_terminal_retention_idx
    ON workspace_member_sessions (
        LEAST(expires_at, COALESCE(revoked_at, expires_at)),
        id
    );

CREATE INDEX pass_redemptions_staff_session_retention_idx
    ON pass_redemptions (workspace_id, staff_session_id);

CREATE INDEX fan_action_tokens_expiry_retention_idx
    ON fan_action_tokens (expires_at, id);

CREATE INDEX fan_action_tokens_consumed_retention_idx
    ON fan_action_tokens (consumed_at, id)
    WHERE consumed_at IS NOT NULL;

CREATE INDEX admission_passes_claim_expiry_retention_idx
    ON admission_passes (claim_expires_at, id)
    WHERE status = 'issued';

CREATE INDEX outbox_events_terminal_retention_idx
    ON outbox_events (COALESCE(delivered_at, dead_at), id)
    WHERE status IN ('delivered', 'dead');

CREATE INDEX outbox_events_terminal_secret_retention_idx
    ON outbox_events (COALESCE(delivered_at, dead_at), id)
    WHERE status IN ('delivered', 'dead')
      AND payload ?| ARRAY[
          'confirmation_token',
          'session_recovery_token',
          'unsubscribe_token',
          'claim_token',
          'coupon_code'
      ];
