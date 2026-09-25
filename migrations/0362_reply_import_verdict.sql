-- Imported outreach answers carry the band's own sheet verdict in
-- `outreach_interactions.metadata` (`master:`/`promo:` source keys) but
-- never carried reply text — the reply-classifier queue, which exists to
-- classify text, cannot route them. The deterministic verdict map decides
-- the terminal rows; everything else needs a human's eyes, and
-- `imported_verdict` is the honest reason: no classifier ran, the sheet's
-- own answer is what arrived.

ALTER TABLE reply_classifications
    DROP CONSTRAINT IF EXISTS reply_classifications_human_review_reason_check;
ALTER TABLE reply_classifications
    ADD CONSTRAINT reply_classifications_human_review_reason_check CHECK (
        human_review_reason IS NULL
        OR human_review_reason IN (
            'ambiguous_text', 'not_in_supported_language', 'too_short',
            'previous_do_not_contact', 'unmatched_text', 'negotiation_reply',
            'imported_verdict'
        )
    );
