-- Cross-act catalogue rotation (5.15, §4h-7.4): a fourth amplification
-- purpose. A fan of act A has never heard act B's back catalogue, and the
-- label holds both rights. The consent is its own grant — own monthly cap,
-- own cooldown — because "act A lets the label mail its fans act B's back
-- catalogue" is a different ask than a release feature or a crossbill.
--
-- Widening, not narrowing: the constraint gains a value, so existing rows
-- validate under the new CHECK without a rewrite.
ALTER TABLE amplification_consents
    DROP CONSTRAINT amplification_consents_purpose_check;
ALTER TABLE amplification_consents
    ADD CONSTRAINT amplification_consents_purpose_check
        CHECK (purpose IN (
            'cross_promote', 'release_feature', 'event_crossbill',
            'catalogue_rotation'
        ));
