"""Contract tests for what counts as an active fan.

The campaign KPI is a thousand deduplicated thirty-day-active people, so the
number this defines is the one everything is steered by. The properties pinned
here are the ones whose failure inflates it quietly: a definition that drifts
between Rust and SQL, an account status masquerading as activity, and a window
that stops being enforced.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
DOMAIN = ROOT / "crates/crowdrelay-domain/src/fan_activation.rs"
MIGRATION = ROOT / "migrations/0078_fan_last_meaningful_action.sql"
METRICS = ROOT / "crates/crowdrelay-infra/src/autopilot/growth_metrics.rs"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def shipped(rust: str) -> str:
    return rust.split("#[cfg(test)]", 1)[0]


class FanActivationContract(unittest.TestCase):
    def setUp(self) -> None:
        self.domain = read(DOMAIN)
        self.migration = read(MIGRATION)
        self.metrics = read(METRICS)

    def test_the_sql_function_covers_exactly_the_domain_actions(self) -> None:
        # The definition exists twice by necessity — once as a rule and once as
        # a set-oriented query — so the drift has to be caught by a test rather
        # than by somebody noticing the number looks wrong.
        block = (
            shipped(self.domain)
            .split("pub const fn all() -> [Self; 6] {", 1)[1]
            .split("\n    }", 1)[0]
        )
        rust = {name for name in re.findall(r"Self::(\w+)", block)}
        wire = {
            re.search(rf'Self::{name} => "([a-z_]+)"', self.domain).group(1)
            for name in rust
        }
        commented = set(re.findall(r"-- ([a-z_]+):", self.migration))
        self.assertEqual(
            wire,
            commented,
            "every domain action needs a branch in fan_last_meaningful_action",
        )

    def test_an_open_account_is_not_treated_as_activity(self) -> None:
        # The failure this whole module exists to prevent: a fan who signed up
        # two years ago and did nothing since is not an active fan.
        self.assertIn("signing_up_is_not_being_active", self.domain)
        self.assertIn("'active_fans', 'Fans with an open account'", self.metrics)
        self.assertIn(
            "'activated_fans_30d', 'Fans active in the last 30 days'", self.metrics
        )

    def test_the_kpi_series_requires_consent_and_a_recent_action(self) -> None:
        totals = self.metrics.split("activated_fans_30d,", 1)[0].rsplit(
            "AS activated_fans", 1
        )[0]
        self.assertIn("consent.granted", totals)
        self.assertIn("purpose = 'marketing'", totals)
        self.assertIn("INTERVAL '30 days'", totals)
        self.assertIn("fan_last_meaningful_action", totals)

    def test_only_the_latest_consent_decision_counts(self) -> None:
        # A fan who granted and then withdrew has withdrawn.
        totals = self.metrics.split("AS activated_fans_30d", 1)[0]
        self.assertIn("max(latest.recorded_at)", totals)

    def test_the_window_is_one_constant_rather_than_a_scattered_number(self) -> None:
        self.assertIn("ACTIVITY_WINDOW_DAYS: i64 = 30", shipped(self.domain))
        self.assertIn("the_window_is_exactly_thirty_days", self.domain)

    def test_a_future_timestamp_never_counts_as_activity(self) -> None:
        # A bad import must not be able to inflate the only number that matters.
        rule = shipped(self.domain).split("pub fn activation_state", 1)[1]
        self.assertIn("occurred_at > now", rule)
        self.assertIn("an_action_stamped_in_the_future_never_counts", self.domain)

    def test_inactivity_says_which_wall_the_fan_hit(self) -> None:
        # "never consented" and "consented then did nothing" call for
        # completely different responses.
        for reason in ("AccountClosed", "NoConsent", "NeverActed", "WindowExpired"):
            self.assertIn(f"InactiveReason::{reason}", shipped(self.domain))

    def test_synthetic_runs_are_excluded(self) -> None:
        # A synthetic run is the system talking to itself.
        self.assertIn("NOT run.synthetic", self.migration)

    def test_a_revoked_session_is_not_a_visit(self) -> None:
        self.assertIn("revoked_at IS NULL", self.migration)

    def test_the_per_fan_lookups_are_indexed(self) -> None:
        # The KPI query calls the function once per fan, so every branch it
        # touches needs to be cheap or the metric cycle degrades as fans grow.
        for index in (
            "merch_order_facts_fan_confirmed_idx",
            "referral_attributions_referrer_accepted_idx",
            "fan_sessions_fan_last_seen_idx",
        ):
            self.assertIn(index, self.migration)

    def test_activation_is_measured_per_city_because_the_loop_is_geographic(
        self,
    ) -> None:
        # A thousand fans scattered across Europe produce no bookable city;
        # two hundred across four Polish cities produce four shows.
        self.assertIn("async fn sync_city_series", self.metrics)
        self.assertIn("'city', city.id", self.metrics)
        self.assertIn("FROM fan_city_interests", self.metrics)

    def test_a_city_series_reuses_the_one_activation_definition(self) -> None:
        city = self.metrics.split("let city_points = sqlx::query_scalar", 1)[1].split(
            '"#', 1
        )[0]
        self.assertIn("fan_last_meaningful_action", city)
        self.assertIn("INTERVAL '30 days'", city)
        self.assertIn("consent.granted", city)

    def test_a_quiet_city_records_a_zero_rather_than_no_row(self) -> None:
        # Skipping the row would read as a dead feed rather than as a city that
        # cooled, and those call for opposite responses.
        city = self.metrics.split("let city_points = sqlx::query_scalar", 1)[1].split(
            '"#', 1
        )[0]
        self.assertIn("LEFT JOIN per_city", city)
        self.assertIn("COALESCE(per_city.activated, 0)", city)

    def test_a_city_series_is_never_retired(self) -> None:
        # A city that goes quiet is a fact worth keeping visible, and retiring
        # on an empty month would flap the series every time somebody moved.
        retire = self.metrics.split("WITH retired AS", 1)[1].split('"#', 1)[0]
        self.assertNotIn("'city'", retire)

    def test_the_activity_cache_is_never_the_source_of_truth(self) -> None:
        # A denormalized column maintained by triggers across six tables would
        # be subtly wrong forever. Recomputed wholesale, it can only ever be
        # stale by one cycle.
        migration = read(ROOT / "migrations/0080_fan_last_activity_and_city_beacons.sql")
        self.assertIn("ADD COLUMN last_activity_at timestamptz", migration)
        self.assertIn("Never the source of truth", migration)
        refresh = self.metrics.split("UPDATE fans AS fan", 1)[1].split('"#', 1)[0]
        self.assertIn("fan_last_meaningful_action", refresh)
        # Only rows that actually changed, so the cycle does not rewrite the
        # whole table every six hours.
        self.assertIn("IS DISTINCT FROM", refresh)

    def test_a_warm_city_can_be_scouted_without_a_show(self) -> None:
        # The show-scoped rule can never fire for a band with no shows, which
        # is exactly the band that needs scene nodes most.
        beacons = read(ROOT / "crates/crowdrelay-domain/src/beacons.rs")
        self.assertIn("pub fn evaluate_city_beacon_discovery", beacons)
        self.assertIn("a_warm_city_with_no_scene_nodes_is_scouted_without_a_show", beacons)

    def test_city_warmth_is_measured_in_activated_fans_not_signups(self) -> None:
        # A hundred dormant accounts is not a reason to go hunting for
        # promoters, and treating it as one wastes the scouting budget.
        beacons = read(ROOT / "crates/crowdrelay-domain/src/beacons.rs")
        rule = beacons.split("pub fn evaluate_city_beacon_discovery", 1)[1].split(
            "\n}", 1
        )[0]
        self.assertIn("snapshot.activated_fans", rule)
        self.assertNotIn("signups", rule)
        self.assertIn("a_city_of_dormant_accounts_is_not_warm", beacons)

    def test_a_consented_fan_gets_a_referral_code_before_any_message(self) -> None:
        # Every message can carry an invite, and an invite with no code behind
        # it is a dead end.
        lifecycle = read(ROOT / "crates/crowdrelay-domain/src/audience_lifecycle.rs")
        self.assertIn("IssueReferralCode", lifecycle)
        self.assertIn("a_fan_without_a_code_gets_one_before_any_message", lifecycle)
        self.assertIn("issuing_a_code_outranks_even_a_milestone", lifecycle)
        # Consent still outranks it: a code for somebody who never agreed to
        # hear from us implies a relationship that does not exist.
        self.assertIn("a_fan_without_consent_gets_no_code_either", lifecycle)

    def test_a_fan_can_never_hold_two_referral_codes(self) -> None:
        # Two codes split one fan's referrals across two identities and make
        # the ledger wrong.
        candidates = read(
            ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/candidates.rs"
        )
        self.assertIn('format!("action:referral-code:{}", snapshot.fan_id)', candidates)
        actions = read(ROOT / "crates/crowdrelay-infra/src/autopilot/actions.rs")
        guard = actions.split("INSERT INTO referral_codes", 1)[1].split('"#', 1)[0]
        self.assertIn("WHERE NOT EXISTS", guard)
        self.assertIn("ON CONFLICT DO NOTHING", guard)

    def test_the_domain_holds_no_provider_or_sql_concept(self) -> None:
        for forbidden in ("sqlx", "reqwest", "SELECT ", "INSERT "):
            self.assertNotIn(forbidden, self.domain)


if __name__ == "__main__":
    unittest.main()
