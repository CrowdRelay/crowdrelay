"""Contract tests for channel performance.

This is the read model a zero-budget campaign is steered by: which community
produced people who stayed. The failures worth pinning are the ones that make a
bad channel look good — merging signups with activation, inventing a "direct"
bucket for people nobody can trace, crediting a return visit to the wrong
channel, and dropping the unattributable part out of view.
"""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
DOMAIN = ROOT / "crates/crowdrelay-domain/src/acquisition_channel.rs"
MIGRATION = ROOT / "migrations/0079_smart_link_channel_identity.sql"
LOADER = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/acquisition_channels.rs"
APP = ROOT / "crates/crowdrelay-application/src/autopilot/control.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def shipped(rust: str) -> str:
    return rust.split("#[cfg(test)]", 1)[0]


def query(loader: str) -> str:
    return loader.split("async fn load_acquisition_channels", 1)[1].split('"#', 1)[0]


class AcquisitionChannelContract(unittest.TestCase):
    def setUp(self) -> None:
        self.domain = read(DOMAIN)
        self.loader = read(LOADER)

    def test_there_is_no_direct_bucket_anywhere(self) -> None:
        # Every analytics tool invents one, and it makes the channel you cannot
        # see look like a channel that works.
        code = shipped(self.domain)
        self.assertNotIn("Direct", code)
        self.assertIn("a_signup_with_no_click_is_unknown_and_never_direct", self.domain)

    def test_each_broken_link_in_the_chain_has_its_own_reason_and_fix(self) -> None:
        code = shipped(self.domain)
        for reason in ("NoVisitor", "NoClickBeforeSignup", "LinkNotLabelled"):
            self.assertIn(f"UnattributedReason::{reason}", code)
        self.assertIn("pub const fn remedy", code)

    def test_the_first_acquisition_event_is_the_acquisition(self) -> None:
        # A later event is a return visit, and crediting the channel somebody
        # came back through would rob the one that found them.
        sql = query(self.loader)
        self.assertIn("ORDER BY event.occurred_at ASC", sql)
        self.assertIn("LIMIT 1", sql)

    def test_the_click_must_precede_the_signup(self) -> None:
        sql = query(self.loader)
        self.assertIn("click.occurred_at <= arrival.signed_up_at", sql)
        self.assertIn("ORDER BY click.occurred_at DESC", sql)

    def test_activation_reuses_the_one_definition(self) -> None:
        # Not a second copy of "active" that can drift from the KPI series.
        sql = query(self.loader)
        self.assertIn("fan_last_meaningful_action", sql)
        self.assertIn("INTERVAL '30 days'", sql)
        self.assertIn("consent.granted", sql)

    def test_only_the_latest_consent_decision_counts(self) -> None:
        sql = query(self.loader)
        self.assertIn("max(latest.recorded_at)", sql)

    def test_signups_and_activation_are_reported_side_by_side(self) -> None:
        # A channel with two hundred signups and four active people is a bad
        # channel wearing a good number.
        entry = read(APP).split("pub struct ChannelPerformance {", 1)[1].split("}", 1)[0]
        self.assertIn("pub signups: u32", entry)
        self.assertIn("pub activated_30d: u32", entry)

    def test_a_rate_from_an_empty_denominator_is_not_a_zero(self) -> None:
        self.assertIn("activation_basis_points: Option<u32>", read(APP))
        self.assertIn("(signups > 0).then(", self.loader)

    def test_the_unattributable_part_stays_in_view(self) -> None:
        # A report that hides its unknowns is how a large attribution gap goes
        # unnoticed for a month.
        summary = read(APP).split("pub struct AcquisitionChannels {", 1)[1].split("}", 1)[0]
        self.assertIn("pub unattributed: Vec<UnattributedGroup>", summary)

    def test_groups_sharing_a_reason_are_merged_into_one_line(self) -> None:
        # An operator wants one line per fix, not one per underlying shape.
        self.assertIn("iter_mut().find(|group| group.reason == reason)", self.loader)

    def test_the_result_is_bounded(self) -> None:
        sql = query(self.loader)
        self.assertIn("GROUP BY", sql)
        self.assertIn("LIMIT $3", sql)
        self.assertIn("MAX_SNAPSHOTS_PER_CONTEXT", self.loader)

    def test_the_attribution_walk_is_indexed(self) -> None:
        migration = read(MIGRATION)
        self.assertIn("click_events_visitor_time_idx", migration)
        self.assertIn("smart_links_channel_idx", migration)

    def test_channel_identity_is_free_text_rather_than_a_constraint(self) -> None:
        # Trying a new community is a Tuesday afternoon decision; a CHECK here
        # would mean a migration every time.
        migration = read(MIGRATION)
        for column in ("channel_source", "channel_community", "channel_creative"):
            self.assertIn(f"ADD COLUMN {column} text", migration)
            self.assertNotIn(f"{column} text NOT NULL", migration)

    def test_the_endpoint_is_admin_only_and_documented(self) -> None:
        self.assertIn('"/v1/admin/autopilot/acquisition-channels"', read(ROUTING))
        openapi = read(OPENAPI)
        block = openapi.split("/admin/autopilot/acquisition-channels:", 1)[1].split(
            "  /admin/autopilot/tour-economics:", 1
        )[0]
        self.assertIn("adminBearer", block)
        self.assertIn("PrivateNoStore", block)

    def test_the_domain_holds_no_provider_or_sql_concept(self) -> None:
        for forbidden in ("sqlx", "reqwest", "SELECT ", "INSERT "):
            self.assertNotIn(forbidden, self.domain)


if __name__ == "__main__":
    unittest.main()
