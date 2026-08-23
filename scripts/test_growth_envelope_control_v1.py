"""Contract tests for the operator control over the growth envelope.

The envelope had no mutation path at all until the agent was already live. It
was written by migration 0076 and changeable only by hand in psql, which meant
the one control an operator reaches for in a hurry — stop the agent — was the
one with no button. That is the wrong shape for a safety mechanism, and it was
found the hard way: flipping the agent on in production required a raw UPDATE.

The properties pinned here are the ones whose absence would put it back:

- the kill switch is reachable over the admin API at all;
- the write is whole-envelope, so no field can be silently left at its old value
  while an operator believes they restated the limits;
- it is guarded by `expected_version`, so two operators editing at once is a
  refusal rather than a last-writer-wins;
- stopping the agent does not depend on anything the agent runs.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
CONTROL = ROOT / "crates/crowdrelay-application/src/autopilot/control.rs"
PORTS = ROOT / "crates/crowdrelay-application/src/autopilot/control/runtime_ports.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/autopilot/control.rs"
HANDLER = ROOT / "crates/crowdrelay-api/src/autopilot/authority_booking.rs"
REQUESTS = ROOT / "crates/crowdrelay-api/src/autopilot/requests.rs"
ROUTING = ROOT / "crates/crowdrelay-api/src/routing.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"
MIGRATION = ROOT / "migrations/0076_viryaos_growth_envelope.sql"

ROUTE = "/v1/admin/autopilot/growth-envelope"

# Every field of the envelope. The write must carry all of them.
FIELDS = (
    "agent_enabled",
    "dry_run",
    "weekly_owned_audience_touches",
    "weekly_third_party_touches",
    "subject_cooldown_hours",
    "max_recipients_per_step",
)


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def repository_body() -> str:
    """The whole `set_growth_envelope` impl, not a fixed-size window."""
    return read(INFRA).split("async fn set_growth_envelope", 1)[1].split(
        "\n    async fn ", 1
    )[0]


class GrowthEnvelopeControlContract(unittest.TestCase):
    def test_the_kill_switch_is_reachable_over_the_admin_api(self) -> None:
        self.assertIn(ROUTE, read(ROUTING))
        self.assertIn("set_growth_envelope", read(ROUTING))
        self.assertIn("pub async fn set_growth_envelope", read(HANDLER))
        self.assertIn("async fn set_growth_envelope", read(PORTS))
        self.assertIn("async fn set_growth_envelope", read(INFRA))

    def test_every_envelope_field_is_carried_end_to_end(self) -> None:
        # A partial update is how one ceiling gets widened while another is
        # believed tightened, so each layer must name all of them.
        for name, text in (
            ("command", read(CONTROL).split("pub struct SetGrowthEnvelope", 1)[1][:600]),
            (
                "request",
                read(REQUESTS).split("pub struct GrowthEnvelopeRequest", 1)[1][:600],
            ),
            (
                "handler",
                read(HANDLER).split("pub async fn set_growth_envelope", 1)[1][:2500],
            ),
            ("repository", repository_body()),
        ):
            for field in FIELDS:
                self.assertIn(field, text, f"{name} dropped {field}")

    def test_the_request_refuses_unknown_fields(self) -> None:
        block = read(REQUESTS).split("pub struct GrowthEnvelopeRequest", 1)[0]
        self.assertTrue(
            block.rstrip().endswith("]"),
            "the struct must be preceded by its derive and serde attributes",
        )
        preamble = block[-200:]
        self.assertIn("deny_unknown_fields", preamble)

    def test_the_write_is_guarded_by_expected_version(self) -> None:
        repository = repository_body()
        self.assertIn("version = version + 1", repository)
        self.assertIn("AND version = $8", repository)
        # A missing envelope row and a lost version race are different problems
        # and must not both read as a conflict.
        self.assertIn("RepositoryError::NotFound", repository)
        self.assertIn("RepositoryError::Conflict", repository)

    def test_the_handler_refuses_what_the_table_would_refuse(self) -> None:
        handler = read(HANDLER).split("pub async fn set_growth_envelope", 1)[1][:2500]
        migration = read(MIGRATION)
        # The bounds are asserted against the migration so the two cannot drift.
        for bound in ("100000", "1000", "8760"):
            self.assertIn(bound, migration.replace("_", ""))
        self.assertIn("expected_version <= 0", handler)
        self.assertIn("100_000", handler)
        self.assertIn("8_760", handler)

    def test_stopping_the_agent_does_not_depend_on_the_agent(self) -> None:
        # No outbox, no queued action, no worker in the path: a kill switch that
        # needs the thing it is killing is not one.
        repository = repository_body()
        for forbidden in ("outbox_events", "viryaos_autopilot_actions", "emit_external"):
            self.assertNotIn(forbidden, repository)

    def test_the_change_is_audited_like_every_other_operator_mutation(self) -> None:
        repository = repository_body()
        self.assertIn("insert_operator_action", repository)
        self.assertIn('"set_growth_envelope"', repository)
        self.assertIn("replayed: true", repository)

    def test_the_endpoint_is_published(self) -> None:
        openapi = read(OPENAPI)
        self.assertIn("/admin/autopilot/growth-envelope:", openapi)
        self.assertIn("setGrowthEnvelope", openapi)
        schema = openapi.split("GrowthEnvelopeRequest:", 1)[1].split(
            "AutopilotAuthorityRequest:", 1
        )[0]
        required = re.search(r"required: \[([^\]]*)\]", schema)
        self.assertIsNotNone(required)
        published = {name.strip() for name in required.group(1).split(",")}
        self.assertEqual(
            published,
            set(FIELDS) | {"expected_version"},
            "the published contract drifted from the envelope",
        )


if __name__ == "__main__":
    unittest.main()
