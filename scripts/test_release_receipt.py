"""Tests for the durable release receipt writer."""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "release_receipt.py"


class ReleaseReceiptTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.mkdtemp(prefix="receipt-test-")
        self.state_dir = Path(self.tmpdir) / "releases"

    def tearDown(self) -> None:
        import shutil
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _run(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            capture_output=True, text=True, check=False,
        )

    def test_init_creates_directories(self) -> None:
        r = self._run("init", "--state-dir", str(self.state_dir), "--service", "crowdrelay")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue((self.state_dir / "receipts").is_dir())
        self.assertTrue((self.state_dir / "configs").is_dir())

    def test_full_lifecycle(self) -> None:
        """pending -> phase -> finalize produces current.json and a receipt."""
        release_id = "test-001"
        self._run("init", "--state-dir", str(self.state_dir), "--service", "crowdrelay")

        # Write a temp compose file to get a config digest
        compose = Path(self.tmpdir) / "compose.yaml"
        compose.write_text("services:\n  api:\n    image: test\n")

        r = self._run(
            "pending", "--state-dir", str(self.state_dir),
            "--service", "crowdrelay",
            "--release-id", release_id,
            "--source-sha", "a" * 40,
            "--image-digests", "api=sha256:" + "b" * 64, "worker=sha256:" + "c" * 64,
            "--oci-revision", "a" * 40,
            "--oci-architecture", "amd64",
            "--deploy-color", "green",
            "--current-color", "blue",
            "--current-container", "crowdrelay-api-1",
            "--candidate-container", "crowdrelay-api-green-1",
            "--compose-file", str(compose),
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("RECEIPT_PENDING=PASS", r.stdout)

        pending = json.loads((self.state_dir / "pending.json").read_text())
        self.assertEqual(pending["sourceSha"], "a" * 40)
        self.assertEqual(pending["deployColor"], "green")
        self.assertIn("compose", pending["configDigests"])
        self.assertTrue(pending["configDigests"]["compose"].startswith("sha256:"))

        # Record a phase
        r = self._run(
            "phase", "--state-dir", str(self.state_dir),
            "--release-id", release_id,
            "--phase", "cutover", "--status", "pass",
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        pending = json.loads((self.state_dir / "pending.json").read_text())
        self.assertIn("cutover", pending["phases"])
        self.assertEqual(pending["phases"]["cutover"]["status"], "pass")

        # Finalize
        r = self._run(
            "finalize", "--state-dir", str(self.state_dir),
            "--release-id", release_id, "--status", "pass",
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("RECEIPT_FINALIZE=PASS", r.stdout)

        # pending.json should be gone
        self.assertFalse((self.state_dir / "pending.json").exists())
        # current.json should exist
        current = json.loads((self.state_dir / "current.json").read_text())
        self.assertEqual(current["releaseId"], release_id)
        self.assertIn("finalized", current["phases"])
        # Receipt should exist
        receipt = json.loads((self.state_dir / "receipts" / f"{release_id}.json").read_text())
        self.assertEqual(receipt["releaseId"], release_id)

    def test_rollback_keeps_current_intact(self) -> None:
        """Rollback removes pending but keeps current (last good release) intact."""
        self._run("init", "--state-dir", str(self.state_dir), "--service", "crowdrelay")

        # First release (successful)
        self._run(
            "pending", "--state-dir", str(self.state_dir),
            "--service", "crowdrelay",
            "--release-id", "rel-1",
            "--source-sha", "1" * 40,
        )
        self._run("finalize", "--state-dir", str(self.state_dir), "--release-id", "rel-1")

        # Second release (successful — creates previous.json with rel-1)
        self._run(
            "pending", "--state-dir", str(self.state_dir),
            "--service", "crowdrelay",
            "--release-id", "rel-2",
            "--source-sha", "2" * 40,
        )
        self._run("finalize", "--state-dir", str(self.state_dir), "--release-id", "rel-2")

        # Third release (will fail — only in pending, never promoted to current)
        self._run(
            "pending", "--state-dir", str(self.state_dir),
            "--service", "crowdrelay",
            "--release-id", "rel-3",
            "--source-sha", "3" * 40,
        )
        r = self._run(
            "rollback", "--state-dir", str(self.state_dir),
            "--release-id", "rel-3", "--reason", "soak-failure",
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("current_intact=true", r.stdout)

        # current.json should still be rel-2 (the last successful release)
        current = json.loads((self.state_dir / "current.json").read_text())
        self.assertEqual(current["releaseId"], "rel-2")
        # previous.json should still be rel-1
        previous = json.loads((self.state_dir / "previous.json").read_text())
        self.assertEqual(previous["releaseId"], "rel-1")
        # pending.json should be gone
        self.assertFalse((self.state_dir / "pending.json").exists())
        # Failure receipt should exist
        receipt = json.loads((self.state_dir / "receipts" / "rel-3.json").read_text())
        self.assertEqual(receipt["rollbackResult"]["reason"], "soak-failure")

    def test_rollback_without_previous_keeps_current(self) -> None:
        """Rollback after only one release keeps current.json as the last good release."""
        self._run("init", "--state-dir", str(self.state_dir), "--service", "crowdrelay")
        self._run(
            "pending", "--state-dir", str(self.state_dir),
            "--service", "crowdrelay",
            "--release-id", "rel-1",
            "--source-sha", "1" * 40,
        )
        self._run("finalize", "--state-dir", str(self.state_dir), "--release-id", "rel-1")
        self._run(
            "pending", "--state-dir", str(self.state_dir),
            "--service", "crowdrelay",
            "--release-id", "rel-2",
            "--source-sha", "2" * 40,
        )
        r = self._run(
            "rollback", "--state-dir", str(self.state_dir),
            "--release-id", "rel-2", "--reason", "health-failure",
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("current_intact=true", r.stdout)
        current = json.loads((self.state_dir / "current.json").read_text())
        self.assertEqual(current["releaseId"], "rel-1")
        self.assertFalse((self.state_dir / "pending.json").exists())

    def test_no_secrets_in_receipt(self) -> None:
        """Receipt must not contain env file contents, only a digest."""
        self._run("init", "--state-dir", str(self.state_dir), "--service", "crowdrelay")
        env = Path(self.tmpdir) / ".env"
        env.write_text("SECRET_TOKEN=supersecret123\nDB_PASSWORD=hunter2\n")

        self._run(
            "pending", "--state-dir", str(self.state_dir),
            "--service", "crowdrelay",
            "--release-id", "test-secret",
            "--source-sha", "a" * 40,
            "--env-file", str(env),
        )
        pending = json.loads((self.state_dir / "pending.json").read_text())
        receipt_text = json.dumps(pending)
        self.assertNotIn("supersecret123", receipt_text)
        self.assertNotIn("hunter2", receipt_text)
        self.assertIn("env", pending["configDigests"])

    def test_show_current(self) -> None:
        self._run("init", "--state-dir", str(self.state_dir), "--service", "crowdrelay")
        self._run(
            "pending", "--state-dir", str(self.state_dir),
            "--service", "crowdrelay",
            "--release-id", "show-1",
            "--source-sha", "a" * 40,
        )
        self._run("finalize", "--state-dir", str(self.state_dir), "--release-id", "show-1")
        r = self._run("show", "--state-dir", str(self.state_dir), "current")
        self.assertEqual(r.returncode, 0, r.stderr)
        data = json.loads(r.stdout)
        self.assertEqual(data["releaseId"], "show-1")


if __name__ == "__main__":
    unittest.main()
