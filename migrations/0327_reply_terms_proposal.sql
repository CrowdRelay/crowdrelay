-- A promoter's, venue's, or festival's written reply may carry the one
-- number a negotiation turns on. ('agent'/'label' ride along because
-- viryaos_outreach_targets already admits them and the outreach ingress
-- writes this same column — the old CHECK would have rejected the row.) Booking-channel replies join the triage queue so
-- the deterministic reader can propose the terms it found — a proposal a
-- human confirms through the terms route, never a write the machine made.
-- `negotiation_reply` is the honest needs-human reason: a reply whose
-- disposition the operator already filed still deserves eyes for the
-- number inside it.

ALTER TABLE viryaos_reply_classifications
    DROP CONSTRAINT IF EXISTS viryaos_reply_classifications_target_kind_check;
ALTER TABLE viryaos_reply_classifications
    ADD CONSTRAINT viryaos_reply_classifications_target_kind_check CHECK (target_kind IN (
        'playlist','radio','press','creator','support_slot','endorsement','media_patronage',
        'promoter','venue','festival','agent','label'
    ));

ALTER TABLE viryaos_reply_classifications
    DROP CONSTRAINT IF EXISTS viryaos_reply_classifications_human_review_reason_check;
ALTER TABLE viryaos_reply_classifications
    ADD CONSTRAINT viryaos_reply_classifications_human_review_reason_check CHECK (
        human_review_reason IS NULL
        OR human_review_reason IN (
            'ambiguous_text', 'not_in_supported_language', 'too_short',
            'previous_do_not_contact', 'unmatched_text', 'negotiation_reply'
        )
    );

-- What the reader proposed: the fee and currency it read, and the live
-- team opportunity the reply's target matches (by the same
-- contact-email/organization join the market floor already trusts). Fee
-- and currency arrive together or not at all; the opportunity link is
-- independently nullable because a number with no matching open
-- negotiation is still worth showing.
ALTER TABLE viryaos_reply_classifications
    ADD COLUMN proposed_fee_minor bigint,
    ADD COLUMN proposed_currency text,
    ADD COLUMN proposed_opportunity_id uuid;

ALTER TABLE viryaos_reply_classifications
    ADD CONSTRAINT viryaos_reply_classifications_proposed_check CHECK (
        (proposed_fee_minor IS NULL) = (proposed_currency IS NULL)
        AND (proposed_fee_minor IS NULL OR proposed_fee_minor > 0)
        AND (proposed_currency IS NULL OR proposed_currency ~ '^[A-Z]{3}$')
    );
