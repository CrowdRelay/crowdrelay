"""Contract tests for the follow-ask ladder — the first play anchored on a person.

The track-us ask reaches the fans of one show at the moment that show is the
thing on their mind. It therefore never reaches the fan who bought a ticket last
spring and has had no date near them since, which is most of the list most of
the time. The ladder is for exactly those people: one ask, a nudge six weeks
later, a last one at four months.

Making the anchor a fan is the part that could go quietly wrong. Every read in
the play machinery joined `events` on `anchor_id` because there was nothing else
an anchor could be, and an assumption like that does not fail loudly — it
returns no rows, the play skips every step it has, and the campaign looks like
it ran. The claims pinned here are the ones whose absence would produce exactly
that silence, plus the two that keep the ladder from becoming a mailing list
with an ask attached.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATIONS = ROOT / "migrations"
MIGRATION = MIGRATIONS / "0095_viryaos_play_follow_ask_ladder.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/plays.rs"
CANDIDATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/plays.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/plays.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


def latest_constraint(pattern: str) -> str:
    """The named CHECK as the newest migration to define it leaves it.

    Pinned to the migration that first added a value, this claim would keep
    passing against a set the database had stopped enforcing — the same trap
    the context-set claim in test_plays_v1 avoids the same way.
    """
    latest = ""
    for path in sorted(MIGRATIONS.glob("*.sql")):
        found = re.findall(pattern, strip_sql_comments(read(path)), re.DOTALL)
        if found:
            latest = found[-1]
    return latest


class FollowAskLadderContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = strip_sql_comments(read(MIGRATION))
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)

    # --- the anchor is a choice now, not an assumption -------------------

    def test_the_play_kind_decides_the_anchor_kind(self) -> None:
        # Not the row and not the caller. A play started against the wrong kind
        # of anchor reads an audience that cannot exist, and settles every step
        # as "no eligible recipients" while looking like it ran.
        self.assertIn("pub const fn anchor_kind(self) -> PlayAnchorKind", self.domain)
        mapping = self.domain.split("pub const fn anchor_kind", 1)[1].split("\n    }", 1)[0]
        fan_arm = next(
            (line for line in mapping.splitlines() if "PlayAnchorKind::Fan" in line), ""
        )
        self.assertIn("Self::FollowAskLadder", fan_arm)
        self.assertIn("PlayAnchorKind::Event", mapping)

    def test_the_stored_anchor_kinds_match_the_rust_enum(self) -> None:
        # Named, not just shaped: waves have an `anchor_kind` too, and matching
        # on the shape alone compared the play enum against theirs.
        stored = latest_constraint(
            r"viryaos_plays_anchor_kind_check\s+CHECK \(anchor_kind IN \((.*?)\)\)"
        )
        self.assertTrue(stored)
        declared = set(
            re.findall(
                r'Self::\w+ => "([a-z_]+)"',
                self.domain.split("impl PlayAnchorKind", 1)[1].split("pub fn parse", 1)[0],
            )
        )
        self.assertEqual(set(re.findall(r"'([a-z_]+)'", stored)), declared)

    def test_a_stored_anchor_kind_that_disagrees_refuses_the_read(self) -> None:
        # There is no code path that writes one, so a row carrying one means
        # something else is wrong. Running the audience query anyway is how a
        # campaign contacts the wrong people.
        snapshots = self.infra.split("load_play_snapshots_impl", 1)[1]
        self.assertIn("if anchor_kind != kind.anchor_kind()", snapshots)
        self.assertIn("return Err(RepositoryError::Unexpected)", snapshots)
        self.assertIn("if anchor.anchor.kind() != kind.anchor_kind()", read(CANDIDATE))

    def test_each_anchor_kind_reads_its_own_table(self) -> None:
        # One query with a branch inside it would be a statement that means
        # neither question.
        for name in (
            "PLAY_EVENT_ANCHORS_SQL",
            "PLAY_FAN_ANCHORS_SQL",
            "PLAY_AUDIENCE_SQL",
            "PLAY_FAN_AUDIENCE_SQL",
        ):
            self.assertIn(f"const {name}", self.infra)
        statements = self.infra.split("const fn anchor_statement", 1)[1].split("\n}", 1)[0]
        self.assertIn("PLAY_EVENT_ANCHORS_SQL", statements)
        self.assertIn("PlayKind::FollowAskLadder => (PLAY_FAN_ANCHORS_SQL, true)", statements)

    def test_one_ladder_per_fan_for_ever(self) -> None:
        # The unique constraint from 0088 already carries this once anchor_kind
        # can be 'fan'; the loader must not offer a fan who has one either, or
        # every cycle spends a round trip losing the same race.
        anchors = self.infra.split("const PLAY_FAN_ANCHORS_SQL", 1)[1].split('"#;', 1)[0]
        self.assertIn("NOT EXISTS", anchors)
        self.assertIn("play.anchor_kind = 'fan'", anchors)
        self.assertIn("play.anchor_id = fan.id", anchors)

    # --- what makes it a campaign rather than a mailing list -------------

    def test_engaged_means_they_did_something_recently(self) -> None:
        # Every consented fan is a mailing list with an ask attached, which is
        # the thing this play exists instead of.
        anchors = self.infra.split("const PLAY_FAN_ANCHORS_SQL", 1)[1].split('"#;', 1)[0]
        self.assertIn("ticket_orders", anchors)
        self.assertIn("event_interests", anchors)
        self.assertIn("INTERVAL '1 year'", anchors)
        self.assertIn("latest_consent.granted", anchors)
        self.assertIn("fan.status = 'active'", anchors)

    def test_no_rung_is_scheduled_before_its_anchor(self) -> None:
        # The anchor is the moment the fan qualified, so a negative offset would
        # put a rung in the past and settle it as a skip the cycle it was made.
        specs = self.domain.split("const FOLLOW_ASK_LADDER_STEPS", 1)[1].split("];", 1)[0]
        offsets = [
            int(days) * 24 if days else int(hours)
            for days, hours in re.findall(r"offset_hours: (?:(-?\d+) \* 24|(-?\d+))", specs)
        ]
        self.assertEqual(len(offsets), 3, "three rungs")
        self.assertTrue(all(offset >= 0 for offset in offsets))
        self.assertEqual(offsets, sorted(offsets), "the rungs climb in order")
        # And the class still comes from the step kind, never from the spec.
        self.assertNotIn("class: ActionClass::", specs)
        self.assertIn("class: PlayStepKind::", specs)

    def test_the_lead_time_floor_applies_only_to_a_play_that_precedes_its_anchor(self) -> None:
        # Holding a fan-anchored play to a show-shaped minimum lead refuses
        # every one of them, for ever, with no error anywhere.
        rule = self.domain.split("pub fn play_is_worth_starting", 1)[1].split("\n}", 1)[0]
        self.assertIn("spec.offset_hours < 0", rule)
        self.assertIn("if anticipates_anchor && hours_until_anchor", rule)

    def test_every_rung_has_its_own_copy(self) -> None:
        # Three sends of the same template are three sends whose separate
        # results cannot be told apart.
        templates = self.domain.split("pub const fn template_key", 1)[1].split("\n    }", 1)[0]
        for key in ("play.follow_ask.first", "play.follow_ask.second", "play.follow_ask.final"):
            self.assertIn(f'"{key}"', templates)

    def test_a_rung_is_an_owned_audience_message_to_one_person(self) -> None:
        classes = self.domain.split("pub const fn action_class", 1)[1].split("\n    }", 1)[0]
        ladder = classes.split("Self::FollowAskFirst", 1)[1]
        self.assertIn("ActionClass::OwnedAudience", ladder.split("\n        }", 1)[0])
        audience = self.domain.split("pub const fn audience", 1)[1].split("\n    }", 1)[0]
        self.assertIn("Self::FollowAskFirst", audience.split("StepAudience::Fans", 1)[0])

    def test_the_ladder_reaches_the_anchor_fan_and_nobody_else(self) -> None:
        audience = self.infra.split("const PLAY_FAN_AUDIENCE_SQL", 1)[1].split('"#;', 1)[0]
        self.assertIn("fan.id = $3", audience)
        # Committed work, not delivered work: reading only the delivered table
        # re-offers the same fan every cycle and the ladder never climbs.
        self.assertIn("viryaos_autopilot_actions", audience)
        self.assertIn("status <> 'cancelled'", audience)
        self.assertIn("latest_consent.granted", audience)

    def test_a_fan_who_withdrew_consent_is_a_withdrawn_anchor(self) -> None:
        # The same fact as a cancelled show, about a different kind of anchor:
        # the remaining rungs are skipped and recorded, not sent.
        active = self.infra.split("CASE play.anchor_kind", 1)[1].split("END AS anchor_active", 1)[0]
        self.assertIn("fan.status = 'active'", active)
        self.assertIn("consent.granted", active)
        self.assertIn("ORDER BY consent.recorded_at DESC", active)
        # The fan row is joined only for a fan anchor, so an event-anchored
        # play cannot be voided by a fan that happens to share its uuid.
        joins = self.infra.split("LEFT JOIN fans AS fan", 1)[1].split('"#,', 1)[0]
        self.assertIn("play.anchor_kind = 'fan'", joins)

    # --- the one call to action -----------------------------------------

    def test_the_ask_points_at_a_tracked_link_the_operator_owns(self) -> None:
        # A call to action nobody tracks turns the campaign into an
        # unmeasurable guess, and a URL the agent invented is a URL nobody
        # agreed to publish.
        self.assertIn('FOLLOW_ASK_SMART_LINK_SLUG: &str = "follow"', self.infra)
        anchors = self.infra.split("const PLAY_FAN_ANCHORS_SQL", 1)[1].split('"#;', 1)[0]
        self.assertIn("smart_links", anchors)
        self.assertIn("link.active", anchors)
        dispatch = self.infra.split("pub(super) async fn execute_play_step", 1)[1]
        self.assertIn("call_to_action_url", dispatch)
        link = self.infra.split("async fn follow_ask_link", 1)[1]
        self.assertIn("RepositoryError::Conflict", link)

    def test_a_play_with_no_show_carries_no_show(self) -> None:
        # Carrying an event id a fan-anchored play does not have would make the
        # executor render an ask about a date that has nothing to do with it.
        self.assertIn("event_id: Option<EventId>", self.infra)
        self.assertIn("pub const fn event_id(self) -> Option<EventId>", read(
            ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
        ))
        self.assertIn("event_id: snapshot.anchor.event_id()", read(CANDIDATE))

    # --- the closed sets stay closed -------------------------------------

    def test_the_stored_kinds_match_the_rust_enums(self) -> None:
        for pattern, header in (
            (r"viryaos_plays_play_kind_check\s+CHECK \(play_kind IN \((.*?)\)\)", "impl PlayKind"),
            (
                r"viryaos_play_steps_step_kind_check\s+CHECK \(step_kind IN \((.*?)\)\)",
                "impl PlayStepKind",
            ),
        ):
            stored = latest_constraint(pattern)
            self.assertTrue(stored, pattern)
            declared = set(
                re.findall(
                    r'Self::\w+ => "([a-z_]+)"',
                    self.domain.split(header, 1)[1].split("pub fn parse", 1)[0],
                )
            )
            self.assertEqual(set(re.findall(r"'([a-z_]+)'", stored)), declared)

    def test_the_learning_table_knows_the_new_play(self) -> None:
        # A play kind the learning table rejects is a play whose record cannot
        # be written, and the failure surfaces long after the campaign ran.
        learning = latest_constraint(
            r"viryaos_play_learning_play_kind_check\s+CHECK \(play_kind IN \((.*?)\)\)"
        )
        self.assertIn("follow_ask_ladder", learning)

    def test_the_operator_reads_the_anchor_rather_than_a_bare_uuid(self) -> None:
        openapi = read(OPENAPI)
        self.assertIn("follow_ask_ladder", openapi)
        entry = openapi.split("    PlayLedgerEntry:", 1)[1].split("\n    PlayRecordCounts:", 1)[0]
        self.assertIn("anchor:", entry)
        self.assertNotIn("event_id: { type: string, format: uuid }\n        anchor_at", entry)


if __name__ == "__main__":
    unittest.main()
