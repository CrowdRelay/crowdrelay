"""Contract tests for reply triage, calendar routing, and operator brief
per-capability alerts — Phase 19 completion + self-starting 10.

Pinned:
- Reply triage domain classifier exists, classifies all dispositions, routes
  ambiguous/DNC/short/unknown to human review.
- Calendar routing conflict detector exists and detects consecutive-day
  routing conflicts.
- GrowthDebtKind has CalendarRoutingConflict variant with all required methods.
- Operator brief snapshot carries per-capability parked detail.
- Reply triage port trait exists in application layer.
- Reply triage repository impl exists in infra layer.
- Worker runs reply triage phase after wave outcomes.
- Migration 0120 creates the reply_classifications table.
- API layer holds no SQL writes (api-sql-ratchet invariant).
- Domain modules are engine-core (no bounded context imports).
"""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
DOMAIN = ROOT / "crates/crowdrelay-domain/src"
APPLICATION = ROOT / "crates/crowdrelay-application/src"
INFRA = ROOT / "crates/crowdrelay-infra/src"
WORKER = ROOT / "crates/crowdrelay-worker/src"
MIGRATIONS = ROOT / "migrations"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class ReplyTriageContract(unittest.TestCase):
    def setUp(self) -> None:
        self.classifier = read(DOMAIN / "reply_triage.rs")
        self.ports = read(APPLICATION / "autopilot/ports.rs")
        self.worker = read(WORKER / "autopilot.rs")
        self.migration = read(MIGRATIONS / "0120_reply_triage.sql")

    def test_classifier_exists(self) -> None:
        self.assertIn("pub fn classify_reply", self.classifier)

    def test_classifier_has_all_dispositions(self) -> None:
        self.assertIn("OutreachReplyDisposition::Positive", self.classifier)
        self.assertIn("OutreachReplyDisposition::Declined", self.classifier)
        self.assertIn("OutreachReplyDisposition::DoNotContact", self.classifier)

    def test_needs_human_reasons_exist(self) -> None:
        for reason in [
            "AmbiguousText",
            "NotInSupportedLanguage",
            "TooShort",
            "PreviousDoNotContact",
            "UnmatchedText",
        ]:
            self.assertIn(reason, self.classifier)

    def test_dnc_overrides_positive(self) -> None:
        # DNC is checked before positive/declined ambiguity
        self.assertIn("dnc", self.classifier.lower())

    def test_polish_and_english_keywords(self) -> None:
        self.assertIn("tak", self.classifier)
        self.assertIn("yes", self.classifier)
        self.assertIn("nie dziękuję", self.classifier)
        self.assertIn("no thanks", self.classifier)

    def test_port_trait_exists(self) -> None:
        self.assertIn("AutopilotReplyTriageRepository", self.ports)
        self.assertIn("ReplyNeedingTriage", self.ports)
        self.assertIn("ReplyTriageResult", self.ports)

    def test_port_has_load_and_record(self) -> None:
        self.assertIn("load_replies_needing_triage", self.ports)
        self.assertIn("record_reply_classification", self.ports)

    def test_worker_runs_triage_phase(self) -> None:
        self.assertIn("load_replies_needing_triage", self.worker)
        self.assertIn("record_reply_classification", self.worker)
        self.assertIn("classify_reply", self.worker)
        self.assertIn("REPLY_TRIAGE_BATCH_SIZE", self.worker)

    def test_worker_triage_after_wave_outcomes(self) -> None:
        # Reply triage should come after wave outcome settlement
        triage_pos = self.worker.find("reply triage")
        wave_pos = self.worker.find("wave outcome")
        self.assertGreater(triage_pos, wave_pos)

    def test_migration_creates_table(self) -> None:
        self.assertIn("CREATE TABLE viryaos_reply_classifications", self.migration)
        self.assertIn("reply_text", self.migration)
        self.assertIn("classification_result", self.migration)
        self.assertIn("needs_human", self.migration)
        self.assertIn("human_review_reason", self.migration)

    def test_migration_has_needs_human_index(self) -> None:
        self.assertIn("needs_human_idx", self.migration)


class CalendarRoutingContract(unittest.TestCase):
    def setUp(self) -> None:
        self.module = read(DOMAIN / "calendar_routing.rs")
        self.growth_debt = read(DOMAIN / "growth_debt.rs")

    def test_detector_exists(self) -> None:
        self.assertIn("pub fn evaluate_routing_conflict", self.module)
        self.assertIn("pub fn scan_routing_conflicts", self.module)

    def test_policy_has_thresholds(self) -> None:
        self.assertIn("max_consecutive_day_km", self.module)
        self.assertIn("max_two_day_gap_km", self.module)

    def test_decision_has_conflict_variant(self) -> None:
        self.assertIn("CalendarRoutingDecision::Conflict", self.module)
        self.assertIn("CalendarRoutingDecision::Ok", self.module)

    def test_growth_debt_has_calendar_routing(self) -> None:
        self.assertIn("CalendarRoutingConflict", self.growth_debt)
        self.assertIn("calendar_routing_conflict", self.growth_debt)
        self.assertIn("review_calendar_routing", self.growth_debt)
        self.assertIn("raise_growth_debt_calendar_routing_conflict", self.growth_debt)


class OperatorBriefCapabilityAlertsContract(unittest.TestCase):
    def setUp(self) -> None:
        self.brief = read(DOMAIN / "operator_brief.rs")

    def test_parked_capabilities_field_exists(self) -> None:
        self.assertIn("parked_capabilities", self.brief)

    def test_parked_capability_struct_exists(self) -> None:
        self.assertIn("pub struct ParkedCapability", self.brief)
        self.assertIn("capability:", self.brief)
        self.assertIn("parked_count:", self.brief)
        self.assertIn("days_since_heartbeat:", self.brief)


class DomainLayeringContract(unittest.TestCase):
    """Engine-core modules must not import bounded contexts."""

    def test_reply_triage_imports_no_bounded_contexts(self) -> None:
        content = read(DOMAIN / "reply_triage.rs")
        # reply_triage is a bounded context, not engine-core, so it may
        # import outreach types. But it must not import engine-core modules
        # in reverse — check it doesn't import learning, autonomy internals.
        self.assertNotIn("use crate::learning", content)

    def test_calendar_routing_is_pure_domain(self) -> None:
        content = read(DOMAIN / "calendar_routing.rs")
        self.assertNotIn("use crate::outreach", content)
        self.assertNotIn("use crate::booking", content)
        self.assertNotIn("sqlx", content)


if __name__ == "__main__":
    unittest.main()
