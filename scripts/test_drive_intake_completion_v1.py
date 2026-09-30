"""Keep failed registry writes and partial Drive scans out of clean markers."""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
WORKER = ROOT / "crates/crowdrelay-worker/src"


class IntakeCompletionTests(unittest.TestCase):
    def test_every_sheet_write_failure_blocks_each_transport_marker(self):
        sheet = (WORKER / "sheet_intake.rs").read_text()
        harvest = sheet.split("pub struct SheetHarvest {", 1)[1].split("\n}", 1)[0]
        failures = set(re.findall(r"pub (\w+_failed): u64", harvest))
        self.assertTrue(failures, "the gate must inspect actual harvest counters")
        for name in ("gdrive_contacts_sync.rs", "github_registry_sync.rs"):
            with self.subTest(transport=name):
                source = (WORKER / name).read_text()
                match = re.search(r"let row_failures = (harvest\.[\s\S]*?);", source)
                self.assertIsNotNone(match)
                counted = set(re.findall(r"harvest\.(\w+_failed)", match.group(1)))
                self.assertEqual(failures, counted)
                self.assertIn("if row_failures == 0", source)

    def test_partial_scan_is_checked_before_marking_connection_clean(self):
        source = (WORKER / "gdrive_contacts_sync.rs").read_text()
        connection = source.split("async fn sync_connection(", 1)[1].split(
            "async fn access_token(", 1
        )[0]
        self.assertIn("counts.scan_result()?;", connection)
        self.assertLess(
            connection.index("counts.scan_result()?;"),
            connection.index(".mark_sync_ok("),
        )
        self.assertIn("files_refused == 0", source)

    def test_intake_postgres_proofs_run_in_local_and_ci_gates(self):
        for path in (ROOT / "justfile", ROOT / ".github/workflows/ci.yml"):
            with self.subTest(path=path):
                source = path.read_text()
                # Both entry points use multiline shell commands.
                prefix = r"\{\{CARGO\}\}" if path.name == "justfile" else r"cargo"
                commands = re.findall(
                    prefix + r"[^\n]*crowdrelay-worker --test postgres"
                    r"(?:[^\n]*\\\n)+[^\n]*",
                    source,
                )
                self.assertTrue(commands)
                self.assertTrue(any(re.search(r"\bsheet_intake\b", c) for c in commands))


if __name__ == "__main__":
    unittest.main()
