"""Contract tests for show economics.

The booking gate has always refused a negative margin; it just had no idea what
the cost was, because `estimated_cost_minor` arrived from outside and nothing
computed it. These pin the properties that decide whether the band drives 500 km
for nothing: vehicle count derived rather than assumed, missing inputs refused
rather than filled in, revenue never invented, and an uncosted show never
committed to automatically.
"""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"
CONTROL = ROOT / "crates/crowdrelay-infra/src/autopilot/control.rs"
MIGRATION = ROOT / "migrations/0077_viryaos_tour_economics.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/tour_economics.rs"
LIVE = ROOT / "crates/crowdrelay-domain/src/live_opportunities.rs"
LOADER = ROOT / "crates/crowdrelay-infra/src/autopilot/decisions/opportunity_reads.rs"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


def shipped(rust: str) -> str:
    """Declarations only. The unit tests assert on these very strings."""
    return rust.split("#[cfg(test)]", 1)[0]


def code_only(rust: str) -> str:
    """Declarations with comments stripped.

    The prose deliberately names the things it forbids, so a search for them
    has to look at what ships rather than at what explains it.
    """
    return "\n".join(
        line
        for line in shipped(rust).splitlines()
        if not line.lstrip().startswith(("//", "///", "//!"))
    )


class TourEconomicsContract(unittest.TestCase):
    def setUp(self) -> None:
        self.migration = read(MIGRATION)
        self.domain = read(DOMAIN)
        self.live = read(LIVE)

    def test_vehicle_count_is_derived_from_crew_and_backline(self) -> None:
        # Assuming one vehicle halves the fuel and the tolls, and answers "can
        # we afford this gig" wrong in the direction that costs the band.
        code = shipped(self.domain)
        self.assertIn("pub const fn required_vehicles", code)
        self.assertIn("policy.vehicle.seats", code)
        self.assertIn("policy.backline_litres", code)
        self.assertIn("five_people_and_a_backline_need_two_cars", self.domain)
        self.assertIn("a_second_car_roughly_doubles_the_road_costs", self.domain)

    def test_road_costs_scale_with_vehicle_count(self) -> None:
        code = code_only(self.domain)
        fuel = code.split("let fuel_minor =", 1)[1].split("let tolls_minor", 1)[0]
        tolls = code.split("let tolls_minor =", 1)[1].split("let nights_away", 1)[0]
        self.assertIn("vehicles_i128", fuel)
        self.assertIn("vehicles_i128", tolls)

    def test_every_missing_input_is_refused_and_named(self) -> None:
        code = shipped(self.domain)
        for missing in (
            "Distance",
            "TransportRate",
            "VehicleCapacity",
            "CrewSize",
            "AccommodationRate",
        ):
            self.assertIn(f"MissingInput::{missing}", code)
        self.assertIn("an_unknown_distance_is_never_filled_in", self.domain)
        self.assertIn("an_unset_road_rate_is_not_a_free_road", self.domain)
        self.assertIn("a_trip_that_needs_beds_refuses_when_nobody_priced_a_bed", self.domain)

    def test_an_unset_rate_is_never_read_as_a_free_one(self) -> None:
        # A free road makes every distant gig look profitable.
        code = shipped(self.domain)
        self.assertIn("if !flat_rate && !itemised", code)
        self.assertIn("MissingInput::TransportRate", code)
        # And an overnight with no room rate is not free beds.
        self.assertIn("policy.accommodation_minor_per_room_night <= 0", code)

    def test_the_flat_rate_replaces_the_itemised_model_rather_than_adding(self) -> None:
        # Adding both would double-count the road on every trip.
        code = shipped(self.domain)
        self.assertIn("TransportBasis::FlatRate", code)
        self.assertIn("TransportBasis::FuelAndTolls", code)
        self.assertIn(
            "the_flat_rate_replaces_the_fuel_arithmetic_rather_than_adding_to_it", self.domain
        )
        self.assertIn("a_flat_rate_scales_when_the_trip_needs_more_cars_than_it_covers", self.domain)

    def test_unknown_cost_is_never_profitable(self) -> None:
        code = shipped(self.domain)
        clears = code.split("pub const fn clears_floor", 1)[1].split("\n    }", 1)[0]
        self.assertIn("Self::Insufficient { .. } => false", clears)
        self.assertIn("unknown_cost_is_never_treated_as_profitable", self.domain)

    def test_merch_and_bar_revenue_are_not_modelled(self) -> None:
        # Real, unpredictable, and an agent that books a losing show because it
        # assumed merch would cover it is worse than one that says no.
        code = code_only(self.domain).lower()
        for invented in ("revenue", "uplift", "expected_attendance", "merch_sales"):
            self.assertNotIn(invented, code)
        # The only merch in the model is the volume it occupies in the van.
        self.assertNotIn("merch_minor", code)

    def test_an_uncosted_show_is_never_submitted_automatically(self) -> None:
        gate = self.live.split("fn may_auto_submit", 1)[1].split("\n}", 1)[0]
        self.assertIn("snapshot.costed_from_logistics", gate)
        self.assertIn(
            "an_uncosted_show_is_prepared_for_a_human_but_never_submitted_alone", self.live
        )

    def test_an_uncosted_show_scores_no_economics_points(self) -> None:
        # Otherwise an unknown cost reads as break-even and outranks a costed
        # profitable gig.
        score = self.live.split("fn economics_score", 1)[1].split("\n}", 1)[0]
        self.assertIn("if !snapshot.costed_from_logistics", score)
        self.assertIn("return 0;", score)

    def test_the_computed_cost_wins_over_the_typed_one(self) -> None:
        loader = read(LOADER)
        self.assertIn("estimate_show_cost(", loader)
        self.assertIn(
            "costed.cost().map_or(row.estimated_cost_minor, |cost| cost.total_cost_minor)",
            loader,
        )
        self.assertIn("costed_from_logistics: costed.cost().is_some()", loader)

    def test_the_band_configuration_is_read_once_per_batch(self) -> None:
        loader = read(LOADER)
        snapshots = loader.split("async fn load_live_opportunity_snapshots_impl", 1)[1]
        body = snapshots.split("rows.into_iter()", 1)
        self.assertIn("load_tour_economics", body[0])
        self.assertNotIn("load_tour_economics", body[1])

    def test_a_missing_configuration_row_reports_uncosted_not_free(self) -> None:
        loader = read(LOADER)
        self.assertIn("map_or_else(TourEconomicsPolicy::default", loader)
        code = shipped(self.domain)
        default = code.split("impl Default for TourEconomicsPolicy", 1)[1].split(
            "\n}", 1
        )[0]
        self.assertIn("fuel_price_minor_per_litre: 0", default)

    def test_the_per_offer_facts_are_nullable_and_bounded(self) -> None:
        # NULL is honest: nobody supplied it. A default would be a guess.
        schema = strip_sql_comments(self.migration)
        self.assertIn("ADD COLUMN distance_km integer CHECK (distance_km IS NULL", schema)
        self.assertIn("ADD COLUMN nights_away smallint CHECK (nights_away IS NULL", schema)

    def test_every_configured_rate_is_bounded(self) -> None:
        schema = strip_sql_comments(self.migration)
        for column in (
            "vehicle_seats",
            "vehicle_cargo_litres",
            "vehicle_fuel_centilitres_per_100km",
            "max_vehicles",
            "crew_size",
            "backline_litres",
            "crew_per_room",
            "overnight_threshold_km",
        ):
            self.assertIn(f"CHECK ({column} BETWEEN", schema)
        for column in (
            "fuel_price_minor_per_litre",
            "toll_minor_per_km",
            "accommodation_minor_per_room_night",
            "per_diem_minor_per_person_day",
            "fixed_overhead_minor",
            "minimum_margin_minor",
        ):
            self.assertIn(f"CHECK ({column} >= 0)", schema)

    def test_the_config_exists_for_existing_and_future_workspaces(self) -> None:
        self.assertIn("INSERT INTO viryaos_tour_economics (workspace_id)", self.migration)
        self.assertIn("viryaos_provision_tour_economics", self.migration)
        self.assertIn("AFTER INSERT ON workspaces", self.migration)

    def test_money_arithmetic_cannot_wrap_into_a_bargain(self) -> None:
        code = shipped(self.domain)
        self.assertIn("i128", code)
        self.assertIn("fn money(product: i128) -> i64", code)
        self.assertIn(
            "extreme_inputs_saturate_instead_of_wrapping_into_a_bargain", self.domain
        )

    def test_the_walk_away_fee_exists_for_negotiation(self) -> None:
        # Phase 8 negotiates up from this. Negotiating without a floor is
        # guessing with the band's money.
        code = shipped(self.domain)
        self.assertIn("pub walk_away_fee_minor: i64", code)
        self.assertIn("minimum_margin_minor", code)

    def test_the_domain_holds_no_provider_or_sql_concept(self) -> None:
        for forbidden in ("sqlx", "http", "reqwest", "SELECT ", "INSERT "):
            self.assertNotIn(forbidden, self.domain)


    def test_the_configuration_is_editable_over_the_admin_api(self) -> None:
        # Without an endpoint the only way to set the band's fuel price is SQL,
        # and a number nobody can change is a number nobody will fix.
        self.assertIn('"/v1/admin/autopilot/tour-economics"', read(ROUTING))
        openapi = read(OPENAPI)
        block = openapi.split("/admin/autopilot/tour-economics:", 1)[1].split(
            "  /admin/autopilot/next-best-actions", 1
        )[0]
        self.assertIn("adminBearer", block)
        self.assertIn("IdempotencyKey", block)
        self.assertIn("'409'", block)

    def test_a_stale_version_is_a_conflict_not_a_silent_overwrite(self) -> None:
        # Two people editing the van in the same afternoon must not overwrite
        # each other's fuel price.
        control = read(CONTROL).split("async fn set_tour_economics", 1)[1].split(
            "\n    async fn", 1
        )[0]
        self.assertIn("WHERE workspace_id=$1 AND version=$18", control)
        self.assertIn("ok_or(RepositoryError::Conflict)", control)
        self.assertIn("version=version+1", control)

    def test_saving_the_configuration_is_idempotent(self) -> None:
        control = read(CONTROL).split("async fn set_tour_economics", 1)[1].split(
            "\n    async fn", 1
        )[0]
        self.assertIn("insert_operator_action", control)
        self.assertIn("replayed: true", control)

    def test_a_half_configured_policy_can_still_be_saved(self) -> None:
        # An operator enters figures one at a time; the refusal for a missing
        # road rate belongs at estimate time, naming the field.
        code = shipped(self.domain)
        valid = code.split("pub const fn is_valid", 1)[1].split("\n    }", 1)[0]
        self.assertNotIn("transport_minor_per_100km_round_trip > 0", valid)
        self.assertIn("a_half_configured_workspace_can_still_be_saved", self.domain)

    def test_the_shape_that_would_divide_by_nothing_is_rejected(self) -> None:
        code = shipped(self.domain)
        valid = code.split("pub const fn is_valid", 1)[1].split("\n    }", 1)[0]
        for guard in (
            "self.vehicle.seats >= 1",
            "self.max_vehicles >= 1",
            "self.transport_rate_covers_vehicles >= 1",
            "self.crew_per_room >= 1",
        ):
            self.assertIn(guard, valid)


if __name__ == "__main__":
    unittest.main()
