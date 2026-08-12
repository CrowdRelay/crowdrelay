#!/usr/bin/env python3
import csv
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class ViryaOsExecutorManifestContract(unittest.TestCase):
    def test_public_manifest_matches_rust_event_capabilities(self):
        source = (ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs").read_text()
        start = source.index("fn executor_capability_for_event")
        end = source.index("pub(in crate::autopilot) async fn ensure_executor_capability", start)
        body = source[start:end]
        rust_pairs = dict(re.findall(r'"(viryaos\.[^"]+)"\s*=>\s*"([^"]+)"', body))
        with (ROOT / "n8n/viryaos-executor-manifest.tsv").open(newline="") as handle:
            rows = list(csv.DictReader(handle, delimiter="\t"))
        manifest_pairs = {row["event_type"]: row["capability"] for row in rows}
        self.assertEqual(manifest_pairs, rust_pairs)

    def test_unsafe_provider_capabilities_default_off(self):
        with (ROOT / "n8n/viryaos-executor-manifest.tsv").open(newline="") as handle:
            rows = {row["capability"]: row for row in csv.DictReader(handle, delimiter="\t")}
        self.assertEqual(rows["promotion.budget"]["default_advertised"], "0")
        self.assertEqual(rows["calendar.upsert"]["default_advertised"], "0")
        self.assertEqual(rows["funding.package"]["default_advertised"], "0")
        for capability in ("merch.reorder", "merch.bundle", "content.artifact", "show.escalation"):
            self.assertEqual(rows[capability]["default_advertised"], "1")


    def test_concrete_production_manifest_matches_capability_contract(self):
        with (ROOT / "n8n/viryaos-executor-manifest.tsv").open(newline="") as handle:
            contract = {row["event_type"]: row for row in csv.DictReader(handle, delimiter="\t")}
        with (ROOT / "n8n/viryaos-production-workflow-manifest.tsv").open(newline="") as handle:
            production = {row["event_type"]: row for row in csv.DictReader(handle, delimiter="\t")}

        for event_type, row in contract.items():
            if row["default_advertised"] == "1":
                self.assertIn(event_type, production)
                self.assertEqual(row["capability"], production[event_type]["capability"])
                self.assertEqual("1", production[event_type]["enabled"])
            elif event_type in production:
                self.assertEqual(row["capability"], production[event_type]["capability"])
                self.assertEqual("0", production[event_type]["enabled"])

    def test_production_manifest_sha_file_is_exact(self):
        import hashlib
        manifest = ROOT / "n8n/viryaos-production-workflow-manifest.tsv"
        expected = (ROOT / "n8n/viryaos-production-workflow-manifest.sha256").read_text().strip()
        self.assertEqual(hashlib.sha256(manifest.read_bytes()).hexdigest(), expected)
        self.assertRegex(expected, r"^[0-9a-f]{64}$")

    def test_legacy_import_helper_is_fail_closed(self):
        helper = (ROOT / "n8n/import-workflows.sh").read_text()
        self.assertIn("REFUSED", helper)
        self.assertNotIn("n8n import:workflow", helper)


if __name__ == "__main__":
    unittest.main()
