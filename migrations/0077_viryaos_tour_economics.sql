-- What a show costs to play, so the booking gate stops trusting a typed number.
--
-- `viryaos_team_opportunities.estimated_cost_minor` has always been an input
-- from outside: nothing computed it, so every "can we take this gig" answer was
-- only as good as whatever somebody entered. The domain now costs the trip from
-- the band's own configuration plus one fact per offer, and the answer it gives
-- is the same shape as the question the operator actually asks -- home is
-- Wroclaw, the show is 500 km away, we travel in two cars, does the fee leave
-- money.
--
-- Two vehicles is roughly double the fuel and double the tolls, and vehicle
-- count is derived from crew and backline against one vehicle's capacity rather
-- than assumed. Merch and bar revenue are deliberately not modelled: they are
-- real and unpredictable, and an agent that books a losing show because it
-- assumed merch would cover it is worse than one that says no.

CREATE TABLE viryaos_tour_economics (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,

    -- One vehicle the band owns or hires.
    vehicle_seats smallint NOT NULL DEFAULT 5
        CHECK (vehicle_seats BETWEEN 1 AND 60),
    vehicle_cargo_litres integer NOT NULL DEFAULT 900
        CHECK (vehicle_cargo_litres BETWEEN 0 AND 100000),
    -- Centilitres per 100 km: 800 is 8.0 l/100 km. Centilitres so a realistic
    -- figure survives integer arithmetic without a float anywhere near money.
    vehicle_fuel_centilitres_per_100km integer NOT NULL DEFAULT 800
        CHECK (vehicle_fuel_centilitres_per_100km BETWEEN 0 AND 100000),
    max_vehicles smallint NOT NULL DEFAULT 3 CHECK (max_vehicles BETWEEN 1 AND 20),

    crew_size smallint NOT NULL DEFAULT 5 CHECK (crew_size BETWEEN 0 AND 100),
    backline_litres integer NOT NULL DEFAULT 1200
        CHECK (backline_litres BETWEEN 0 AND 100000),

    -- Zero means "not configured", and the domain reports that as insufficient
    -- evidence rather than as free fuel. Zero fuel makes every distant gig look
    -- profitable, which is the exact failure this table exists to prevent.
    fuel_price_minor_per_litre bigint NOT NULL DEFAULT 0
        CHECK (fuel_price_minor_per_litre >= 0),
    toll_minor_per_km bigint NOT NULL DEFAULT 0 CHECK (toll_minor_per_km >= 0),
    accommodation_minor_per_room_night bigint NOT NULL DEFAULT 0
        CHECK (accommodation_minor_per_room_night >= 0),
    crew_per_room smallint NOT NULL DEFAULT 2 CHECK (crew_per_room BETWEEN 1 AND 20),
    per_diem_minor_per_person_day bigint NOT NULL DEFAULT 0
        CHECK (per_diem_minor_per_person_day >= 0),
    -- Paid whether or not the show sells: rehearsal, loading, wear.
    fixed_overhead_minor bigint NOT NULL DEFAULT 0 CHECK (fixed_overhead_minor >= 0),

    -- One-way distance at or beyond which the band stays the night. Operator
    -- policy, not something the domain infers.
    overnight_threshold_km integer NOT NULL DEFAULT 350
        CHECK (overnight_threshold_km BETWEEN 0 AND 20000),
    -- What the band must clear above cost for a show to be worth playing. This
    -- is the floor Phase 8 negotiates up from.
    minimum_margin_minor bigint NOT NULL DEFAULT 0 CHECK (minimum_margin_minor >= 0),

    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER viryaos_tour_economics_set_updated_at
BEFORE UPDATE ON viryaos_tour_economics
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

INSERT INTO viryaos_tour_economics (workspace_id)
SELECT id FROM workspaces
ON CONFLICT (workspace_id) DO NOTHING;

CREATE OR REPLACE FUNCTION viryaos_provision_tour_economics()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO viryaos_tour_economics (workspace_id)
    VALUES (NEW.id)
    ON CONFLICT (workspace_id) DO NOTHING;
    RETURN NEW;
END;
$$;

CREATE TRIGGER workspaces_provision_tour_economics
AFTER INSERT ON workspaces
FOR EACH ROW
EXECUTE FUNCTION viryaos_provision_tour_economics();

-- The one fact that has to arrive per offer. Nullable, and NULL is honest: an
-- opportunity whose distance nobody supplied is prepared for a human and never
-- submitted automatically. Filling it with a band average is how an agent talks
-- a band into a loss-making drive.
ALTER TABLE viryaos_team_opportunities
    ADD COLUMN distance_km integer CHECK (distance_km IS NULL OR distance_km BETWEEN 0 AND 20000);

-- Stated by the promoter or the operator when known; otherwise the overnight
-- threshold decides. A stated count is a fact and beats the policy fallback.
ALTER TABLE viryaos_team_opportunities
    ADD COLUMN nights_away smallint CHECK (nights_away IS NULL OR nights_away BETWEEN 0 AND 30);
