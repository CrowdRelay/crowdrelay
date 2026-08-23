from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
OBS = (ROOT / "crates/crowdrelay-infra/src/observability.rs").read_text(encoding="utf-8")
API = (ROOT / "crates/crowdrelay-api/src/main.rs").read_text(encoding="utf-8")
WORKER = (ROOT / "crates/crowdrelay-worker/src/main.rs").read_text(encoding="utf-8")


class PortfolioReliabilityV14(unittest.TestCase):
    def test_both_processes_install_structured_panic_hook(self):
        self.assertIn("pub fn install_panic_hook", OBS)
        self.assertIn('observability::install_panic_hook("crowdrelay-api")', API)
        self.assertIn('observability::install_panic_hook("crowdrelay-worker")', WORKER)
        self.assertIn('"process panic"', OBS)

    def test_panic_payload_is_bounded_and_control_characters_are_removed(self):
        self.assertIn("MAX_PANIC_MESSAGE_CHARS", OBS)
        self.assertIn("character.is_control()", OBS)
        self.assertIn("panic.file", OBS)
        self.assertIn("panic.line", OBS)


if __name__ == "__main__": unittest.main()
