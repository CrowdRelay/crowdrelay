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

    def test_public_schema_version_tracks_latest_migration(self):
        migrations = sorted((ROOT / "migrations").glob("[0-9][0-9][0-9][0-9]_*.sql"))
        latest = int(migrations[-1].name.split("_", 1)[0])
        meta = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text()
        ops = (ROOT / "crates/crowdrelay-api/src/ops.rs").read_text()
        self.assertIn(f"const SCHEMA_VERSION: u32 = {latest};", meta)
        self.assertIn("schema_version: crate::meta::SCHEMA_VERSION,", ops)
        self.assertIn('"communication_delivery_ledger_v1"', meta)

    def test_release_ledger_exposes_n8n_executor_manifest_drift(self):
        application = (ROOT / "crates/crowdrelay-application/src/autopilot/control.rs").read_text()
        infra = (ROOT / "crates/crowdrelay-infra/src/autopilot/runtime.rs").read_text()
        openapi = (ROOT / "openapi/openapi.yaml").read_text()
        self.assertIn("pub executor_manifest_drift: bool", application)
        self.assertIn("n8n_release_manifest_sha", infra)
        self.assertIn("executor_manifest_drift", openapi)


    def test_container_build_embeds_immutable_release_identity(self):
        dockerfile = (ROOT / "Dockerfile").read_text()
        publish = (ROOT / ".github/workflows/publish-images.yml").read_text()
        meta = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text()
        api = (ROOT / "crates/crowdrelay-api/src/lib.rs").read_text()
        openapi = (ROOT / "openapi/openapi.yaml").read_text()
        self.assertIn('ARG CROWDRELAY_GIT_SHA=""', dockerfile)
        self.assertIn('ARG CROWDRELAY_BUILD_TIMESTAMP=""', dockerfile)
        self.assertIn("CROWDRELAY_GIT_SHA=${{ env.IMAGE_SHA }}", publish)
        self.assertIn("CROWDRELAY_BUILD_TIMESTAMP=${{ env.BUILD_TIMESTAMP }}", publish)
        self.assertIn("git_sha: Option<&\'static str>", meta)
        self.assertIn('option_env!("CROWDRELAY_GIT_SHA")', meta)
        self.assertIn("meta::release_identity()", api)
        self.assertIn("gitSha", openapi)

    def test_contract_is_service_boundary_not_internal_crate_promise(self):
        text = (ROOT / "docs/STABLE_CONTRACT.md").read_text().lower()
        self.assertIn("openapi/openapi.yaml", text)
        self.assertIn("private surfaces", text)
        self.assertIn("domain -> application -> infrastructure", text)


if __name__ == "__main__":
    unittest.main()
