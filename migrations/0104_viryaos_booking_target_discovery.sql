-- Venue and promoter discovery — the booking pipeline's supply.
--
-- The negotiation machinery is complete and starves: booking targets have
-- been operator-upsert-only since 0033, so zero venues was a stable state.
-- This gives the booking pipeline what Phase 9 gave the pitcher: candidates
-- arrive from an adapter sweep or an operator import, are screened ON WRITE
-- against a closed refusal set (permanent ones: inferred route, missing
-- evidence, pay-to-play), and only a confirmed email route becomes a real
-- bookable target — city-resolved, because targets are city-scoped by their
-- own UNIQUE constraint.
--
-- Refused rows are kept on purpose: a refusal is durable knowledge, and the
-- same pay-to-play festival must not be rediscovered every sweep.

CREATE TABLE viryaos_booking_candidates (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target_kind text NOT NULL CHECK (target_kind IN ('venue', 'promoter', 'festival')),
    display_name text NOT NULL CHECK (
        btrim(display_name) <> '' AND char_length(display_name) <= 200
    ),
    city_slug text CHECK (
        city_slug IS NULL OR (btrim(city_slug) <> '' AND char_length(city_slug) <= 80)
    ),
    route_kind text NOT NULL CHECK (route_kind IN ('email', 'submission_form', 'handle')),
    -- Dedupe is contact identity: the same inbox found through two sources is
    -- one prospect, not two.
    route_value text NOT NULL CHECK (
        btrim(route_value) <> '' AND char_length(route_value) <= 2048
    ),
    source text NOT NULL CHECK (btrim(source) <> '' AND char_length(source) <= 64),
    source_reference text NOT NULL CHECK (char_length(source_reference) <= 2048),
    evidence text CHECK (evidence IS NULL OR char_length(evidence) <= 4000),
    fit_basis_points integer NOT NULL CHECK (fit_basis_points BETWEEN 0 AND 10000),
    capacity integer CHECK (capacity IS NULL OR capacity > 0),
    status text NOT NULL DEFAULT 'admitted' CHECK (status IN ('admitted', 'refused', 'promoted')),
    refusal_reason text CHECK (refusal_reason IS NULL OR refusal_reason IN (
        'route_inferred', 'evidence_missing', 'paid_to_apply', 'poor_fit'
    )),
    promoted_at timestamptz,
    booking_target_id uuid,
    created_at timestamptz NOT NULL DEFAULT now(),

    -- A stored refusal carries its reason; anything else does not. Written as
    -- IS NOT DISTINCT FROM because a plain `=` passes when both sides are NULL
    -- and would let a reasonless refusal through.
    CHECK (
        (status = 'refused') IS NOT DISTINCT FROM (refusal_reason IS NOT NULL)
    ),
    CHECK (
        (status = 'promoted') IS NOT DISTINCT FROM (booking_target_id IS NOT NULL)
    )
);

CREATE INDEX viryaos_booking_candidates_status_idx
    ON viryaos_booking_candidates (workspace_id, status, created_at DESC);

-- Contact-identity dedupe. An expression needs a unique index rather than a
-- table constraint; NULLS NOT DISTINCT does not apply to expressions, and it
-- is not needed here because route_value is NOT NULL.
CREATE UNIQUE INDEX viryaos_booking_candidates_route_identity_uq
    ON viryaos_booking_candidates (workspace_id, route_kind, lower(btrim(route_value)));
