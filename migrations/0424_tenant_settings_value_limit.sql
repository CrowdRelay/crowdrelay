-- tenant_settings.value was capped at 512 characters, but the settings API
-- accepts `join_ask_variants` up to 4096 (a JSON list of several 500-character
-- variants does not fit in 512). A list between 513 and 4096 characters passed
-- API validation and then failed at the database with
-- `tenant_settings_value_check`. The database limit now matches the largest
-- value the API accepts; every other key keeps the API's own 512 bound.
ALTER TABLE tenant_settings
    DROP CONSTRAINT IF EXISTS tenant_settings_value_check;

ALTER TABLE tenant_settings
    ADD CONSTRAINT tenant_settings_value_check
    CHECK (char_length(value) <= 4096);
