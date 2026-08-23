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

    def test_release_surface_docs_exist(self):
        for relative in ("RELEASE.md", "docs/ARCHITECTURE.md", "openapi/openapi.yaml"):
            self.assertTrue((ROOT / relative).is_file(), relative)

    def test_public_schema_version_tracks_latest_migration(self):
        migrations = sorted((ROOT / "migrations").glob("[0-9][0-9][0-9][0-9]_*.sql"))
        latest = int(migrations[-1].name.split("_", 1)[0])
        meta = (ROOT / "crates/crowdrelay-api/src/meta.rs").read_text()
        ops_root = ROOT / "crates/crowdrelay-api/src/ops.rs"
        ops = "\n".join(
            path.read_text()
            for path in (
                ops_root,
                ops_root.parent / "ops/models.rs",
                ops_root.parent / "ops/handlers.rs",
                ops_root.parent / "ops/query_support.rs",
            )
        )
        self.assertIn(f"const SCHEMA_VERSION: u32 = {latest};", meta)
        self.assertIn("schema_version: crate::meta::SCHEMA_VERSION,", ops)
        self.assertIn('"communication_delivery_ledger_v1"', meta)

    def test_release_ledger_exposes_n8n_executor_manifest_drift(self):
        control = ROOT / "crates/crowdrelay-application/src/autopilot/control.rs"
        application = "\n".join(
            path.read_text()
            for path in (control, control.parent / "control/state_ports.rs", control.parent / "control/runtime_ports.rs")
        )
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
        bake = (ROOT / "docker-bake.hcl").read_text()
        self.assertIn("CROWDRELAY_GIT_SHA: ${{ env.IMAGE_SHA }}", publish)
        self.assertIn("CROWDRELAY_BUILD_TIMESTAMP: ${{ env.BUILD_TIMESTAMP }}", publish)
        self.assertIn("CROWDRELAY_GIT_SHA         = CROWDRELAY_GIT_SHA", bake)
        self.assertIn("CROWDRELAY_BUILD_TIMESTAMP = CROWDRELAY_BUILD_TIMESTAMP", bake)
        self.assertIn("git_sha: Option<&'static str>", meta)
        self.assertIn('option_env!("CROWDRELAY_GIT_SHA")', meta)
        self.assertIn("meta::release_identity()", api)
        self.assertIn("gitSha", openapi)


if __name__ == "__main__":
    unittest.main()
