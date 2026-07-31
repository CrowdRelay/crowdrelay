ALTER TABLE fan_action_tokens
    DROP CONSTRAINT fan_action_tokens_purpose_check;

ALTER TABLE fan_action_tokens
    ADD CONSTRAINT fan_action_tokens_purpose_check
    CHECK (purpose IN ('confirm', 'unsubscribe', 'session'));
