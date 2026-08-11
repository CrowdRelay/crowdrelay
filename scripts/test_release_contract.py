#!/usr/bin/env python3
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EXPECTED = "1.0.0"


class ReleaseContract(unittest.TestCase):
    def test_release_version_is_consistent(self):
        self.assertEqual((ROOT / "VERSION").read_text().strip(), EXPECTED)
        cargo = (ROOT / "Cargo.toml").read_text()
        self.assertRegex(cargo, rf'(?m)^version = "{re.escape(EXPECTED)}"$')
        openapi = (ROOT / "openapi/openapi.yaml").read_text()
        self.assertRegex(openapi, rf'(?m)^  version: {re.escape(EXPECTED)}$')

    def test_stable_contract_docs_exist(self):
        for relative in ("RELEASE.md", "docs/STABLE_CONTRACT.md", "openapi/openapi.yaml"):
            self.assertTrue((ROOT / relative).is_file(), relative)

    def test_contract_is_service_boundary_not_internal_crate_promise(self):
        text = (ROOT / "docs/STABLE_CONTRACT.md").read_text().lower()
        self.assertIn("openapi/openapi.yaml", text)
        self.assertIn("private surfaces", text)
        self.assertIn("domain -> application -> infrastructure", text)


if __name__ == "__main__":
    unittest.main()
