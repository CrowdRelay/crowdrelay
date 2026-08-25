from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class Postgres18RuntimeContract(unittest.TestCase):
    def test_runtime_rejects_pre_18(self):
        database = (ROOT / "crates/crowdrelay-infra/src/database.rs").read_text()
        self.assertIn("MIN_POSTGRES_SERVER_VERSION_NUM: i32 = 180_000", database)
        self.assertIn("UnsupportedServerVersion", database)
        self.assertIn("current_setting('server_version_num')", database)

    def test_production_runtime_requires_external_pg18(self):
        compose = (ROOT / "compose.production.yaml").read_text()
        database = (ROOT / "crates/crowdrelay-infra/src/database.rs").read_text()
        # Production intentionally consumes the externally managed database;
        # do not regress by re-introducing an application-owned postgres service.
        self.assertNotIn("image: postgres:", compose)
        self.assertIn("MIN_POSTGRES_SERVER_VERSION_NUM: i32 = 180_000", database)
        self.assertIn("current_setting('server_version_num')", database)

    def test_ci_and_local_runtime_pin_pg18(self):
        compose = (ROOT / "docker-compose.yml").read_text()
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("postgres:18-alpine", compose)
        self.assertIn("postgres:18-alpine", ci)
        self.assertNotIn("postgres:16", compose + ci)
        self.assertNotIn("postgres:17", compose + ci)

    def test_restore_rehearsal_remains_pg18_and_isolated(self):
        rehearsal = (ROOT / "ops/backup/restore-rehearsal.sh").read_text()
        self.assertIn('PG18_IMAGE="${PG18_IMAGE:-postgres:18-alpine}"', rehearsal)
        self.assertIn("--network none", rehearsal)
        self.assertIn("RESTORE_REHEARSAL=PASS", rehearsal)
        self.assertIn("sha256sum", rehearsal)
        self.assertNotIn("CROWDRELAY_DATABASE_URL", rehearsal)


if __name__ == "__main__":
    unittest.main()
