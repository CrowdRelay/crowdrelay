from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

class ComposeHealthContractTests(unittest.TestCase):
    def test_worker_health_checks_real_worker_process(self):
        for name in ("docker-compose.yml", "compose.production.yaml"):
            text = (ROOT / name).read_text()
            self.assertRegex(text, r"/(?:app|usr/local/bin)/crowdrelay-worker", name)
            self.assertNotIn('kill -0 1', text, name)

if __name__ == "__main__":
    unittest.main()
