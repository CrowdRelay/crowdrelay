"""Contract tests for objectives — phase 6.

Every context before this reacts. None could say whether the work added up to
anything, because nothing declared what "anything" would be.

The properties pinned here are the ones whose absence would turn a target into
self-congratulation:

- an objective is never evidence of progress: its state comes from the series
  and from nothing else;
- the baseline is frozen when the target is declared;
- progress is derived on read and never stored, so it cannot go stale silently;
- a missed deadline is reported as missed, and a met target stays met;
- a projection refuses rather than guesses;
- a target ranks below a real deadline in the queue;
- a retired target is kept, not deleted.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
MIGRATION = ROOT / "migrations/0092_viryaos_growth_objectives.sql"
DOMAIN = ROOT / "crates/crowdrelay-domain/src/objectives.rs"
QUEUE_DOMAIN = ROOT / "crates/crowdrelay-domain/src/next_best_action.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/control/objective_ports.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/objectives.rs"
QUEUE_INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/next_best_action.rs"
API = ROOT / "crates/crowdrelay-api/src/autopilot/objectives.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def strip_sql_comments(sql: str) -> str:
    return "\n".join(line.split("--", 1)[0] for line in sql.splitlines())


class GrowthObjectivesContract(unittest.TestCase):
    def setUp(self) -> None:
        self.sql = strip_sql_comments(read(MIGRATION))
        self.domain = read(DOMAIN)
        self.infra = read(INFRA)

    def gaps(self) -> set[str]:
        block = self.domain.split("impl ObjectiveGap", 1)[1].split("\n    }", 1)[0]
        return set(re.findall(r'Self::\w+ => "([a-z_]+)"', block))

    def states(self) -> set[str]:
        block = self.domain.split("impl ObjectiveState", 1)[1].split("\n    }", 1)[0]
        return set(re.findall(r'Self::\w+ \{ \.\. \} => "([a-z_]+)"', block))

    # --- what an objective may claim -------------------------------------

    def test_state_comes_from_the_series_and_nothing_else(self) -> None:
        # Actions taken toward a target move the number zero. The rule takes an
        # observation and a policy; it cannot see an action if it wanted to.
        signature = self.domain.split("pub fn assess_objective", 1)[1].split(") -> ", 1)[0]
        self.assertIn("observed: Option<(i64, OffsetDateTime)>", signature)
        # And it imports nothing that could tell it what the agent did: the
        # rule can see a series and a policy, and there is no third thing.
        imports = self.domain.split("use crate::", 1)[1].split(";", 1)[0]
        self.assertNotIn("action", imports)
        self.assertNotIn("plays", imports)
        self.assertNotIn("autonomy", imports)

    def test_progress_is_derived_on_read_and_never_stored(self) -> None:
        # A stored "on track" goes stale silently; a derived one cannot.
        for column in ("state", "progress_basis_points", "projected_value"):
            self.assertNotIn(f"{column} text", self.sql)
            self.assertNotIn(f"{column} integer", self.sql)
        self.assertNotIn("UPDATE viryaos_growth_objectives\n                SET state", self.infra)
        loader = self.infra.split("async fn load_growth_objectives", 1)[1]
        self.assertIn("assess_objective(", loader)

    def test_the_baseline_is_frozen_when_the_target_is_declared(self) -> None:
        self.assertIn("baseline_value bigint NOT NULL", self.sql)
        declare = self.infra.split("async fn declare_growth_objective", 1)[1]
        frozen = declare.index("latest_series_value")
        inserted = declare.index("INSERT INTO viryaos_growth_objectives")
        self.assertLess(frozen, inserted)
        self.assertIn(
            "Progress measured from a baseline that\n-- moves is not progress",
            read(MIGRATION),
        )

    def test_a_series_with_no_observation_does_not_count_from_zero(self) -> None:
        """Declaration is refused, not defaulted.

        This used to check only that the *response* reported an absent
        baseline, while the row still stored `baseline.unwrap_or(0)`. Anything
        reading the table afterwards saw a real-looking zero, which is what
        happened in production: the Spotify followers objective was declared on
        2026-08-24, that series' first point is 2026-08-31, and the stored
        baseline of 0 made a channel sitting flat at 183 followers report 73%
        progress toward 250.

        Refusing is the only version of this that holds, because the column is
        `NOT NULL` and every reader of the row is entitled to trust it.
        """
        declare = self.infra.split("async fn declare_growth_objective", 1)[1]
        insert = declare.index("INSERT INTO viryaos_growth_objectives")
        refusal = declare.index("RepositoryError::ConflictBecause")
        self.assertLess(
            refusal,
            insert,
            "an unmeasured series must be refused before the insert, not "
            "defaulted into it",
        )
        self.assertNotIn(
            "baseline.unwrap_or(0)",
            declare,
            "the zero fallback is back; it writes a baseline nobody observed "
            "into a NOT NULL column that every later percentage inherits",
        )
        self.assertIn("NoObservation", self.domain)

    def test_a_missed_deadline_is_reported_and_a_met_target_stays_met(self) -> None:
        rule = self.domain.split("pub fn assess_objective", 1)[1]
        met = rule.index("ObjectiveState::Met")
        missed = rule.index("ObjectiveState::Missed")
        self.assertLess(met, missed, "reaching the target wins over the clock")
        self.assertIn("a_met_objective_is_not_unmet_by_a_later_fall", self.domain)
        self.assertIn("a_deadline_that_passed_unmet_is_missed_and_stays_missed", self.domain)
        # Neither is active, so history stops promoting work up the queue.
        active = self.domain.split("pub const fn is_active", 1)[1].split("\n    }", 1)[0]
        self.assertIn("Self::OnTrack { .. } | Self::Behind { .. }", active)

    def test_a_projection_refuses_rather_than_guesses(self) -> None:
        self.assertIn("minimum_elapsed_hours", self.domain)
        self.assertIn("TooEarlyToProject", self.domain)
        self.assertIn("a_projection_from_a_few_hours_is_refused", self.domain)
        self.assertIn("going_backwards_is_zero_progress_and_never_a_negative_percentage", self.domain)

    def test_every_gap_and_state_is_published(self) -> None:
        openapi = read(OPENAPI)
        published_gaps = re.search(
            r"ObjectiveGap:.*?enum: \[(.*?)\]", openapi, re.DOTALL
        )
        self.assertIsNotNone(published_gaps)
        self.assertEqual(
            {value.strip() for value in published_gaps.group(1).split(",")}, self.gaps()
        )
        published_states = re.search(
            r"ObjectiveStateName:.*?enum: \[(.*?)\]", openapi, re.DOTALL
        )
        self.assertIsNotNone(published_states)
        self.assertEqual(
            {value.strip() for value in published_states.group(1).split(",")},
            self.states(),
        )

    # --- the database ----------------------------------------------------

    def test_one_live_target_per_series_per_scope(self) -> None:
        # Two would let a report pick the friendlier one.
        self.assertIn(
            "UNIQUE NULLS NOT DISTINCT (workspace_id, platform, metric_key, scope_kind, scope_id)",
            self.sql,
        )
        self.assertIn("ON CONFLICT (workspace_id, platform, metric_key, scope_kind, scope_id)", self.infra)

    def test_a_scope_is_consistent_with_its_subject(self) -> None:
        self.assertIn(
            "CHECK ((scope_kind = 'workspace') = (scope_id IS NULL))", self.sql
        )

    def test_a_retired_target_is_kept_not_deleted(self) -> None:
        self.assertIn("retired_at timestamptz", self.sql)
        self.assertNotIn("DELETE FROM viryaos_growth_objectives", self.infra)
        retire = self.infra.split("async fn retire_growth_objective", 1)[1]
        self.assertIn("SET retired_at = now()", retire)

    def test_a_target_must_be_owned_and_dated(self) -> None:
        self.assertIn("declared_by text NOT NULL", self.sql)
        self.assertIn("CHECK (deadline > declared_at)", self.sql)
        self.assertIn("MAX_OBJECTIVE_HORIZON_DAYS", read(API))

    # --- where it bites ---------------------------------------------------

    def test_a_target_ranks_below_a_real_deadline(self) -> None:
        queue = read(QUEUE_DOMAIN)
        factors = queue.split("const FACTORS: [RankFactor; 7] = [", 1)[1].split("];", 1)[0]
        order = re.findall(r"RankFactor::(\w+),", factors)
        self.assertLess(order.index("Deadline"), order.index("Objective"))
        self.assertLess(order.index("Objective"), order.index("ValueTier"))
        self.assertIn("a_real_deadline_still_beats_a_declared_target", queue)

    def test_only_a_finding_that_names_the_series_contributes(self) -> None:
        # Guessing which series an arbitrary finding moves would let an
        # unrelated one ride an objective up the queue.
        builder = read(QUEUE_INFRA)
        self.assertIn("fn payload_series", builder)
        series = builder.split("fn payload_series", 1)[1]
        self.assertIn('"raise_growth_opportunity"', series)
        self.assertIn('"run_play_step"', series)
        self.assertIn("_ => None", series)
        # A passed deadline stops promoting work.
        self.assertIn("AND deadline > $2", builder)

    def test_the_surface_is_published(self) -> None:
        routing = read(ROUTING)
        self.assertIn('"/v1/admin/autopilot/objectives"', routing)
        self.assertIn('"/v1/admin/autopilot/objectives/{objective_id}/retire"', routing)
        openapi = read(OPENAPI)
        self.assertIn("/admin/autopilot/objectives:", openapi)
        self.assertIn("GrowthObjectivesResponse", openapi)


if __name__ == "__main__":
    unittest.main()
