"""Contract tests for the release runway — the fifth play.

A release is the biggest organic growth moment the band gets, and the runway
is the sequence that makes it compound: pre-save, announce, curator wave,
release-day push, sustain. Five steps, anchored on `release_at`, and the only
play with a third anchor kind.

Pinned here:
- The anchor kind is `release`, not `event` or `fan`.
- The anchor query reads `viryaos_release_plans`, not `events` or `fans`.
- The audience query reaches all consented fans, not one fan or one show's fans.
- The curator wave is the only third-party step; everything else is owned or
  first-party.
- The pre-save and curator wave reach nobody (StepAudience::None).
- The success metric is Spotify followers.
- The play kind, anchor kind, and step kinds are all in the migration's CHECK
  constraints, so the database refuses a row the code does not understand.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATIONS = ROOT / "migrations"
MIGRATION = MIGRATIONS / "0118_release_runway_play.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/plays.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/plays.rs"
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
EVALUATE = ROOT / "crates/crowdrelay-application/src/autopilot/evaluate/plays.rs"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class ReleaseRunwayContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = strip_sql_comments(read(MIGRATION))
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)
        self.model = read(MODEL)
        self.evaluate = read(EVALUATE)

    # --- the anchor is a release, not a show or a fan -------------------

    def test_the_play_kind_is_in_the_schema_check(self) -> None:
        self.assertIn("'release_runway'", self.sql)
        check = self.sql.split("viryaos_plays_play_kind_check", 1)[1]
        self.assertIn("release_runway", check)

    def test_the_anchor_kind_is_release(self) -> None:
        check = self.sql.split("viryaos_plays_anchor_kind_check", 1)[1]
        self.assertIn("'release'", check)
        # And the domain agrees.
        arm = self.domain.split("pub enum PlayAnchorKind", 1)[1].split("}", 1)[0]
        self.assertIn("Release", arm)

    def test_the_anchor_query_reads_release_plans(self) -> None:
        anchors = self.infra.split("const PLAY_RELEASE_ANCHORS_SQL", 1)[1].split('"#;', 1)[0]
        self.assertIn("viryaos_release_plans", anchors)
        self.assertIn("plan.release_at", anchors)
        self.assertIn("plan.active", anchors)
        self.assertIn("anchor_kind = 'release'", anchors)
        # And it does not read events or fans.
        self.assertNotIn("FROM events", anchors)
        self.assertNotIn("FROM fans", anchors)

    def test_the_anchor_ref_carries_a_release_plan_id(self) -> None:
        # The enum has three variants; check the whole enum body.
        enum_body = self.model.split("pub enum PlayAnchorRef", 1)[1].split("\n}\n", 1)[0]
        self.assertIn("Release { release_plan_id: ReleasePlanId }", enum_body)
        # And the kind() and id() match — find the impl block after the enum.
        impl_block = self.model.split("impl PlayAnchorRef", 1)[1].split("\n}\n", 1)[0]
        self.assertIn("Self::Release { .. } => PlayAnchorKind::Release", impl_block)
        self.assertIn("Self::Release { release_plan_id }", impl_block)

    def test_the_evaluator_subjects_a_release_to_a_release_plan(self) -> None:
        # The evaluator's anchor_subject must map Release to ReleasePlan,
        # not to Event or Fan.
        fn = self.evaluate.split("fn anchor_subject", 1)[1].split("\n}", 1)[0]
        self.assertIn("PlayAnchorRef::Release { release_plan_id }", fn)
        self.assertIn("ActionSubject::ReleasePlan(release_plan_id)", fn)

    # --- the audience is all consented fans -----------------------------

    def test_the_audience_query_reaches_all_consented_fans(self) -> None:
        audience = self.infra.split("const PLAY_RELEASE_AUDIENCE_SQL", 1)[1].split('"#;', 1)[0]
        self.assertIn("fan_consents", audience)
        self.assertIn("consent.purpose = 'marketing'", audience)
        self.assertIn("latest_consent.granted", audience)
        self.assertIn("fan.status = 'active'", audience)
        # And it does not filter by city, event, or a single fan id.
        self.assertNotIn("fan.id = $3", audience)
        self.assertNotIn("city", audience.lower())
        self.assertNotIn("event", audience.lower())

    def test_the_audience_query_is_selected_by_the_release_anchor_kind(self) -> None:
        match = self.infra.split("match anchor_kind", 1)[1].split("})", 1)[0]
        self.assertIn("PlayAnchorKind::Release => PLAY_RELEASE_AUDIENCE_SQL", match)

    # --- the curator wave is the only third-party step ------------------

    def test_five_step_kinds_are_in_the_schema_check(self) -> None:
        check = self.sql.split("viryaos_play_steps_step_kind_check", 1)[1]
        for kind in (
            "release_presave_live",
            "release_audience_announce",
            "release_curator_wave",
            "release_day_push",
            "release_sustain_ask",
        ):
            self.assertIn(kind, check)

    def test_the_curator_wave_is_third_party(self) -> None:
        classes = self.domain.split("pub const fn action_class", 1)[1].split("\n    }", 1)[0]
        arm = classes.split("Self::ReleaseCuratorWave", 1)[1].split("\n        }", 1)[0]
        self.assertIn("ActionClass::ThirdParty", arm)

    def test_the_pre_save_is_first_party_reversible(self) -> None:
        classes = self.domain.split("pub const fn action_class", 1)[1].split("\n    }", 1)[0]
        arm = classes.split("Self::ReleasePresaveLive", 1)[1].split("\n        }", 1)[0]
        self.assertIn("ActionClass::FirstPartyReversible", arm)

    def test_the_owned_audience_steps_are_owned_audience(self) -> None:
        classes = self.domain.split("pub const fn action_class", 1)[1].split("\n    }", 1)[0]
        for step in ("ReleaseAudienceAnnounce", "ReleaseDayPush", "ReleaseSustainAsk"):
            arm = classes.split(f"Self::{step}", 1)[1].split("\n        }", 1)[0]
            self.assertIn("ActionClass::OwnedAudience", arm)

    # --- pre-save and curator wave reach nobody -------------------------

    def test_pre_save_and_curator_wave_have_no_audience(self) -> None:
        # The audience() function maps each step kind to Fans or None.
        # Pre-save and curator wave should be in the None group; the three
        # audience steps should be in the Fans group.
        audience_fn = self.domain.split("pub const fn audience", 1)[1]
        # Find the full match block.
        match_block = audience_fn.split("match self", 1)[1].split("\n    }", 1)[0]
        # The None branch contains pre-save and curator wave.
        none_arm = match_block.split("StepAudience::None", 1)[1].split("StepAudience::Fans", 1)[0]
        # Actually, the structure is: a big OR group => Fans, then a group => None.
        # Let's just check the whole function body.
        full = audience_fn.split("\n    }", 1)[0]
        self.assertIn("ReleasePresaveLive", full)
        self.assertIn("ReleaseCuratorWave", full)
        self.assertIn("ReleaseAudienceAnnounce", full)
        self.assertIn("ReleaseDayPush", full)
        self.assertIn("ReleaseSustainAsk", full)
        # Pre-save and curator wave should be grouped with ListingSweep (None).
        # The None branch is the one that includes ListingSweep.
        none_section = full.split("Self::ListingSweep", 1)[1].split("StepAudience::Fans", 1)[0]
        self.assertIn("ReleasePresaveLive", none_section)
        self.assertIn("ReleaseCuratorWave", none_section)
        # The audience steps should be in the Fans branch (before ListingSweep).
        fans_section = full.split("Self::AnnounceAsk", 1)[1].split("Self::ListingSweep", 1)[0]
        self.assertIn("ReleaseAudienceAnnounce", fans_section)
        self.assertIn("ReleaseDayPush", fans_section)
        self.assertIn("ReleaseSustainAsk", fans_section)

    # --- the schedule ---------------------------------------------------

    def test_the_schedule_spans_six_weeks_around_the_release(self) -> None:
        specs = self.domain.split("const RELEASE_RUNWAY_STEPS", 1)[1].split("];", 1)[0]
        offsets = [
            int(days) * 24 if days else int(hours)
            for days, hours in re.findall(r"offset_hours: (?:(-?\d+) \* 24|(-?\d+))", specs)
        ]
        self.assertEqual(offsets, [-28 * 24, -14 * 24, -7 * 24, 0, 14 * 24])

    def test_each_step_has_its_own_template(self) -> None:
        templates = self.domain.split("pub const fn template_key", 1)[1].split("\n    }", 1)[0]
        for key in (
            "play.release_runway.presave_live",
            "play.release_runway.audience_announce",
            "play.release_runway.curator_wave",
            "play.release_runway.release_day_push",
            "play.release_runway.sustain_ask",
        ):
            self.assertIn(f'"{key}"', templates)

    # --- the success metric ---------------------------------------------

    def test_the_success_metric_is_spotify_followers(self) -> None:
        metrics = self.domain.split("pub const fn success_metric", 1)[1].split("\n    }", 1)[0]
        arm = metrics.split("Self::ReleaseRunway =>", 1)[1]
        self.assertIn('("spotify", "followers")', arm)

    # --- the play does not require a tracked link -----------------------

    def test_the_runway_does_not_require_a_follow_link(self) -> None:
        dispatch = self.infra.split("let follow_link = match play_kind", 1)[1].split(";", 1)[0]
        self.assertIn("PlayKind::ReleaseRunway", dispatch)
        # And it is in the None branch, not the Some branch.
        none_branch = dispatch.split("=> None", 1)[0] + "=> None"
        self.assertIn("ReleaseRunway", none_branch)

    # --- the learning table accepts the new play kind -------------------

    def test_the_learning_table_accepts_release_runway(self) -> None:
        check = self.sql.split("viryaos_play_learning_play_kind_check", 1)[1]
        self.assertIn("release_runway", check)


if __name__ == "__main__":
    unittest.main()
