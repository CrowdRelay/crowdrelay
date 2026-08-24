"""Contract tests for control-plane parametrization — tune knobs, not code.

The brain's thresholds (screening floors, lead windows, cooldowns, ramp
steps) live as typed domain policy configs. This is what makes them
operator-editable rather than frozen at compile time, and the properties
that make that safe:

- ONE parse function is the source of truth for which keys a context accepts.
  The reader, the write path and the API validator all call it — a key cannot
  be accepted on write and silently dropped on read.
- An unknown key is refused with a 400 at the boundary instead of stored and
  ignored.
- Absent config leaves the stored knobs alone; an empty object resets them
  to defaults.
"""

from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
MODEL = ROOT / "crates/crowdrelay-application/src/autopilot/model.rs"
MAPPING = ROOT / "crates/crowdrelay-infra/src/autopilot/mapping.rs"
CONTROL = ROOT / "crates/crowdrelay-infra/src/autopilot/control.rs"
COMMAND = ROOT / "crates/crowdrelay-application/src/autopilot/control.rs"
API = ROOT / "crates/crowdrelay-api/src/autopilot/authority_booking.rs"
REQUESTS = ROOT / "crates/crowdrelay-api/src/autopilot/requests.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


class PolicyParametrizationContract(unittest.TestCase):
    def test_one_parse_function_is_the_source_of_truth(self) -> None:
        model = read(MODEL)
        self.assertIn("pub fn parse_for(", model)
        # Every context has an arm in the single match.
        arms = model.split("impl AutopilotPolicyConfig", 1)[1].split("\n}\n", 1)[0].count(
            "AutopilotContext::"
        )
        self.assertGreaterEqual(arms, 21)
        # The infra reader delegates to it instead of keeping its own copy.
        mapping = read(MAPPING)
        self.assertIn("AutopilotPolicyConfig::parse_for(context, row.config)", mapping)
        self.assertNotIn("fn parse_config<T>", mapping, "the duplicated parser is gone")

    def test_unknown_keys_refused_at_the_boundary_not_stored_silently(self) -> None:
        handler = read(API)
        self.assertIn("AutopilotPolicyConfig::parse_for(context, raw.clone())", handler)
        self.assertIn("Problem::bad_request", handler.split("parse_for(context", 1)[1][:400])

    def test_absent_config_leaves_tuning_alone_and_empty_resets(self) -> None:
        sql = read(CONTROL).split("async fn set_authority", 1)[1][:3000]
        self.assertIn("config = COALESCE($8, config)", sql)
        command = read(COMMAND).split("pub struct SetAutopilotAuthority", 1)[1].split("}", 1)[0]
        self.assertIn("pub config: Option<serde_json::Value>", command)

    def test_reset_to_defaults_is_explicit_in_the_parser(self) -> None:
        model = read(MODEL).split("impl AutopilotPolicyConfig", 1)[1][:6000]
        self.assertIn("is_some_and(serde_json::Map::is_empty)", model)
        self.assertIn("wrap(default)", model)

    def test_openapi_documents_the_knobs(self) -> None:
        openapi = read(OPENAPI).split("    AutopilotAuthorityRequest:", 1)[1][:1800]
        self.assertIn("config:", openapi)
        self.assertIn("without code changes", openapi)


if __name__ == "__main__":
    unittest.main()
