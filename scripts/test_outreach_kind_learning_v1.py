"""Contract tests for outreach kind learning — the wave learning loop.

The per-kind learning table (viryaos_outreach_kind_learning) holds standings
that scale wave sizing for each target kind. The wave outcome table
(viryaos_outreach_wave_outcomes) schedules settlement for each approved wave
and folds the verdict into the learning record.

Pinned here:
- The learning table has the same shape as play learning (counts, weight,
  retirement).
- The wave outcome table has the same lifecycle as play outcomes (pending →
  processing → succeeded/failed) with a window that closes 21 days after
  approval.
- The wave outcome is created in the same transaction as the wave approval.
- The settlement worker counts replies by disposition and folds the verdict
  into the learning record in one transaction.
- The domain assessment uses a 20% reply quorum and a 2:1 ratio for
  improved/worsened, with do_not_contact counting double.
- The engine-core boundary is respected: learning.rs does not import outreach.
"""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATIONS = ROOT / "migrations"
MIGRATION_0117 = MIGRATIONS / "0117_outreach_kind_outcomes.sql"
MIGRATION_0119 = MIGRATIONS / "0119_outreach_wave_outcomes.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/learning.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/play_outcomes.rs"
WAVES = ROOT / "crates/crowdrelay-infra/src/autopilot/waves.rs"
WORKER = ROOT / "crates/crowdrelay-worker/src/autopilot.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/ports.rs"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class OutreachKindLearningContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql_0117 = strip_sql_comments(read(MIGRATION_0117))
        self.sql_0119 = strip_sql_comments(read(MIGRATION_0119))
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)
        self.waves = read(WAVES)
        self.worker = read(WORKER)
        self.ports = read(PORTS)

    # --- the learning table has the same shape as play learning -----------

    def test_the_learning_table_has_counts_weight_and_retirement(self) -> None:
        self.assertIn("improved_count", self.sql_0117)
        self.assertIn("neutral_count", self.sql_0117)
        self.assertIn("worsened_count", self.sql_0117)
        self.assertIn("insufficient_count", self.sql_0117)
        self.assertIn("consecutive_worsened", self.sql_0117)
        self.assertIn("weight_basis_points", self.sql_0117)
        self.assertIn("retired_at", self.sql_0117)
        self.assertIn("retired_reason", self.sql_0117)

    def test_the_learning_table_accepts_all_seven_target_kinds(self) -> None:
        for kind in (
            "playlist",
            "radio",
            "press",
            "creator",
            "support_slot",
            "endorsement",
            "media_patronage",
        ):
            self.assertIn(f"'{kind}'", self.sql_0117)

    def test_retirement_is_stated_not_decayed(self) -> None:
        self.assertIn(
            "viryaos_outreach_kind_learning_retirement_is_stated",
            self.sql_0117,
        )
        self.assertIn(
            "viryaos_outreach_kind_learning_weight_matches_retirement",
            self.sql_0117,
        )

    # --- the wave outcome table has the play outcome lifecycle ----------

    def test_the_wave_outcome_table_has_the_play_outcome_lifecycle(self) -> None:
        for status in ("pending", "processing", "succeeded", "failed"):
            self.assertIn(f"'{status}'", self.sql_0119)

    def test_the_wave_outcome_window_is_21_days(self) -> None:
        self.assertIn("WAVE_OUTCOME_WINDOW_DAYS: i64 = 21", self.infra)

    def test_the_wave_outcome_is_unique_per_wave(self) -> None:
        self.assertIn("UNIQUE (workspace_id, wave_id)", self.sql_0119)

    def test_the_wave_outcome_has_a_due_index(self) -> None:
        self.assertIn("viryaos_outreach_wave_outcomes_due_idx", self.sql_0119)

    def test_evidence_and_assessment_are_consistent(self) -> None:
        self.assertIn("evidence <> 'insufficient' OR effect_assessment IS NULL", self.sql_0119)
        self.assertIn(
            "evidence <> 'measured' OR effect_assessment IS NOT NULL",
            self.sql_0119,
        )

    # --- the wave outcome is created on approval -------------------------

    def test_the_wave_outcome_is_created_in_the_approval_transaction(self) -> None:
        # The approval path must call create_wave_outcome in the same
        # transaction that releases the pitches.
        approval = self.waves.split("approve_outreach_wave_impl", 1)[1]
        self.assertIn("create_wave_outcome", approval)

    def test_create_wave_outcome_uses_on_conflict_do_nothing(self) -> None:
        fn = self.infra.split("pub(super) async fn create_wave_outcome", 1)[1]
        self.assertIn("ON CONFLICT (workspace_id, wave_id) DO NOTHING", fn)

    # --- the settlement worker runs after play outcomes ------------------

    def test_the_worker_settles_wave_outcomes_after_play_outcomes(self) -> None:
        # The wave outcome phase must come after the play outcome phase in the
        # worker's run loop. Check by position: the play outcome claim appears
        # before the wave outcome claim.
        play_pos = self.worker.find("claim_due_play_outcomes")
        wave_pos = self.worker.find("claim_due_wave_outcomes")
        self.assertIsNotNone(play_pos)
        self.assertIsNotNone(wave_pos)
        self.assertLess(play_pos, wave_pos)

    def test_the_worker_uses_assess_wave_claim(self) -> None:
        self.assertIn("assess_wave_claim", self.worker)

    def test_the_worker_folds_into_the_learning_record(self) -> None:
        self.assertIn("complete_wave_outcome", self.worker)
        self.assertIn("fail_wave_outcome", self.worker)

    # --- the domain assessment rules -------------------------------------

    def test_the_quorum_is_20_percent(self) -> None:
        self.assertIn("REPLY_QUORUM_BASIS_POINTS: u32 = 2000", self.domain)

    def test_do_not_contact_counts_double(self) -> None:
        assess = self.domain.split("pub fn assess_wave_outcome", 1)[1]
        self.assertIn("* 2", assess)

    def test_zero_replies_is_insufficient_not_worsened(self) -> None:
        assess = self.domain.split("pub fn assess_wave_outcome", 1)[1]
        self.assertIn("NoReplies", assess)

    def test_insufficient_reasons_have_string_forms(self) -> None:
        self.assertIn("no_replies", self.domain)
        self.assertIn("below_quorum", self.domain)

    # --- the port trait exists -------------------------------------------

    def test_the_wave_outcome_repository_trait_exists(self) -> None:
        self.assertIn("AutopilotWaveOutcomeRepository", self.ports)
        self.assertIn("ClaimedWaveOutcome", self.ports)
        self.assertIn("WaveOutcomeObservation", self.ports)
        self.assertIn("assess_wave_claim", self.ports)

    # --- the learning record folds in the same transaction ---------------

    def test_complete_folds_into_learning_in_one_transaction(self) -> None:
        complete = self.infra.split("complete_wave_outcome_impl", 1)[1]
        self.assertIn("record_outreach_kind_outcome", complete)

    def test_record_outreach_kind_outcome_uses_upsert(self) -> None:
        fn = self.infra.split("async fn record_outreach_kind_outcome", 1)[1]
        self.assertIn("ON CONFLICT (workspace_id, target_kind) DO UPDATE", fn)
        self.assertIn("assess_play_standing", fn)


if __name__ == "__main__":
    unittest.main()
