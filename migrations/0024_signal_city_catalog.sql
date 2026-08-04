-- Canonical Virya Signal city catalogue.
-- Idempotent: safe to execute directly now and later as migration 0024.

CREATE TEMP TABLE signal_city_seed (
    ordinal integer PRIMARY KEY,
    slug text NOT NULL UNIQUE,
    name text NOT NULL,
    region text NOT NULL,
    latitude double precision NOT NULL,
    longitude double precision NOT NULL
);

INSERT INTO signal_city_seed (
    ordinal,
    slug,
    name,
    region,
    latitude,
    longitude
)
VALUES
    (1,  'wroclaw',     'Wrocław',     'Dolnośląskie',         51.1079, 17.0385),
    (2,  'poznan',      'Poznań',      'Wielkopolskie',        52.4064, 16.9252),
    (3,  'gdansk',      'Gdańsk',      'Pomorskie',            54.3520, 18.6466),
    (4,  'warszawa',    'Warszawa',    'Mazowieckie',          52.2297, 21.0122),
    (5,  'katowice',    'Katowice',    'Śląskie',              50.2649, 19.0238),
    (6,  'krakow',      'Kraków',      'Małopolskie',          50.0647, 19.9450),
    (7,  'lodz',        'Łódź',        'Łódzkie',              51.7592, 19.4560),
    (8,  'szczecin',    'Szczecin',    'Zachodniopomorskie',  53.4285, 14.5528),
    (9,  'lublin',      'Lublin',      'Lubelskie',            51.2465, 22.5684),
    (10, 'rzeszow',     'Rzeszów',     'Podkarpackie',         50.0413, 21.9990),
    (11, 'bialystok',   'Białystok',   'Podlaskie',            53.1325, 23.1688),
    (12, 'torun',       'Toruń',       'Kujawsko-Pomorskie',  53.0138, 18.5984),
    (13, 'czestochowa', 'Częstochowa', 'Śląskie',              50.8118, 19.1203);

DO $$
DECLARE
    seed record;
    canonical_id uuid;
    duplicate_ids uuid[];
    merged_request_count integer;
    merged_first_requested_at timestamptz;
    merged_last_requested_at timestamptz;
    unexpected_references text;
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

    FOR seed IN
        SELECT *
        FROM signal_city_seed
        ORDER BY ordinal
    LOOP
        canonical_id := NULL;
        duplicate_ids := ARRAY[]::uuid[];

        SELECT city.id
        INTO canonical_id
        FROM cities AS city
        WHERE city.country_code = 'PL'
          AND city.slug = seed.slug
        LIMIT 1;

        IF canonical_id IS NULL THEN
            SELECT city.id
            INTO canonical_id
            FROM cities AS city
            WHERE city.country_code = 'PL'
              AND lower(regexp_replace(btrim(city.name), '\s+', ' ', 'g'))
                  = lower(seed.name)
            ORDER BY
                CASE city.moderation_status
                    WHEN 'approved' THEN 0
                    WHEN 'pending' THEN 1
                    WHEN 'merged' THEN 2
                    ELSE 3
                END,
                CASE
                    WHEN city.latitude IS NOT NULL
                     AND city.longitude IS NOT NULL
                    THEN 0
                    ELSE 1
                END,
                city.id
            LIMIT 1;
        END IF;

        IF canonical_id IS NULL THEN
            INSERT INTO cities (
                slug,
                name,
                country_code,
                region,
                latitude,
                longitude,
                moderation_status,
                request_count
            )
            VALUES (
                seed.slug,
                seed.name,
                'PL',
                seed.region,
                seed.latitude,
                seed.longitude,
                'approved',
                0
            )
            RETURNING id INTO canonical_id;
        END IF;

        SELECT coalesce(array_agg(city.id ORDER BY city.id), ARRAY[]::uuid[])
        INTO duplicate_ids
        FROM cities AS city
        WHERE city.country_code = 'PL'
          AND city.id <> canonical_id
          AND lower(regexp_replace(btrim(city.name), '\s+', ' ', 'g'))
              = lower(seed.name);

        SELECT
            coalesce(sum(city.request_count), 0)::integer,
            min(city.first_requested_at),
            max(city.last_requested_at)
        INTO
            merged_request_count,
            merged_first_requested_at,
            merged_last_requested_at
        FROM cities AS city
        WHERE city.id = canonical_id
           OR city.id = ANY(duplicate_ids);

        IF cardinality(duplicate_ids) > 0 THEN
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
                interest.workspace_id,
                interest.fan_id,
                canonical_id,
                min(interest.created_at)
            FROM fan_city_interests AS interest
            WHERE interest.city_id = ANY(duplicate_ids)
            GROUP BY interest.workspace_id, interest.fan_id
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
            WHERE city_id = ANY(duplicate_ids);

            DELETE FROM cities
            WHERE id = ANY(duplicate_ids);
        END IF;

        UPDATE cities
        SET slug = seed.slug,
            name = seed.name,
            country_code = 'PL',
            region = seed.region,
            latitude = seed.latitude,
            longitude = seed.longitude,
            moderation_status = 'approved',
            request_count = merged_request_count,
            first_requested_at = merged_first_requested_at,
            last_requested_at = merged_last_requested_at
        WHERE id = canonical_id;

        RAISE NOTICE
            'Signal city ready: % (%) id=% merged_duplicates=%',
            seed.name,
            seed.slug,
            canonical_id,
            cardinality(duplicate_ids);
    END LOOP;
END
$$;

DELETE FROM city_aggregates AS aggregate
USING cities AS city
JOIN signal_city_seed AS seed
  ON seed.slug = city.slug
WHERE aggregate.city_id = city.id
  AND city.country_code = 'PL';

INSERT INTO city_aggregates (
    workspace_id,
    city_id,
    confirmed_fan_count,
    updated_at
)
SELECT
    workspace.id,
    city.id,
    count(DISTINCT interest.fan_id)
        FILTER (WHERE fan.status = 'active'),
    now()
FROM workspaces AS workspace
CROSS JOIN signal_city_seed AS seed
JOIN cities AS city
  ON city.country_code = 'PL'
 AND city.slug = seed.slug
LEFT JOIN fan_city_interests AS interest
  ON interest.workspace_id = workspace.id
 AND interest.city_id = city.id
LEFT JOIN fans AS fan
  ON fan.workspace_id = interest.workspace_id
 AND fan.id = interest.fan_id
GROUP BY workspace.id, city.id
ON CONFLICT (workspace_id, city_id)
DO UPDATE SET
    confirmed_fan_count = EXCLUDED.confirmed_fan_count,
    updated_at = EXCLUDED.updated_at;

DO $$
DECLARE
    missing text;
    duplicates text;
BEGIN
    SELECT string_agg(seed.name, ', ' ORDER BY seed.ordinal)
    INTO missing
    FROM signal_city_seed AS seed
    LEFT JOIN cities AS city
      ON city.country_code = 'PL'
     AND city.slug = seed.slug
    WHERE city.id IS NULL;

    IF missing IS NOT NULL THEN
        RAISE EXCEPTION 'Missing Signal cities after seed: %', missing;
    END IF;

    SELECT string_agg(result.name, ', ' ORDER BY result.name)
    INTO duplicates
    FROM (
        SELECT seed.name
        FROM signal_city_seed AS seed
        JOIN cities AS city
          ON city.country_code = 'PL'
         AND lower(regexp_replace(btrim(city.name), '\s+', ' ', 'g'))
             = lower(seed.name)
        GROUP BY seed.name
        HAVING count(*) <> 1
    ) AS result;

    IF duplicates IS NOT NULL THEN
        RAISE EXCEPTION
            'Signal city names are not unique after seed: %',
            duplicates;
    END IF;
END
$$;

DROP TABLE signal_city_seed;
