from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
COMPOSE = (ROOT / "compose.production.yaml").read_text()


class ReleaseRecreateContract(unittest.TestCase):
    def test_api_and_worker_config_change_with_exact_release_sha(self) -> None:
        marker = "io.crowdrelay.release.ref: ${CROWDRELAY_IMAGE_TAG:?Set CROWDRELAY_IMAGE_TAG to sha-<full commit SHA>}"
        self.assertEqual(COMPOSE.count(marker), 2)
        self.assertIn("api:\n", COMPOSE)
        self.assertIn("worker:\n", COMPOSE)


if __name__ == "__main__":
    unittest.main()
