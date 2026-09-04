#!/usr/bin/env python3
"""Source contract for the fan retention loop.

The loop is: a fan names a city, the city gets coordinates, the fan becomes
reachable, a show near them produces a notification, and a push carries it to
the device. Every stage of it has been present and inert at some point, and none
of those failures produced an error -- the symptom each time was silence.

These assertions pin the three things that made the silence possible: nothing
called the emitter, nothing filled in coordinates, and a disabled push flag
returned without a word. Each one compiles and tests clean when broken again,
which is why the gate is here rather than in the type system.
"""
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class RetentionLoopV1Contract(unittest.TestCase):
    def test_the_nearby_show_emitter_has_a_caller(self):
        # `/v1/internal/nearby-gigs/emit-due` was routed, tested and never
        # called by anything: no worker, no n8n node, no cron. A scheduler must
        # exist and must be spawned, or the whole loop is decoration.
        worker = (ROOT / "crates/crowdrelay-worker/src/nearby_gigs.rs").read_text()
        main = (ROOT / "crates/crowdrelay-worker/src/main.rs").read_text()
        self.assertIn("emit_due_nearby_gigs", worker)
        self.assertIn("NearbyGigScheduler::new", main)
        self.assertIn("nearby_gig_scheduler.run(", main)

    def test_requested_cities_get_coordinates_without_a_human(self):
        worker = (ROOT / "crates/crowdrelay-worker/src/city_geocoding.rs").read_text()
        main = (ROOT / "crates/crowdrelay-worker/src/main.rs").read_text()
        migration = (ROOT / "migrations/0230_city_geocoding_attempts.sql").read_text()
        # Coordinates, not moderation, are what the nearby query gates on, so
        # the worker must write latitude and longitude.
        self.assertIn("SET latitude = $2", worker)
        self.assertIn("longitude = $3", worker)
        # The row is the cache: a city that has coordinates is never selected
        # again, which is what keeps the work proportional to what is unresolved.
        self.assertIn("WHERE latitude IS NULL", worker)
        # Bounded retries: a name nobody can resolve must stop costing requests.
        self.assertIn("geocode_attempts < $1", worker)
        self.assertIn("geocode_next_attempt_at", worker)
        self.assertIn("geocode_attempts integer NOT NULL DEFAULT 0", migration)
        self.assertIn("cities_geocode_pending_idx", migration)
        # And it must actually be spawned.
        self.assertIn("CityGeocodeWorker::from_env", main)
        self.assertIn("worker.run(city_geocode_shutdown)", main)

    def test_the_geocoder_is_swappable_and_identifies_itself(self):
        worker = (ROOT / "crates/crowdrelay-worker/src/city_geocoding.rs").read_text()
        # A trait, so the public geocoder can be replaced by a self-hosted or
        # paid one without touching the loop.
        self.assertIn("pub trait GeocodeProvider", worker)
        self.assertIn("CROWDRELAY_CITY_GEOCODING_BASE_URL", worker)
        # Nominatim's usage policy makes an identifying agent a condition of
        # access; an anonymous client is how you get blocked.
        self.assertIn("CROWDRELAY_CITY_GEOCODING_CONTACT", worker)
        self.assertIn(".user_agent(", worker)

    def test_disabled_push_delivery_is_never_silent(self):
        worker = (ROOT / "crates/crowdrelay-worker/src/push_delivery.rs").read_text()
        # The gap: `run_once` read the flag and returned `Ok(())`, so a
        # workspace with push switched off queued deliveries forever and the
        # only symptom was that fans heard nothing.
        self.assertIn("flag_transition(", worker)
        self.assertIn("FlagTransition::TurnedOff", worker)
        self.assertIn("push_delivery_enabled", worker)
        # Scoped to `run_once`: `run` also warns, and matching that would pass
        # even if the disabled path went back to returning silently.
        run_once = worker.split("async fn run_once(&mut self)", 1)[1]
        short_circuit = run_once.index("if !enabled {")
        self.assertIn("tracing::warn!", run_once[:short_circuit])

    def test_every_stage_of_the_loop_is_counted(self):
        ops = (ROOT / "crates/crowdrelay-api/src/ops/query_support.rs").read_text()
        models = (ROOT / "crates/crowdrelay-api/src/ops/models.rs").read_text()
        spec = (ROOT / "openapi/openapi.yaml").read_text()
        for stage in (
            "cities_awaiting_coordinates",
            "cities_resolved",
            "fans_with_coordinates",
            "nearby_eligible_fans",
            "pushes_queued",
            "pushes_sent",
            "pushes_delivered",
        ):
            self.assertIn(stage, ops, f"{stage} is not queried")
            self.assertIn(stage, spec, f"{stage} is not in the published contract")
        self.assertIn("struct SignalRetentionLoop", models)
        self.assertIn("SignalRetentionLoop:", spec)
        # `pending_city_requests` keeps meaning "awaiting moderation" -- both
        # consumers label it that way -- so the coordinate blocker had to be a
        # separate count rather than a redefinition.
        self.assertIn("city.moderation_status = 'pending'", ops)


if __name__ == "__main__":
    unittest.main()
