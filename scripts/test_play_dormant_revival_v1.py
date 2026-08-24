"""Contract tests for dormant revival — the cheapest audience the band has left.

The list already holds people who bought a ticket two years ago and have heard
nothing since. Reaching them costs nothing, and the agent currently does nothing
with them.

The whole risk of this play is that it degrades into a mailing machine. Four
things stop it, and each of them is a `NOT EXISTS` somebody could delete without
any test noticing: dormant has to mean *was here and stopped*, not *is on the
list*; there has to be something to revive them with; the agent must not follow
a three-rung ladder with a revival; and it must stop after two.

Also pinned here: the success metric resolves to a real series. `read_series`
refuses when a metric key matches more than one series, and this play's metric
has both a workspace-level series and a per-city breakdown — so without the
workspace-level preference the campaign would be permanently unmeasurable while
looking measured.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATIONS = ROOT / "migrations"
MIGRATION = MIGRATIONS / "0096_viryaos_play_dormant_revival.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/plays.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/plays.rs"
OUTCOMES = ROOT / "crates/crowdrelay-infra/src/autopilot/play_outcomes.rs"
METRICS = ROOT / "crates/crowdrelay-infra/src/autopilot/growth_metrics.rs"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class DormantRevivalContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = strip_sql_comments(read(MIGRATION))
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)
        self.anchors = self.infra.split("const PLAY_DORMANT_ANCHORS_SQL", 1)[1].split('"#;', 1)[0]

    # --- dormant means was here and stopped ------------------------------

    def test_dormant_requires_having_been_engaged_at_some_point(self) -> None:
        # Somebody who never did anything is not dormant, they are a name on a
        # list, and writing to them is the failure this context exists to avoid.
        ever = self.anchors.split("AND NOT EXISTS", 1)[0]
        self.assertIn("ticket_orders", ever)
        self.assertIn("event_interests", ever)
        self.assertIn("latest_consent.granted", ever)
        self.assertIn("fan.status = 'active'", ever)

    def test_dormant_and_engaged_cannot_both_be_true(self) -> None:
        # The ladder takes activity inside a year; this takes its exact
        # complement. Two different windows would put one fan in both campaigns
        # at once, which is the same person hearing from the band twice about
        # two contradictory premises.
        ladder = self.infra.split("const PLAY_FAN_ANCHORS_SQL", 1)[1].split('"#;', 1)[0]
        self.assertIn("INTERVAL '1 year'", ladder)
        exclusions = self.anchors.split("AND NOT EXISTS", 1)[1]
        self.assertIn("INTERVAL '1 year'", exclusions)
        self.assertEqual(
            self.anchors.count("INTERVAL '1 year'"),
            2,
            "both recent-activity sources have to be excluded, not just one",
        )

    def test_there_has_to_be_something_to_revive_them_with(self) -> None:
        # A revival in a workspace with no upcoming date is "hello, remember
        # us".
        self.assertIn("event.status = 'published'", self.anchors)
        self.assertIn("event.starts_at > $3", self.anchors)

    def test_a_fan_the_agent_just_talked_at_is_left_alone(self) -> None:
        # The weekly envelope bounds contact; it does not know this fan has
        # just had a whole three-rung ladder.
        self.assertIn("viryaos_play_step_recipients", self.anchors)
        self.assertIn("INTERVAL '6 months'", self.anchors)

    def test_one_revival_per_fan_for_ever(self) -> None:
        self.assertIn("play.anchor_kind = 'fan'", self.anchors)
        self.assertIn("play.anchor_id = fan.id", self.anchors)

    # --- and it stops -----------------------------------------------------

    def test_two_messages_and_then_it_stops(self) -> None:
        # Somebody who ignored the band for a year and then ignored two
        # reminders has answered.
        specs = self.domain.split("const DORMANT_REVIVAL_STEPS", 1)[1].split("];", 1)[0]
        offsets = [
            int(days) * 24 if days else int(hours)
            for days, hours in re.findall(r"offset_hours: (?:(-?\d+) \* 24|(-?\d+))", specs)
        ]
        self.assertEqual(offsets, [0, 45 * 24])
        self.assertNotIn("class: ActionClass::", specs)
        self.assertIn("class: PlayStepKind::", specs)

    def test_each_message_has_its_own_copy(self) -> None:
        templates = self.domain.split("pub const fn template_key", 1)[1].split("\n    }", 1)[0]
        for key in ("play.dormant_revival.first", "play.dormant_revival.final"):
            self.assertIn(f'"{key}"', templates)

    def test_quiet_is_not_withdrawn_consent(self) -> None:
        # A fan who stopped turning up is still a fan of this workspace who
        # agreed to hear from it, so the class is the ordinary owned-audience
        # one and the ordinary envelope applies.
        classes = self.domain.split("pub const fn action_class", 1)[1].split("\n    }", 1)[0]
        arm = classes.split("Self::DormantRevivalFirst", 1)[1].split("\n        }", 1)[0]
        self.assertIn("ActionClass::OwnedAudience", arm)

    def test_the_revival_asks_for_no_tracked_link(self) -> None:
        # Unlike the follow-ask ladder, whose entire content is one call to
        # action. Requiring one here would block a play whose message is the
        # date itself.
        dispatch = self.infra.split("let follow_link = match play_kind", 1)[1].split(";", 1)[0]
        self.assertIn("PlayKind::DormantRevival", dispatch.split("=> {", 1)[0] + dispatch)
        self.assertIn("PlayKind::FollowAskLadder => Some(", dispatch)

    # --- the metric has to resolve ---------------------------------------

    def test_the_success_metric_is_the_thing_being_attempted(self) -> None:
        metrics = self.domain.split("pub const fn success_metric", 1)[1].split("\n    }", 1)[0]
        arm = metrics.split("Self::DormantRevival =>", 1)[1]
        self.assertIn('("signal", "activated_fans_30d")', arm)
        # And the workspace really declares that series first-party.
        self.assertIn("('signal', 'activated_fans_30d'", read(METRICS))

    def test_a_workspace_level_series_wins_over_its_own_breakdown(self) -> None:
        # `signal/activated_fans_30d` exists workspace-wide *and* per city.
        # Read as two answers to one question, the play is permanently
        # unmeasurable while reporting that it looked.
        reader = read(OUTCOMES).split("pub(super) async fn read_series", 1)[1].split("\n}", 1)[0]
        self.assertIn("subject_kind IS NULL", reader)
        self.assertIn("workspace_level", reader)
        # The fallback is load-bearing: bandsintown/trackers exists only per
        # event source, on purpose, because a workspace may sync two artists.
        self.assertIn(
            "WHERE workspace_level = EXISTS (SELECT 1 FROM matching WHERE workspace_level)",
            reader,
        )
        self.assertIn("LIMIT 2", reader)
        self.assertIn("ambiguous: true", reader)

    def test_the_recipient_lookback_is_indexed(self) -> None:
        # Without it the six-month guard is a scan of every recipient row in
        # the workspace, per candidate fan, every cycle.
        self.assertIn("viryaos_play_step_recipients_fan_idx", self.sql)
        self.assertIn("(workspace_id, fan_id, created_at DESC)", self.sql)


if __name__ == "__main__":
    unittest.main()
