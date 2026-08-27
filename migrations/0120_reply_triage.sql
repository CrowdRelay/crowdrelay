-- Phase 19: Reply triage — first-party classification of inbound replies.
--
-- n8n posts replies with a disposition it assigned, but does not classify
-- reliably. Some replies are unclassified (`received`), some are
-- misclassified. The worker re-classifies using the domain classifier and
-- records the result here.
--
-- The reply text is stored because the classifier needs it, and because a
-- human reviewing a `NeedsHuman` classification needs to see what the
-- curator actually wrote. Without the text, the human review path is a
-- label without a reason.

CREATE TABLE viryaos_reply_classifications (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- The target the reply was from. The reply itself is recorded as a
    -- disposition update on the target; this table holds the classification
    -- and the text, keyed to the target and the time it was classified.
    target_id uuid NOT NULL,
    target_kind text NOT NULL CHECK (target_kind IN (
        'playlist','radio','press','creator','support_slot','endorsement','media_patronage'
    )),
    -- The free-text body of the reply, as received. Bounded so a misconfigured
    -- adapter cannot flood the table.
    reply_text text NOT NULL CHECK (
        btrim(reply_text) <> '' AND char_length(reply_text) <= 4000
    ),
    -- What n8n assigned, if anything. NULL means no prior classification.
    previous_disposition text CHECK (
        previous_disposition IS NULL
        OR previous_disposition IN ('none', 'received', 'positive', 'declined', 'do_not_contact')
    ),
    -- The classifier's verdict: 'auto' or 'needs_human'.
    classification_result text NOT NULL CHECK (
        classification_result IN ('auto', 'needs_human')
    ),
    -- The disposition the classifier assigned, when result = 'auto'.
    classified_disposition text CHECK (
        classified_disposition IS NULL
        OR classified_disposition IN ('positive', 'declined', 'do_not_contact')
    ),
    -- Why human review is needed, when result = 'needs_human'.
    human_review_reason text CHECK (
        human_review_reason IS NULL
        OR human_review_reason IN (
            'ambiguous_text', 'not_in_supported_language', 'too_short',
            'previous_do_not_contact', 'unmatched_text'
        )
    ),
    -- Confidence in basis points (0-10000).
    confidence_basis_points integer NOT NULL CHECK (
        confidence_basis_points BETWEEN 0 AND 10000
    ),
    -- Which rules matched, for auditability. Empty for needs_human.
    matched_rules jsonb NOT NULL DEFAULT '[]'::jsonb,
    classified_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    -- One classification per target per classified_at, so a re-classification
    -- is a new row rather than an overwrite.
    UNIQUE (workspace_id, target_id, classified_at)
);

CREATE INDEX viryaos_reply_classifications_needs_human_idx
    ON viryaos_reply_classifications (workspace_id, classified_at DESC)
    WHERE classification_result = 'needs_human';

CREATE INDEX viryaos_reply_classifications_target_idx
    ON viryaos_reply_classifications (workspace_id, target_id, classified_at DESC);
