-- Reconcile canonical Polish city records.
--
-- Idempotent migration corresponding to the production repair run from
-- 2026-08-04: merge duplicate Wrocław rows and replace the placeholder
-- Kaliszkowice Kaliskie with the first large missing Polish city.
-- Known city foreign keys are rewired atomically; an unknown new foreign key
-- aborts the migration instead of risking partial data loss.

SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '90s';

LOCK TABLE cities IN SHARE ROW EXCLUSIVE MODE;
LOCK TABLE events IN SHARE ROW EXCLUSIVE MODE;
LOCK TABLE fan_city_interests IN SHARE ROW EXCLUSIVE MODE;
LOCK TABLE city_aggregates IN SHARE ROW EXCLUSIVE MODE;
LOCK TABLE fan_location_preferences IN SHARE ROW EXCLUSIVE MODE;

DO $$
DECLARE
    canonical_id uuid;
    duplicate_ids uuid[];
    candidate_count integer;
    unexpected_references text;

    placeholder_id uuid;
    placeholder_count integer;
    replacement record;
    live_reference_count bigint;
BEGIN
    SELECT string_agg(
        format('%s.%s', ns.nspname, cls.relname),
        ', ' ORDER BY ns.nspname, cls.relname
    )
    INTO unexpected_references
    FROM pg_constraint con
    JOIN pg_class cls ON cls.oid = con.conrelid
    JOIN pg_namespace ns ON ns.oid = cls.relnamespace
    WHERE con.contype = 'f'
      AND con.confrelid = 'cities'::regclass
      AND con.conrelid NOT IN (
          'events'::regclass,
          'fan_city_interests'::regclass,
          'city_aggregates'::regclass,
          'fan_location_preferences'::regclass
      );

    IF unexpected_references IS NOT NULL THEN
        RAISE EXCEPTION
            'Unexpected foreign keys reference cities: %',
            unexpected_references;
    END IF;

    ---------------------------------------------------------------------------
    -- 1. Merge duplicate Wrocław rows into the approved canonical row.
    ---------------------------------------------------------------------------
    SELECT count(*)
    INTO candidate_count
    FROM cities
    WHERE country_code = 'PL'
      AND lower(regexp_replace(btrim(name), '\s+', ' ', 'g')) = 'wrocław';

    IF candidate_count = 0 THEN
        RAISE EXCEPTION 'No Wrocław city row found';
    END IF;

    SELECT id
    INTO canonical_id
    FROM cities
    WHERE country_code = 'PL'
      AND lower(regexp_replace(btrim(name), '\s+', ' ', 'g')) = 'wrocław'
    ORDER BY
        CASE moderation_status
            WHEN 'approved' THEN 0
            WHEN 'pending' THEN 1
            WHEN 'merged' THEN 2
            ELSE 3
        END,
        CASE WHEN slug = 'wroclaw' THEN 0 ELSE 1 END,
        CASE WHEN latitude IS NOT NULL AND longitude IS NOT NULL THEN 0 ELSE 1 END,
        id
    LIMIT 1;

    SELECT coalesce(array_agg(id ORDER BY id), ARRAY[]::uuid[])
    INTO duplicate_ids
    FROM cities
    WHERE country_code = 'PL'
      AND lower(regexp_replace(btrim(name), '\s+', ' ', 'g')) = 'wrocław'
      AND id <> canonical_id;

    IF cardinality(duplicate_ids) > 0 THEN
        UPDATE cities AS canonical
        SET name = 'Wrocław',
            moderation_status = 'approved',
            region = coalesce(
                canonical.region,
                (
                    SELECT region
                    FROM cities
                    WHERE id = ANY(duplicate_ids)
                      AND region IS NOT NULL
                    ORDER BY id
                    LIMIT 1
                )
            ),
            latitude = coalesce(
                canonical.latitude,
                (
                    SELECT latitude
                    FROM cities
                    WHERE id = ANY(duplicate_ids)
                      AND latitude IS NOT NULL
                    ORDER BY id
                    LIMIT 1
                )
            ),
            longitude = coalesce(
                canonical.longitude,
                (
                    SELECT longitude
                    FROM cities
                    WHERE id = ANY(duplicate_ids)
                      AND longitude IS NOT NULL
                    ORDER BY id
                    LIMIT 1
                )
            ),
            request_count = (
                SELECT coalesce(sum(request_count), 0)
                FROM cities
                WHERE id = canonical_id OR id = ANY(duplicate_ids)
            ),
            first_requested_at = (
                SELECT min(first_requested_at)
                FROM cities
                WHERE id = canonical_id OR id = ANY(duplicate_ids)
            ),
            last_requested_at = (
                SELECT max(last_requested_at)
                FROM cities
                WHERE id = canonical_id OR id = ANY(duplicate_ids)
            )
        WHERE canonical.id = canonical_id;

        UPDATE events
        SET city_id = canonical_id
        WHERE city_id = ANY(duplicate_ids);

        INSERT INTO fan_city_interests (
            workspace_id,
            fan_id,
            city_id,
            created_at
        )
        SELECT
            workspace_id,
            fan_id,
            canonical_id,
            min(created_at)
        FROM fan_city_interests
        WHERE city_id = ANY(duplicate_ids)
        GROUP BY workspace_id, fan_id
        ON CONFLICT (workspace_id, fan_id, city_id)
        DO UPDATE SET
            created_at = least(
                fan_city_interests.created_at,
                EXCLUDED.created_at
            );

        DELETE FROM fan_city_interests
        WHERE city_id = ANY(duplicate_ids);

        UPDATE fan_location_preferences
        SET city_id = canonical_id
        WHERE city_id = ANY(duplicate_ids);

        DELETE FROM city_aggregates
        WHERE city_id = canonical_id OR city_id = ANY(duplicate_ids);

        INSERT INTO city_aggregates (
            workspace_id,
            city_id,
            confirmed_fan_count,
            updated_at
        )
        SELECT
            workspace.id,
            canonical_id,
            count(DISTINCT interest.fan_id)
                FILTER (WHERE fan.status = 'active'),
            now()
        FROM workspaces AS workspace
        LEFT JOIN fan_city_interests AS interest
            ON interest.workspace_id = workspace.id
           AND interest.city_id = canonical_id
        LEFT JOIN fans AS fan
            ON fan.workspace_id = interest.workspace_id
           AND fan.id = interest.fan_id
        GROUP BY workspace.id
        ON CONFLICT (workspace_id, city_id)
        DO UPDATE SET
            confirmed_fan_count = EXCLUDED.confirmed_fan_count,
            updated_at = EXCLUDED.updated_at;

        DELETE FROM cities
        WHERE id = ANY(duplicate_ids);

        RAISE NOTICE
            'Merged % duplicate Wrocław row(s) into %',
            cardinality(duplicate_ids),
            canonical_id;
    ELSE
        RAISE NOTICE 'Wrocław is already unique: %', canonical_id;
    END IF;

    ---------------------------------------------------------------------------
    -- 2. Replace Kaliszkowice Kaliskie with the largest preferred PL city
    --    that is not already present. The city UUID stays stable.
    ---------------------------------------------------------------------------
    SELECT count(*)
    INTO placeholder_count
    FROM cities
    WHERE country_code = 'PL'
      AND lower(regexp_replace(btrim(name), '\s+', ' ', 'g'))
          = 'kaliszkowice kaliskie';

    IF placeholder_count > 1 THEN
        RAISE EXCEPTION
            'Expected at most one Kaliszkowice Kaliskie row, found %',
            placeholder_count;
    ELSIF placeholder_count = 1 THEN
        SELECT id
        INTO placeholder_id
        FROM cities
        WHERE country_code = 'PL'
          AND lower(regexp_replace(btrim(name), '\s+', ' ', 'g'))
              = 'kaliszkowice kaliskie';

        SELECT candidate.*
        INTO replacement
        FROM (
            VALUES
                (1,  'warszawa',     'Warszawa',     'mazowieckie',          52.2297::double precision, 21.0122::double precision),
                (2,  'krakow',       'Kraków',       'małopolskie',          50.0647::double precision, 19.9450::double precision),
                (3,  'lodz',         'Łódź',         'łódzkie',              51.7592::double precision, 19.4560::double precision),
                (4,  'poznan',       'Poznań',       'wielkopolskie',        52.4064::double precision, 16.9252::double precision),
                (5,  'gdansk',       'Gdańsk',       'pomorskie',            54.3520::double precision, 18.6466::double precision),
                (6,  'szczecin',     'Szczecin',     'zachodniopomorskie',   53.4285::double precision, 14.5528::double precision),
                (7,  'lublin',       'Lublin',       'lubelskie',            51.2465::double precision, 22.5684::double precision),
                (8,  'bydgoszcz',    'Bydgoszcz',    'kujawsko-pomorskie',   53.1235::double precision, 18.0084::double precision),
                (9,  'bialystok',    'Białystok',    'podlaskie',            53.1325::double precision, 23.1688::double precision),
                (10, 'katowice',     'Katowice',     'śląskie',              50.2649::double precision, 19.0238::double precision),
                (11, 'gdynia',       'Gdynia',       'pomorskie',            54.5189::double precision, 18.5305::double precision),
                (12, 'radom',        'Radom',        'mazowieckie',          51.4027::double precision, 21.1471::double precision),
                (13, 'rzeszow',      'Rzeszów',      'podkarpackie',         50.0413::double precision, 21.9990::double precision),
                (14, 'torun',        'Toruń',        'kujawsko-pomorskie',   53.0138::double precision, 18.5984::double precision),
                (15, 'kielce',       'Kielce',       'świętokrzyskie',       50.8661::double precision, 20.6286::double precision),
                (16, 'olsztyn',      'Olsztyn',      'warmińsko-mazurskie',  53.7784::double precision, 20.4801::double precision),
                (17, 'opole',        'Opole',        'opolskie',             50.6751::double precision, 17.9213::double precision),
                (18, 'zielona-gora', 'Zielona Góra', 'lubuskie',             51.9356::double precision, 15.5062::double precision)
        ) AS candidate(
            priority,
            slug,
            name,
            region,
            latitude,
            longitude
        )
        WHERE NOT EXISTS (
            SELECT 1
            FROM cities existing
            WHERE existing.country_code = 'PL'
              AND (
                  existing.slug = candidate.slug
                  OR lower(regexp_replace(btrim(existing.name), '\s+', ' ', 'g'))
                     = lower(candidate.name)
              )
        )
        ORDER BY candidate.priority
        LIMIT 1;

        IF replacement.slug IS NULL THEN
            RAISE EXCEPTION
                'Every preferred large Polish city is already present';
        END IF;

        SELECT
            (SELECT count(*) FROM events WHERE city_id = placeholder_id)
            + (SELECT count(*) FROM fan_city_interests WHERE city_id = placeholder_id)
            + (SELECT count(*) FROM fan_location_preferences WHERE city_id = placeholder_id)
        INTO live_reference_count;

        UPDATE cities
        SET slug = replacement.slug,
            name = replacement.name,
            region = replacement.region,
            latitude = replacement.latitude,
            longitude = replacement.longitude,
            moderation_status = 'approved',
            request_count = 0,
            first_requested_at = NULL,
            last_requested_at = NULL
        WHERE id = placeholder_id;

        RAISE NOTICE
            'Replaced Kaliszkowice Kaliskie with % (%); stable city id %, live references retained: %',
            replacement.name,
            replacement.slug,
            placeholder_id,
            live_reference_count;
    ELSE
        RAISE NOTICE 'Kaliszkowice Kaliskie row is absent; nothing to replace';
    END IF;
END
$$;


SELECT
    id,
    slug,
    name,
    country_code,
    region,
    moderation_status,
    request_count,
    latitude,
    longitude
FROM cities
WHERE country_code = 'PL'
  AND (
      lower(regexp_replace(btrim(name), '\s+', ' ', 'g')) = 'wrocław'
      OR slug IN (
          'warszawa', 'krakow', 'lodz', 'poznan', 'gdansk', 'szczecin',
          'lublin', 'bydgoszcz', 'bialystok', 'katowice', 'gdynia',
          'radom', 'rzeszow', 'torun', 'kielce', 'olsztyn', 'opole',
          'zielona-gora'
      )
  )
ORDER BY name, id;
