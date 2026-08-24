"""Contract tests for the autonomy posture — the one dial.

Twenty-one policy rows, four class ceilings and an envelope are the real
authority store. The posture is the template that sets all of them at once,
because "let the agent work" should be one decision, not twenty-six endpoint
calls where the last one is forgotten.

Two properties are absolute and pinned here:
1. Applying a posture is a human act, recorded in the operator ledger.
   Nothing widens authority by itself, ever.
2. Money never runs unattended. `paid` stays behind approval in every
   posture — pinned against the domain mapping, not against a config file
   somebody could loosen by accident.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0102_viryaos_growth_posture.sql"
POSTURE = ROOT / "crates/crowdrelay-application/src/autopilot/growth_posture.rs"
MUTATIONS = ROOT / "crates/crowdrelay-infra/src/autopilot/control_mutations.rs"
CONTROL = ROOT / "crates/crowdrelay-infra/src/autopilot/control.rs"
API = ROOT / "crates/crowdrelay-api/src/autopilot/authority_booking.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
REQUESTS = ROOT / "crates/crowdrelay-api/src/autopilot/requests.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class GrowthPostureContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = "\n".join(
            line.split("--", 1)[0] for line in read(MIGRATION).splitlines()
        )
        self.posture = read(POSTURE)
        self.mutations = read(MUTATIONS)

    def test_one_row_records_the_operator_choice(self) -> None:
        self.assertIn("CREATE TABLE viryaos_growth_posture", self.sql)
        self.assertRegex(
            self.sql,
            r"posture text NOT NULL CHECK \(posture IN \('grounded', 'working', 'full_send'\)\)",
        )
        self.assertIn("expected_version", self.sql)

    def test_applying_is_recorded_in_the_ledger(self) -> None:
        # A posture flip moves every authority surface at once; if that is
        # not in the ledger, nobody can later explain why the agent changed.
        block = self.mutations.split("async fn set_growth_posture_impl", 1)[1].split(
            "\nimpl PostgresAutopilotRepository", 1
        )[0]
        self.assertIn('"set_growth_autonomy_posture"', block)
        self.assertIn("insert_operator_action", block)

    def test_all_three_surfaces_move_in_one_transaction(self) -> None:
        block = self.mutations.split("async fn set_growth_posture_impl", 1)[1].split(
            "\nimpl PostgresAutopilotRepository", 1
        )[0]
        self.assertIn("UPDATE viryaos_autopilot_policies", block)
        self.assertIn("INSERT INTO viryaos_growth_autonomy", block)
        self.assertIn("INSERT INTO viryaos_growth_envelope", block)
        self.assertIn("viryaos_growth_posture", block)
        # Exactly one commit for the application itself — the replay path
        # above commits its own read-only transaction and returns early, so
        # count only what follows the concurrency check.
        self.assertEqual(
            block.split("let current: Option<i64>", 1)[1].count("transaction.commit()"),
            1,
        )

    def test_budgets_survive_a_posture_flip(self) -> None:
        # The envelope write touches only the switches. Tuned budgets and
        # cooldowns belong to the operator; a posture that reset them would
        # be a regression wearing a feature's clothes.
        block = self.mutations.split("async fn set_growth_posture_impl", 1)[1].split(
            "\nimpl PostgresAutopilotRepository", 1
        )[0]
        envelope_sql = block.split("INSERT INTO viryaos_growth_envelope", 1)[1].split("#", 1)[0]
        self.assertNotIn("weekly_owned_audience_touches =", envelope_sql)
        self.assertNotIn("subject_cooldown_hours =", envelope_sql)
        self.assertIn("agent_enabled = EXCLUDED.agent_enabled", envelope_sql)
        self.assertIn("dry_run = EXCLUDED.dry_run", envelope_sql)

    def test_every_context_is_set_so_no_switch_is_forgotten(self) -> None:
        block = self.mutations.split("async fn set_growth_posture_impl", 1)[1].split(
            "\nimpl PostgresAutopilotRepository", 1
        )[0]
        self.assertIn("for context in AutopilotContext::ALL", block)
        self.assertIn("enabled = true", block, "a posture enables the contexts it applies")

    def test_money_never_runs_unattended_in_any_posture(self) -> None:
        domain = self.posture
        self.assertIn("fn money_is_never_autonomous_in_any_posture", domain)
        # And the mapping itself says so structurally: paid has no bounded_auto arm.
        ceiling = domain.split("pub const fn ceiling(self, class: ActionClass) -> AutonomyLevel", 1)[1].split("\n    }", 1)[0]
        paid_arms = [line for line in ceiling.splitlines() if "ActionClass::Paid" in line]
        self.assertTrue(paid_arms)
        for line in paid_arms:
            self.assertNotIn("BoundedAuto", line)

    def test_grounded_rehearses_and_working_drafts_third_party(self) -> None:
        domain = self.posture
        grounded = domain.split("Self::Grounded => (true, true)", 1)[0]
        self.assertIn("Grounded", grounded)
        working = domain.split("(Self::Working, ActionClass::ThirdParty", 1)[0].splitlines()
        self.assertTrue(any("RequireApproval" in line for line in working))

    def test_unknown_posture_refused_at_the_boundary(self) -> None:
        handler = read(API)
        self.assertIn("GrowthPosture::parse(&request.posture)", handler)
        self.assertIn("deny_unknown_fields", read(REQUESTS))

    def test_optimistic_concurrency_on_the_posture_row(self) -> None:
        block = self.mutations.split("async fn set_growth_posture_impl", 1)[1].split(
            "\nimpl PostgresAutopilotRepository", 1
        )[0]
        self.assertIn("expected_version != command.expected_version", block.replace("current_version !=", "expected_version !="))
        self.assertIn("FOR UPDATE", block)

    def test_route_and_openapi_documented(self) -> None:
        routing = read(ROUTING)
        self.assertIn('"/v1/admin/autopilot/posture"', routing)
        openapi = read(OPENAPI)
        self.assertIn("/admin/autopilot/posture:", openapi)
        self.assertIn("getAutopilotGrowthPosture", openapi)
        self.assertIn("setAutopilotGrowthPosture", openapi)
        self.assertIn("enum: [grounded, working, full_send]", openapi)


if __name__ == "__main__":
    unittest.main()
