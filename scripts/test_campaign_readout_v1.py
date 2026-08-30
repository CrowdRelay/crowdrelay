"""Contract tests for the 1000 active metalheads campaign readout layer.

Verifies:
- Funnel endpoint uses the real 30d activation definition (consent +
  meaningful action), not account status.
- KPI view includes retained_30d (30d retention).
- Referral conversion endpoint exists and tracks sent → qualified → activated.
- Acquisition channels readout includes retained_30d.
- All endpoints are routed.
- SCHEMA_VERSION tracks the latest migration.
"""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
API = ROOT / "crates/crowdrelay-api/src"
INFRA = ROOT / "crates/crowdrelay-infra/src"
APPLICATION = ROOT / "crates/crowdrelay-application/src"
MIGRATIONS = ROOT / "migrations"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class FunnelActivationContract(unittest.TestCase):
    def setUp(self) -> None:
        self.audience = read(API / "audience.rs")
        self.models = read(API / "audience/models.rs")

    def test_funnel_uses_real_activation_not_status(self) -> None:
        # The old funnel checked status = 'active'. The new one must use
        # fan_last_meaningful_action + consent.
        self.assertIn("fan_last_meaningful_action", self.audience)
        self.assertIn("consented", self.audience)
        self.assertIn("INTERVAL '30 days'", self.audience)
        # The old status = 'active' check must be gone from the funnel.
        funnel_section = self.audience[
            self.audience.find("pub async fn funnel"):
            self.audience.find("pub async fn revenue")
        ]
        self.assertNotIn("status = 'active'", funnel_section)

    def test_funnel_row_has_expected_fields(self) -> None:
        self.assertIn("acquired_fans", self.models)
        self.assertIn("active_fans", self.models)
        self.assertIn("ticket_buyers", self.models)
        self.assertIn("attendees", self.models)


class RetentionContract(unittest.TestCase):
    def setUp(self) -> None:
        self.migration = read(MIGRATIONS / "0121_activation_kpi_retention.sql")
        self.control = read(APPLICATION / "autopilot/control.rs")
        self.infra = read(INFRA / "autopilot/operations/acquisition_channels.rs")

    def test_migration_adds_retained_30d(self) -> None:
        self.assertIn("retained_30d", self.migration)
        self.assertIn("CREATE OR REPLACE VIEW viryaos_fan_activation_kpi", self.migration)

    def test_control_struct_has_retained_30d(self) -> None:
        self.assertIn("retained_30d", self.control)

    def test_infra_loads_retained_30d(self) -> None:
        self.assertIn("retained_30d", self.infra)


class ReferralConversionContract(unittest.TestCase):
    def setUp(self) -> None:
        self.audience = read(API / "audience.rs")
        self.models = read(API / "audience/models.rs")
        self.routing = read(API / "routing.rs")

    def test_endpoint_exists(self) -> None:
        self.assertIn("pub async fn referral_conversion", self.audience)

    def test_endpoint_tracks_full_funnel(self) -> None:
        self.assertIn("referrals_sent", self.audience)
        self.assertIn("qualified", self.audience)
        self.assertIn("activated", self.audience)
        self.assertIn("reversed", self.audience)

    def test_endpoint_uses_real_activation(self) -> None:
        self.assertIn("fan_last_meaningful_action", self.audience)
        self.assertIn("INTERVAL '30 days'", self.audience)

    def test_row_struct_exists(self) -> None:
        self.assertIn("ReferralConversionRow", self.models)
        self.assertIn("referrals_sent", self.models)
        self.assertIn("qualified", self.models)
        self.assertIn("activated", self.models)
        self.assertIn("reversed", self.models)

    def test_endpoint_is_routed(self) -> None:
        self.assertIn("/v1/admin/analytics/referral-conversion", self.routing)
        self.assertIn("audience::referral_conversion", self.routing)


class SchemaVersionContract(unittest.TestCase):
    def test_schema_version_matches_latest_migration(self) -> None:
        meta = read(API / "meta.rs")
        # SCHEMA_VERSION is now auto-discovered by build.rs from the
        # migrations directory, so we verify the env-var pattern is present
        # and build.rs exists rather than checking a hardcoded number.
        self.assertIn("CROWDRELAY_SCHEMA_VERSION", meta)
        self.assertTrue((ROOT / "crates/crowdrelay-api/build.rs").is_file())


class FanListActivationContract(unittest.TestCase):
    """The fan list must surface activation state, not just account status."""

    def setUp(self) -> None:
        self.models = read(API / "audience/models.rs")
        self.query = read(API / "audience/query_support.rs")
        self.handlers = read(API / "audience/engagement_handlers.rs")

    def test_fan_card_has_activation_fields(self) -> None:
        self.assertIn("consented", self.models)
        self.assertIn("last_activity_at", self.models)
        self.assertIn("activation_state", self.models)

    def test_fan_list_query_supports_activation_filter(self) -> None:
        self.assertIn("activation", self.models)
        self.assertIn("activation_filter", self.query)
        self.assertIn("fan_last_meaningful_action", self.query)

    def test_fan_list_handler_validates_activation_filter(self) -> None:
        self.assertIn("activation", self.handlers)
        self.assertIn("inactive_no_consent", self.handlers)
        self.assertIn("inactive_never_acted", self.handlers)
        self.assertIn("inactive_window_expired", self.handlers)

    def test_activation_state_values_are_documented(self) -> None:
        # All 5 states must appear in the query
        for state in [
            "active",
            "inactive_account_closed",
            "inactive_no_consent",
            "inactive_never_acted",
            "inactive_window_expired",
        ]:
            self.assertIn(state, self.query)


class ChannelBestActionContract(unittest.TestCase):
    """Channel performance must report the strongest action, not just counts."""

    def setUp(self) -> None:
        self.infra = read(INFRA / "autopilot/operations/acquisition_channels.rs")

    def test_best_action_is_populated_from_sql(self) -> None:
        self.assertIn("best_action", self.infra)
        self.assertIn("MeaningfulAction::parse", self.infra)
        # The SQL must check for ticket purchases (the strongest signal)
        self.assertIn("ticket_purchase", self.infra)
        self.assertIn("merch_purchase", self.infra)
        self.assertIn("qualified_referral", self.infra)

    def test_best_action_priority_order(self) -> None:
        # ticket_purchase must be checked before signal_session
        ticket_pos = self.infra.find("'ticket_purchase'")
        signal_pos = self.infra.find("'signal_session'")
        self.assertGreater(signal_pos, ticket_pos)


class TicketSaleWatchContract(unittest.TestCase):
    """Ticket-sale watch detection via SQL growth_debt loader.

    The domain module was removed — detection is done entirely in SQL
    by the growth_debt loader in infra. These tests verify the SQL
    integration is still wired correctly.
    """

    def setUp(self) -> None:
        self.growth_debt = read(ROOT / "crates/crowdrelay-domain/src/growth_debt.rs")
        self.infra = read(INFRA / "autopilot/operations/growth_debt.rs")

    def test_growth_debt_kind_registered(self) -> None:
        self.assertIn("TicketSalesBehindPace", self.growth_debt)
        self.assertIn("ticket_sales_behind_pace", self.growth_debt)

    def test_wired_into_growth_debt_loader(self) -> None:
        self.assertIn("ticket_sales_behind_pace", self.infra)
        self.assertIn("pace_comparison", self.infra)
        # The SQL must enforce the minimum history count
        self.assertIn("HAVING count(*) >= 2", self.infra)

    def test_uses_real_activation_not_account_status(self) -> None:
        # The detector must compare paid tickets, not account status
        self.assertIn("paid_tickets", self.infra)


class CalendarRoutingWiringContract(unittest.TestCase):
    """Calendar routing conflict must be wired into the growth_debt loader."""

    def setUp(self) -> None:
        self.infra = read(INFRA / "autopilot/operations/growth_debt.rs")

    def test_calendar_routing_cte_exists(self) -> None:
        self.assertIn("calendar_routing_conflicts", self.infra)
        self.assertIn("show_pairs", self.infra)

    def test_uses_show_cost_ledger_distances(self) -> None:
        self.assertIn("viryaos_show_cost_ledger", self.infra)
        self.assertIn("distance_km", self.infra)

    def test_thresholds_match_domain_defaults(self) -> None:
        # 400 km for consecutive days, 800 km for 2-day gap
        # These MUST match CalendarRoutingPolicy::default() in the domain.
        # If they drift, the SQL will raise conflicts the domain would not,
        # or vice versa.
        self.assertIn("> 400", self.infra)
        self.assertIn("> 800", self.infra)
        # Pin the domain defaults too, so changing one without the other
        # fails this test.
        domain = read(ROOT / "crates/crowdrelay-domain/src/calendar_routing.rs")
        self.assertIn("max_consecutive_day_km: 400", domain)
        self.assertIn("max_two_day_gap_km: 800", domain)

    def test_wired_into_union_all(self) -> None:
        self.assertIn("SELECT * FROM calendar_routing_conflicts", self.infra)


class CityFunnelContract(unittest.TestCase):
    """Per-city funnel endpoint must exist and be routed."""

    def setUp(self) -> None:
        self.audience = read(API / "audience.rs")
        self.models = read(API / "audience/models.rs")
        self.routing = read(API / "routing.rs")
        self.openapi = read(ROOT / "openapi/openapi.yaml")

    def test_endpoint_exists(self) -> None:
        self.assertIn("pub async fn city_funnel", self.audience)

    def test_uses_real_activation(self) -> None:
        self.assertIn("fan_last_meaningful_action", self.audience)
        self.assertIn("INTERVAL '30 days'", self.audience)

    def test_has_bookable_flag(self) -> None:
        self.assertIn("bookable", self.models)
        self.assertIn("bookable", self.audience)

    def test_row_struct_exists(self) -> None:
        self.assertIn("CityFunnelRow", self.models)
        self.assertIn("city_slug", self.models)
        self.assertIn("city_name", self.models)
        self.assertIn("country_code", self.models)
        self.assertIn("active_30d", self.models)
        self.assertIn("consented", self.models)

    def test_endpoint_is_routed(self) -> None:
        self.assertIn("/v1/admin/analytics/city-funnel", self.routing)
        self.assertIn("audience::city_funnel", self.routing)

    def test_endpoint_in_openapi(self) -> None:
        self.assertIn("/admin/analytics/city-funnel", self.openapi)
        self.assertIn("getAdminAudienceCityFunnel", self.openapi)


class AgentScorecardContract(unittest.TestCase):
    """The agent scorecard must show results, not logs."""

    def setUp(self) -> None:
        self.scorecard = read(API / "autopilot/scorecard.rs")
        self.routing = read(API / "routing.rs")
        self.openapi = read(ROOT / "openapi/openapi.yaml")

    def test_endpoint_is_routed(self) -> None:
        self.assertIn("/v1/admin/autopilot/scorecard", self.routing)
        self.assertIn("scorecard_handler", self.routing)

    def test_endpoint_in_openapi(self) -> None:
        self.assertIn("/admin/autopilot/scorecard", self.openapi)
        self.assertIn("getAutopilotScorecard", self.openapi)

    def test_has_status_section(self) -> None:
        self.assertIn("AgentStatus", self.scorecard)
        self.assertIn("agent_enabled", self.scorecard)
        self.assertIn("dry_run", self.scorecard)
        self.assertIn("posture", self.scorecard)
        self.assertIn("live_capabilities", self.scorecard)
        self.assertIn("parked_capabilities", self.scorecard)

    def test_has_week_summary(self) -> None:
        self.assertIn("WeekSummary", self.scorecard)
        self.assertIn("executed", self.scorecard)
        self.assertIn("succeeded", self.scorecard)
        self.assertIn("failed", self.scorecard)
        self.assertIn("parked", self.scorecard)
        self.assertIn("success_rate_basis_points", self.scorecard)

    def test_has_track_record(self) -> None:
        self.assertIn("TrackRecord", self.scorecard)
        self.assertIn("improved", self.scorecard)
        self.assertIn("worsened", self.scorecard)
        self.assertIn("unmeasured", self.scorecard)
        self.assertIn("measurement_coverage_basis_points", self.scorecard)

    def test_has_recent_results(self) -> None:
        self.assertIn("RecentResult", self.scorecard)
        self.assertIn("outcome", self.scorecard)
        self.assertIn("delta_basis_points", self.scorecard)
        self.assertIn("completed_at", self.scorecard)

    def test_has_context_breakdown(self) -> None:
        self.assertIn("ContextBreakdown", self.scorecard)
        self.assertIn("by_context", self.scorecard)

    def test_uses_existing_tables_not_new_state(self) -> None:
        # Must read from existing ledger tables, not create new ones
        self.assertIn("viryaos_autopilot_actions", self.scorecard)
        self.assertIn("viryaos_autopilot_outcomes", self.scorecard)
        self.assertIn("viryaos_growth_posture", self.scorecard)
        self.assertIn("viryaos_executor_capabilities", self.scorecard)

    def test_no_writes(self) -> None:
        # The scorecard must be read-only
        lowered = self.scorecard.lower()
        self.assertNotIn("insert into", lowered)
        self.assertNotIn("update ", lowered)
        self.assertNotIn("delete from", lowered)


if __name__ == "__main__":
    unittest.main()
