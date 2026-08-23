"""Contract tests for the cross-context Next Best Action queue.

The queue is the one surface an operator reads before deciding what to do, so
the properties worth pinning are the ones whose failure is invisible: an
ordering that quietly stops being explainable, a response that quietly stops
being capped, a denied decision quietly appearing as work to do, and an
"expected impact" quietly turning into an invented number.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
DOMAIN = ROOT / "crates/crowdrelay-domain/src/next_best_action.rs"
LOADER = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/next_best_action.rs"
CONTROL = ROOT / "crates/crowdrelay-application/src/autopilot/control.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class NextBestActionContract(unittest.TestCase):
    def setUp(self) -> None:
        self.domain = read(DOMAIN)
        self.loader = read(LOADER)

    def test_the_queue_stores_nothing(self) -> None:
        # A denormalized queue table starts disagreeing with its own evidence
        # the moment an action succeeds.
        for forbidden in ("INSERT ", "UPDATE ", "DELETE ", "CREATE TABLE"):
            self.assertNotIn(forbidden, self.loader)

    def test_the_ranking_order_matches_the_documented_order(self) -> None:
        factors = re.findall(
            r"RankFactor::(\w+),",
            self.domain.split("const FACTORS: [RankFactor; 6] = [", 1)[1].split("];", 1)[0],
        )
        self.assertEqual(
            factors,
            [
                "Authority",
                "Deadline",
                "ValueTier",
                "MeasuredEffect",
                "Confidence",
                "Magnitude",
            ],
        )

    def test_the_ranking_is_lexicographic_not_a_weighted_score(self) -> None:
        # A weighted sum lets a good past record buy its way past a deadline,
        # and no operator can tell why anything landed where it did.
        self.assertIn("rank_key", self.domain)
        self.assertIn(
            "a_measured_record_never_outranks_a_deadline_or_a_value_tier", self.domain
        )
        # The key is compared component by component; there is no place a
        # weight could be introduced without changing this shape.
        self.assertIn("-> [u32; 6]", self.domain)
        self.assertIn("separating_factor", self.domain)

    def test_the_response_is_capped_in_the_domain_and_in_the_contract(self) -> None:
        self.assertIn("MAX_QUEUE_ENTRIES: usize = 10", self.domain)
        self.assertIn("candidates.truncate(MAX_QUEUE_ENTRIES)", self.domain)
        openapi = read(OPENAPI)
        queue = openapi.split("/admin/autopilot/next-best-actions:", 1)[1].split(
            "  /admin/autopilot/booking-targets", 1
        )[0]
        self.assertIn("maxItems: 10", queue)

    def test_the_candidate_window_is_bounded(self) -> None:
        self.assertIn("MAX_QUEUE_CANDIDATES", self.loader)
        self.assertIn("LIMIT $3", self.loader)

    def test_a_denied_decision_never_appears_as_work_to_do(self) -> None:
        self.assertIn("decision.disposition <> 'deny'", self.loader)
        self.assertIn("a_denied_decision_never_enters_the_queue", self.domain)

    def test_finished_work_is_not_next(self) -> None:
        self.assertIn("done.status IN ('succeeded', 'cancelled')", self.loader)

    def test_a_refreshed_finding_appears_once_not_once_per_cycle(self) -> None:
        # Every evaluation cycle writes a new decision row when the evidence
        # changes. Without this the queue fills with the same finding.
        newer = self.loader.split("AS newer", 1)[1]
        self.assertIn("newer.decision_kind = decision.decision_kind", newer)
        self.assertIn("(newer.evaluated_at, newer.id) > (decision.evaluated_at", newer)

    def test_expected_impact_is_never_a_currency_amount(self) -> None:
        # Checked against declarations rather than prose: the doc comments
        # deliberately talk about currency in order to forbid it.
        # The unit tests below assert on these very strings, so only the
        # shipped declarations are scanned.
        code = "\n".join(
            line
            for line in self.domain.split("#[cfg(test)]", 1)[0].splitlines()
            if not line.lstrip().startswith(("//", "///", "//!"))
        )
        for forbidden in ("_minor", "currency", "PLN", "EUR", "revenue"):
            self.assertNotIn(forbidden, code)
        fields = [
            line.strip()
            for line in read(CONTROL)
            .split("pub struct NextBestAction {", 1)[1]
            .split("}", 1)[0]
            .splitlines()
            if line.strip().startswith("pub ")
        ]
        self.assertIn("pub deviation_basis_points: Option<u32>,", fields)
        for field in fields:
            for forbidden in ("_minor", "amount", "currency"):
                self.assertNotIn(forbidden, field)

    def test_a_deadline_is_only_ever_a_real_date(self) -> None:
        # Every branch reads a date that already exists on the subject. A
        # fallback would invent urgency the business never declared.
        deadline = self.loader.split("AS deadline ON true", 1)[0].split("SELECT min(due_at)", 1)[1]
        self.assertIn("event.starts_at", deadline)
        self.assertIn("plan.release_at", deadline)
        self.assertIn("opportunity.deadline", deadline)
        self.assertIn("action.approval_expires_at", deadline)
        self.assertNotIn("INTERVAL", deadline)
        self.assertIn("an_expired_deadline_does_not_win_the_queue", self.domain)

    def test_an_unreadable_payload_reports_no_evidence_rather_than_zero(self) -> None:
        signals = self.loader.split("fn payload_signals", 1)[1].split("\npub", 1)[0]
        self.assertIn("return (None, None, None)", signals)
        self.assertNotIn("unwrap_or(0)", signals)

    def test_every_entry_explains_itself(self) -> None:
        entry = read(CONTROL).split("pub struct NextBestAction {", 1)[1].split("}", 1)[0]
        for required in ("ranked_by", "consequence", "reason", "recommended_action"):
            self.assertIn(required, entry)
        self.assertIn("every_entry_states_what_happens_if_it_is_ignored", self.domain)

    def test_the_endpoint_is_admin_only_and_documented(self) -> None:
        self.assertIn(
            '"/v1/admin/autopilot/next-best-actions"',
            read(ROUTING),
        )
        openapi = read(OPENAPI)
        queue = openapi.split("/admin/autopilot/next-best-actions:", 1)[1].split(
            "  /admin/autopilot/booking-targets", 1
        )[0]
        self.assertIn("adminBearer", queue)
        self.assertIn("PrivateNoStore", queue)
        self.assertIn("NextBestAction", queue)

    def test_the_ordering_the_operator_sees_is_the_one_under_test(self) -> None:
        # A second ORDER BY in SQL that means anything would drift from the
        # ranked order without a single test failing.
        sql = self.loader.split('r#"', 1)[1].split('"#', 1)[0]
        self.assertEqual(sql.count("ORDER BY"), 2)
        self.assertIn("ORDER BY decision.evaluated_at DESC", sql)
        self.assertIn("rank_next_best_actions", self.loader)


if __name__ == "__main__":
    unittest.main()
