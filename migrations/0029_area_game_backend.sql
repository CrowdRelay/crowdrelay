-- CrowdRelay is the canonical source of truth for VIRYA AREA drops and claims.
-- Additive migration: it does not alter mail, webhook, ticketing, or reward tables.

CREATE TABLE IF NOT EXISTS area_players (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    id uuid NOT NULL DEFAULT gen_random_uuid(),
    normalized_email text NOT NULL,
    fan_id uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, normalized_email),
    UNIQUE (workspace_id, fan_id),
    CONSTRAINT area_players_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans(workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (normalized_email = lower(btrim(normalized_email))),
    CHECK (length(normalized_email) BETWEEN 3 AND 320)
);

CREATE TABLE IF NOT EXISTS area_drops (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    id text NOT NULL,
    number text NOT NULL,
    city_id uuid NOT NULL REFERENCES cities(id) ON DELETE RESTRICT,
    city text NOT NULL,
    region text NOT NULL,
    signal_city_slug text NOT NULL,
    map_x smallint NOT NULL,
    map_y smallint NOT NULL,
    approximate_lat double precision NOT NULL,
    approximate_lng double precision NOT NULL,
    exact_lat double precision,
    exact_lng double precision,
    radius_meters integer NOT NULL,
    max_claims integer NOT NULL,
    starts_at timestamptz NOT NULL,
    ends_at timestamptz NOT NULL,
    clue_en text NOT NULL,
    clue_pl text NOT NULL,
    collectible_line text NOT NULL,
    collectible_track text NOT NULL,
    collectible_edition text NOT NULL,
    collectible_riddle text NOT NULL DEFAULT 'Yanus',
    active boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, number),
    UNIQUE (workspace_id, city_id),
    CHECK (id ~ '^[a-z]{3}-[0-9]{3}$'),
    CHECK (number ~ '^[0-9]{3}$'),
    CHECK (btrim(city) <> ''),
    CHECK (btrim(region) <> ''),
    CHECK (signal_city_slug ~ '^[a-z0-9][a-z0-9-]{0,99}$'),
    CHECK (map_x BETWEEN 0 AND 100),
    CHECK (map_y BETWEEN 0 AND 100),
    CHECK (approximate_lat BETWEEN -90 AND 90),
    CHECK (approximate_lng BETWEEN -180 AND 180),
    CHECK (exact_lat IS NULL OR exact_lat BETWEEN -90 AND 90),
    CHECK (exact_lng IS NULL OR exact_lng BETWEEN -180 AND 180),
    CHECK (NOT active OR (exact_lat IS NOT NULL AND exact_lng IS NOT NULL)),
    CHECK (radius_meters BETWEEN 25 AND 500),
    CHECK (max_claims BETWEEN 1 AND 500),
    CHECK (ends_at > starts_at)
);

CREATE TABLE IF NOT EXISTS area_challenges (
    workspace_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT gen_random_uuid(),
    player_id uuid NOT NULL,
    drop_id text NOT NULL,
    token_hash bytea NOT NULL,
    issued_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    consumed_at timestamptz,
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, token_hash),
    FOREIGN KEY (workspace_id, player_id)
        REFERENCES area_players(workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, drop_id)
        REFERENCES area_drops(workspace_id, id) ON DELETE CASCADE,
    CHECK (expires_at > issued_at),
    CHECK (consumed_at IS NULL OR consumed_at >= issued_at)
);

CREATE TABLE IF NOT EXISTS area_claims (
    workspace_id uuid NOT NULL,
    player_id uuid NOT NULL,
    drop_id text NOT NULL,
    claimed_at timestamptz NOT NULL DEFAULT now(),
    distance_meters integer NOT NULL,
    edition_number integer NOT NULL,
    claim_source text NOT NULL DEFAULT 'gps',
    PRIMARY KEY (workspace_id, player_id, drop_id),
    UNIQUE (workspace_id, drop_id, edition_number),
    FOREIGN KEY (workspace_id, player_id)
        REFERENCES area_players(workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, drop_id)
        REFERENCES area_drops(workspace_id, id) ON DELETE RESTRICT,
    CHECK (distance_meters >= 0),
    CHECK (edition_number > 0),
    CHECK (claim_source IN ('gps', 'legacy_import'))
);

CREATE INDEX IF NOT EXISTS area_challenges_active_idx
    ON area_challenges (workspace_id, player_id, issued_at DESC)
    WHERE consumed_at IS NULL;
CREATE INDEX IF NOT EXISTS area_challenges_expiry_idx
    ON area_challenges (expires_at);
CREATE INDEX IF NOT EXISTS area_claims_drop_idx
    ON area_claims (workspace_id, drop_id, claimed_at);
CREATE INDEX IF NOT EXISTS area_claims_player_idx
    ON area_claims (workspace_id, player_id, claimed_at DESC);

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM workspaces WHERE slug = 'virya') THEN
        RAISE EXCEPTION 'VIRYA AREA migration requires workspace slug virya';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM (VALUES
            ('wroclaw'), ('poznan'), ('gdansk'), ('warszawa'),
            ('katowice'), ('krakow'), ('lodz'), ('szczecin'),
            ('lublin'), ('rzeszow'), ('bialystok'), ('torun')
        ) AS required(slug)
        LEFT JOIN cities AS city
          ON city.country_code = 'PL'
         AND city.slug = required.slug
        WHERE city.id IS NULL
    ) THEN
        RAISE EXCEPTION 'VIRYA AREA migration requires canonical Signal cities from migration 0024';
    END IF;
END
$$;

WITH seed (
    id, number, city, region, signal_city_slug, map_x, map_y,
    approximate_lat, approximate_lng,
    clue_en, clue_pl, collectible_line, collectible_track, collectible_edition
) AS (
    VALUES
    ('wro-001','001','Wrocław','Dolny Śląsk','wroclaw',34,70,51.1,17.0,
     'A signal is forming somewhere between concrete, water and noise.',
     'Sygnał zbiera się gdzieś pomiędzy betonem, wodą i hałasem.',
     'Damnation through automation.','Technophobia','Genesis'),
    ('poz-002','002','Poznań','Wielkopolska','poznan',29,45,52.4,16.9,
     'Follow the gold signal. Leave the obvious route behind.',
     'Idź za złotym sygnałem. Zostaw oczywistą trasę za sobą.',
     'Take them out, embrace your scars.','Unmasked','Signal'),
    ('gdn-003','003','Gdańsk','Pomorze','gdansk',49,17,54.4,18.6,
     'Look for the echo where steel meets salt.',
     'Szukaj echa tam, gdzie stal spotyka sól.',
     'My time has not yet come.','The Calling','Signal'),
    ('waw-004','004','Warszawa','Mazowsze','warszawa',68,48,52.2,21.0,
     'The loudest city hides its quietest transmission.',
     'Najgłośniejsze miasto ukrywa najcichszą transmisję.',
     'Rise unbound.','Rise','Genesis'),
    ('ktw-005','005','Katowice','Śląsk','katowice',53,79,50.3,19.0,
     'An industrial pulse is waiting below the surface.',
     'Przemysłowy puls czeka tuż pod powierzchnią.',
     'I won''t be the extension of your narcissism.','Hybrid','Signal'),
    ('krk-006','006','Kraków','Małopolska','krakow',65,86,50.1,19.9,
     'Old stone. New noise. One line locked inside.',
     'Stary kamień. Nowy hałas. Jedna linia zamknięta w środku.',
     'Through the flames you''ll find your way.','From The Ashes','Genesis'),
    ('ldz-007','007','Łódź','Łódzkie','lodz',53,56,51.8,19.5,
     'Follow the thread through brick, rails and reinvention.',
     'Idź za nicią przez cegłę, tory i miasto wymyślone na nowo.',
     'Follow the thread through brick, rails and reinvention.','AREA Transmission','Signal'),
    ('szc-008','008','Szczecin','Zachodniopomorskie','szczecin',14,29,53.4,14.6,
     'The signal drifts inland from water shaped like a maze.',
     'Sygnał płynie w głąb lądu od wody ułożonej jak labirynt.',
     'The signal drifts inland from water shaped like a maze.','AREA Transmission','Signal'),
    ('lub-009','009','Lublin','Lubelskie','lublin',82,63,51.2,22.6,
     'Listen where old gates carry a new frequency.',
     'Słuchaj tam, gdzie stare bramy niosą nową częstotliwość.',
     'Listen where old gates carry a new frequency.','AREA Transmission','Signal'),
    ('rze-010','010','Rzeszów','Podkarpackie','rzeszow',82,87,50.0,22.0,
     'A southern pulse hides between motion and open sky.',
     'Południowy puls ukrywa się między ruchem a otwartym niebem.',
     'A southern pulse hides between motion and open sky.','AREA Transmission','Signal'),
    ('bia-011','011','Białystok','Podlaskie','bialystok',85,35,53.1,23.2,
     'At the forest''s edge, the quiet signal travels furthest.',
     'Na skraju lasu cichy sygnał dociera najdalej.',
     'At the forest''s edge, the quiet signal travels furthest.','AREA Transmission','Signal'),
    ('tor-012','012','Toruń','Kujawsko-Pomorskie','torun',47,37,53.0,18.6,
     'Look up, then follow the orbit back to the street.',
     'Spójrz w górę, potem sprowadź orbitę z powrotem na ulicę.',
     'Look up, then follow the orbit back to the street.','AREA Transmission','Signal')
)
INSERT INTO area_drops (
    workspace_id, id, number, city_id, city, region, signal_city_slug, map_x, map_y,
    approximate_lat, approximate_lng, exact_lat, exact_lng,
    radius_meters, max_claims, starts_at, ends_at,
    clue_en, clue_pl, collectible_line, collectible_track, collectible_edition,
    collectible_riddle, active
)
SELECT
    workspace.id, seed.id, seed.number, city.id, seed.city, seed.region,
    seed.signal_city_slug, seed.map_x, seed.map_y,
    seed.approximate_lat, seed.approximate_lng, NULL, NULL,
    100, 25,
    '2026-07-27T08:00:00+02:00'::timestamptz,
    '2027-12-31T23:59:59+01:00'::timestamptz,
    seed.clue_en, seed.clue_pl, seed.collectible_line,
    seed.collectible_track, seed.collectible_edition, 'Yanus', false
FROM workspaces AS workspace
CROSS JOIN seed
INNER JOIN cities AS city
  ON city.country_code = 'PL'
 AND city.slug = seed.signal_city_slug
WHERE workspace.slug = 'virya'
ON CONFLICT (workspace_id, id) DO UPDATE SET
    number = EXCLUDED.number,
    city_id = EXCLUDED.city_id,
    city = EXCLUDED.city,
    region = EXCLUDED.region,
    signal_city_slug = EXCLUDED.signal_city_slug,
    map_x = EXCLUDED.map_x,
    map_y = EXCLUDED.map_y,
    approximate_lat = EXCLUDED.approximate_lat,
    approximate_lng = EXCLUDED.approximate_lng,
    radius_meters = EXCLUDED.radius_meters,
    max_claims = EXCLUDED.max_claims,
    starts_at = EXCLUDED.starts_at,
    ends_at = EXCLUDED.ends_at,
    clue_en = EXCLUDED.clue_en,
    clue_pl = EXCLUDED.clue_pl,
    collectible_line = EXCLUDED.collectible_line,
    collectible_track = EXCLUDED.collectible_track,
    collectible_edition = EXCLUDED.collectible_edition,
    collectible_riddle = EXCLUDED.collectible_riddle,
    updated_at = now();

DO $$
DECLARE
    seeded integer;
BEGIN
    SELECT count(*)::integer
    INTO seeded
    FROM area_drops AS area_drop
    INNER JOIN workspaces AS workspace ON workspace.id = area_drop.workspace_id
    WHERE workspace.slug = 'virya';

    IF seeded <> 12 THEN
        RAISE EXCEPTION 'VIRYA AREA expected 12 drops, found %', seeded;
    END IF;
END
$$;
