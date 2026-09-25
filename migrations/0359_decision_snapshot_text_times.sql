-- Stored decision snapshots stop showing tuple dates.
--
-- `serde_json::to_value` writes a bare `OffsetDateTime` as
-- `[year, ordinal, hour, minute, second, nanoseconds, offH, offM, offS]` and a
-- `Date` as `[year, ordinal]`. Every `autopilot_decisions` row written before
-- the wire-time formatting landed carries that shape inside `input_snapshot`,
-- `policy_snapshot` and `recommendation` — `content_supply` alone held 738 —
-- and the console's evidence view showed the arrays raw.
--
-- New rows already write RFC 3339 text; this rewrites the stored history so
-- the audit trail reads the same way. The rewrite is a recursive JSONB walk:
-- any 9-element all-integer array inside the timestamp bounds becomes
-- `YYYY-MM-DDTHH:MM:SS[.fraction][Z|±HH:MM]`, any 2-element `[year, ordinal]`
-- becomes `YYYY-MM-DD`. Plain integer arrays that happen to match the bounds
-- are theoretically at risk — accepted, because the bounds (ordinal, hour,
-- offset ranges) are what serde emits and nothing in these columns stores a
-- 9-integer vector.
--
-- The function is created, run, and dropped in one file: a one-shot rewrite
-- helper is not a schema object and must not outlive the migration.

CREATE OR REPLACE FUNCTION _m0359_time_tuples_to_text(value jsonb)
RETURNS jsonb
LANGUAGE plpgsql
IMMUTABLE
AS $rewrite$
DECLARE
    result jsonb;
    element jsonb;
    ints integer[];
    y int; ord int; hh int; mi int; ss int; ns bigint; oh int; om int; os int;
    day_text text;
    frac_text text;
    offset_text text;
BEGIN
    CASE jsonb_typeof(value)
        WHEN 'object' THEN
            SELECT jsonb_object_agg(item.key, _m0359_time_tuples_to_text(item.value))
              INTO result
              FROM jsonb_each(value) AS item;
            RETURN COALESCE(result, '{}'::jsonb);
        WHEN 'array' THEN
            SELECT array_agg((elem.value)::int)
              INTO ints
              FROM jsonb_array_elements_text(value) AS elem
             WHERE (elem.value)::text ~ '^-?[0-9]+$';
            IF array_length(ints, 1) IS DISTINCT FROM jsonb_array_length(value) THEN
                ints := NULL;
            END IF;
            IF ints IS NOT NULL
               AND jsonb_array_length(value) = 9
               AND ints[1] BETWEEN 2000 AND 2100
               AND ints[2] BETWEEN 1 AND 366
               AND ints[3] BETWEEN 0 AND 23
               AND ints[4] BETWEEN 0 AND 59
               AND ints[5] BETWEEN 0 AND 60
               AND ints[6] >= 0
               -- Real timezone offsets only (±14h), and serde signs every
               -- offset component together — a 9-integer vector that fails
               -- either rule is data, not a timestamp.
               AND ints[7] BETWEEN -14 AND 14
               AND ints[8] BETWEEN -59 AND 59
               AND ints[9] BETWEEN -59 AND 59
               AND ((ints[7] >= 0 AND ints[8] >= 0 AND ints[9] >= 0)
                    OR (ints[7] <= 0 AND ints[8] <= 0 AND ints[9] <= 0))
            THEN
                y := ints[1]; ord := ints[2]; hh := ints[3]; mi := ints[4];
                ss := ints[5]; ns := ints[6]; oh := ints[7]; om := ints[8]; os := ints[9];
                day_text := to_char(to_date(y::text || '-' || ord::text, 'YYYY-DDD')
                                    + make_time(hh, mi, ss),
                                    'YYYY-MM-DD"T"HH24:MI:SS');
                -- Postgres keeps microseconds; the stored nanoseconds round to
                -- six digits, which is still valid RFC 3339.
                frac_text := CASE WHEN ns > 0
                                  THEN '.' || lpad(least(ns, 999999999)::text, 9, '0')
                                  ELSE '' END;
                frac_text := rtrim(frac_text, '0');
                IF frac_text = '.' THEN frac_text := ''; END IF;
                IF oh = 0 AND om = 0 AND os = 0 THEN
                    offset_text := 'Z';
                ELSE
                    offset_text := (CASE WHEN oh < 0 OR om < 0 OR os < 0 THEN '-' ELSE '+' END)
                                   || lpad(abs(oh)::text, 2, '0')
                                   || ':' || lpad(abs(om)::text, 2, '0');
                END IF;
                RETURN to_jsonb(day_text || frac_text || offset_text);
            ELSIF ints IS NOT NULL
                  AND jsonb_array_length(value) = 2
                  AND ints[1] BETWEEN 2000 AND 2100
                  AND ints[2] BETWEEN 1 AND 366
            THEN
                RETURN to_jsonb(
                    to_char(to_date(ints[1]::text || '-' || ints[2]::text, 'YYYY-DDD'),
                            'YYYY-MM-DD'));
            ELSE
                SELECT jsonb_agg(_m0359_time_tuples_to_text(elem.value))
                  INTO result
                  FROM jsonb_array_elements(value) AS elem;
                RETURN COALESCE(result, '[]'::jsonb);
            END IF;
        ELSE
            RETURN value;
    END CASE;
END;
$rewrite$;

UPDATE autopilot_decisions
SET input_snapshot = _m0359_time_tuples_to_text(input_snapshot),
    policy_snapshot = _m0359_time_tuples_to_text(policy_snapshot),
    recommendation = _m0359_time_tuples_to_text(recommendation)
WHERE input_snapshot::text ~ '\[\s*(19|20|21)[0-9]{2}\s*,\s*[0-9]{1,3}\s*,'
   OR policy_snapshot::text ~ '\[\s*(19|20|21)[0-9]{2}\s*,\s*[0-9]{1,3}\s*,'
   OR recommendation::text ~ '\[\s*(19|20|21)[0-9]{2}\s*,\s*[0-9]{1,3}\s*,';

-- `operator_actions.details` held the same tuples (`responds_by` went through
-- a bare `json!`), but the table is append-only — a trigger rejects every
-- UPDATE — so its history keeps the shape it was written with. The writer
-- emits RFC 3339 now; only new rows are clean.

DROP FUNCTION _m0359_time_tuples_to_text(jsonb);
