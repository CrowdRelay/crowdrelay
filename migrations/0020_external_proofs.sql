-- Optional external proof layer for CrowdRelay.
--
-- PostgreSQL remains the source of truth. Proof creation and chain anchoring are
-- asynchronous and never participate in ticketing, consent, draw selection or
-- gate-redemption availability. Only SHA-256 commitments are sent to a chain.

INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
SELECT workspace.id, defaults.key, defaults.enabled, 'proof layer default'
FROM workspaces AS workspace
CROSS JOIN (VALUES
    ('draw_proofs_enabled', true),
    ('blockchain_anchoring_enabled', false)
) AS defaults(key, enabled)
ON CONFLICT (workspace_id, key) DO NOTHING;

CREATE TABLE external_proof_batches (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    proof_kind text NOT NULL CHECK (proof_kind IN ('audit_ledger', 'draw_receipt')),
    schema_version integer NOT NULL DEFAULT 1 CHECK (schema_version > 0),
    hash_algorithm text NOT NULL DEFAULT 'sha256' CHECK (hash_algorithm = 'sha256'),
    tree_algorithm text NOT NULL DEFAULT 'binary-duplicate-last-v1'
        CHECK (tree_algorithm IN ('binary-duplicate-last-v1', 'single-leaf-v1')),
    root_sha256 bytea NOT NULL CHECK (octet_length(root_sha256) = 32),
    leaf_count integer NOT NULL CHECK (leaf_count BETWEEN 1 AND 100000),
    status text NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'processing', 'confirmed', 'failed', 'dead')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts integer NOT NULL DEFAULT 12 CHECK (max_attempts BETWEEN 1 AND 100),
    available_at timestamptz NOT NULL DEFAULT now(),
    locked_at timestamptz,
    lock_owner text CHECK (lock_owner IS NULL OR (btrim(lock_owner) <> '' AND char_length(lock_owner) <= 128)),
    lease_expires_at timestamptz,
    chain_namespace text CHECK (chain_namespace IS NULL OR chain_namespace ~ '^[a-z0-9][a-z0-9_.-]{1,31}$'),
    chain_id bigint CHECK (chain_id IS NULL OR chain_id > 0),
    contract_address text CHECK (contract_address IS NULL OR contract_address ~ '^0x[0-9a-fA-F]{40}$'),
    transaction_hash text CHECK (transaction_hash IS NULL OR transaction_hash ~ '^0x[0-9a-fA-F]{64}$'),
    block_number bigint CHECK (block_number IS NULL OR block_number >= 0),
    transaction_batch_index integer CHECK (transaction_batch_index IS NULL OR transaction_batch_index >= 0),
    block_hash text CHECK (block_hash IS NULL OR block_hash ~ '^0x[0-9a-fA-F]{64}$'),
    last_error_kind text CHECK (last_error_kind IS NULL OR (btrim(last_error_kind) <> '' AND char_length(last_error_kind) <= 96)),
    request_id text CHECK (request_id IS NULL OR char_length(request_id) <= 128),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    confirmed_at timestamptz,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, proof_kind, root_sha256),
    CHECK (attempts <= max_attempts),
    CHECK (
        (status = 'processing' AND locked_at IS NOT NULL AND lock_owner IS NOT NULL AND lease_expires_at > locked_at)
        OR (status <> 'processing' AND locked_at IS NULL AND lock_owner IS NULL AND lease_expires_at IS NULL)
    ),
    CHECK (
        status = 'confirmed'
        OR (
            chain_namespace IS NULL
            AND chain_id IS NULL
            AND contract_address IS NULL
            AND transaction_hash IS NULL
            AND block_number IS NULL
            AND transaction_batch_index IS NULL
            AND block_hash IS NULL
        )
    ),
    CHECK (
        (status = 'confirmed'
         AND chain_namespace IS NOT NULL
         AND chain_id IS NOT NULL
         AND contract_address IS NOT NULL
         AND transaction_hash IS NOT NULL
         AND block_number IS NOT NULL
         AND transaction_batch_index IS NOT NULL
         AND block_hash IS NOT NULL
         AND confirmed_at IS NOT NULL)
        OR (status <> 'confirmed' AND confirmed_at IS NULL)
    )
);

CREATE TRIGGER external_proof_batches_set_updated_at
BEFORE UPDATE ON external_proof_batches
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE UNIQUE INDEX external_proof_batches_chain_tx_idx
    ON external_proof_batches (chain_namespace, chain_id, transaction_hash, transaction_batch_index)
    WHERE transaction_hash IS NOT NULL;

CREATE INDEX external_proof_batches_claim_idx
    ON external_proof_batches (workspace_id, available_at, created_at, id)
    WHERE status IN ('queued', 'failed');

CREATE INDEX external_proof_batches_lease_recovery_idx
    ON external_proof_batches (workspace_id, lease_expires_at, id)
    WHERE status = 'processing';

CREATE INDEX external_proof_batches_workspace_recent_idx
    ON external_proof_batches (workspace_id, status, created_at DESC, id DESC);

CREATE INDEX external_proof_batches_confirmed_idx
    ON external_proof_batches (workspace_id, confirmed_at DESC, id DESC)
    WHERE status = 'confirmed';

CREATE TABLE external_proof_items (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    batch_id uuid NOT NULL,
    sequence integer NOT NULL CHECK (sequence >= 0),
    source_kind text NOT NULL CHECK (source_kind IN ('audit_event', 'operator_action', 'reward_draw_run')),
    source_id uuid NOT NULL,
    leaf_sha256 bytea NOT NULL CHECK (octet_length(leaf_sha256) = 32),
    occurred_at timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, batch_id, sequence),
    UNIQUE (workspace_id, source_kind, source_id),
    CONSTRAINT external_proof_items_batch_fk
        FOREIGN KEY (workspace_id, batch_id)
        REFERENCES external_proof_batches (workspace_id, id)
        ON DELETE CASCADE
);


CREATE TRIGGER external_proof_items_append_only
BEFORE UPDATE OR DELETE ON external_proof_items
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_reject_append_only_mutation();

CREATE TABLE reward_draw_proofs (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    run_id uuid NOT NULL,
    draw_id uuid NOT NULL,
    anchor_batch_id uuid NOT NULL,
    receipt_sha256 bytea NOT NULL CHECK (octet_length(receipt_sha256) = 32),
    candidate_snapshot_sha256 bytea NOT NULL CHECK (octet_length(candidate_snapshot_sha256) = 32),
    winner_snapshot_sha256 bytea NOT NULL CHECK (octet_length(winner_snapshot_sha256) = 32),
    eligible_count integer NOT NULL CHECK (eligible_count >= 0),
    selected_winners integer NOT NULL CHECK (selected_winners >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, run_id),
    UNIQUE (workspace_id, anchor_batch_id),
    CONSTRAINT reward_draw_proofs_run_fk
        FOREIGN KEY (workspace_id, draw_id, run_id)
        REFERENCES reward_draw_runs (workspace_id, draw_id, id)
        ON DELETE CASCADE,
    CONSTRAINT reward_draw_proofs_anchor_fk
        FOREIGN KEY (workspace_id, anchor_batch_id)
        REFERENCES external_proof_batches (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE INDEX reward_draw_proofs_draw_idx
    ON reward_draw_proofs (workspace_id, draw_id, created_at DESC, run_id DESC);

CREATE TRIGGER reward_draw_proofs_append_only
BEFORE UPDATE OR DELETE ON reward_draw_proofs
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_reject_append_only_mutation();
