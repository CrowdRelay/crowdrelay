-- Sprint 3.5b.16 — the brain tunes worker call params from call telemetry.
--
-- `agent_service_llm_calls` is one row per LLM call the agent service makes:
-- what was asked, who answered, how it ended. The daily aggregates in
-- `agent_service_usage` can say what was spent but not why calls fail — a
-- truncation signature, a parse failure rate, a dead provider all need the
-- per-call tail. Owned by the agent service like the other agent_service_*
-- tables; created here (as `agent_outcomes` was in 0125) so the Rust brain
-- treats it as a first-party relation.
--
-- `agent_service_llm_tuning` is the tuning decision the brain writes and the
-- agent runner resolves before each call. One row per workspace: the tuned
-- temperature (NULL = default), a max_tokens scale (NULL = default), and a
-- paid breaker deadline — while it is in the future the runner skips paid
-- models entirely. The brain writes it from `crowdrelay-brain::tune_llm`
-- each autopilot cycle; the runner reads it per task and never trusts it
-- past its own bounds (temp floor/ceiling, scale ceiling, breaker expiry).

CREATE TABLE agent_service_llm_calls (
    id             BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id   UUID NOT NULL,
    task_id        UUID,
    template_id    TEXT NOT NULL,
    provider       TEXT NOT NULL,
    model_id       TEXT NOT NULL,
    -- 'generator' today; verifier calls get their own rows later so a
    -- verifier outage can never masquerade as generator instability.
    call_role      TEXT NOT NULL DEFAULT 'generator',
    paid           BOOLEAN NOT NULL DEFAULT FALSE,
    ok             BOOLEAN NOT NULL,
    -- NULL for free-form tasks; for structured-outcome tasks, whether the
    -- response parsed into the declared schema. Unclassified-heavy output is
    -- the signature the temperature rule reads.
    classified     BOOLEAN,
    error_kind     TEXT,
    -- 'length'/'max_tokens' when the provider cut the response off — the
    -- signature the max_tokens scale rule reads.
    finish_reason  TEXT,
    latency_ms     INTEGER,
    temperature    DOUBLE PRECISION,
    max_tokens     INTEGER,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX agent_service_llm_calls_tail_idx
    ON agent_service_llm_calls (workspace_id, created_at DESC);

CREATE TABLE agent_service_llm_tuning (
    workspace_id        UUID PRIMARY KEY,
    temperature         DOUBLE PRECISION,
    max_tokens_scale    DOUBLE PRECISION,
    paid_breaker_until  TIMESTAMPTZ,
    reason              TEXT NOT NULL DEFAULT '',
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
