from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class PostgresRuntimeContract(unittest.TestCase):
    """Pins the PostgreSQL major the repo is developed against.

    Dev/CI run the 19 beta (image overridable via CROWDRELAY_POSTGRES_IMAGE);
    the runtime guard in database.rs stays at 18 because production consumes
    an externally managed database that may still be on 18. The filename keeps
    its historical name pending a deliberate rename; content tracks reality.
    """

    def test_runtime_rejects_pre_18(self):
        database = (ROOT / "crates/crowdrelay-infra/src/database.rs").read_text()
        self.assertIn("MIN_POSTGRES_SERVER_VERSION_NUM: i32 = 180_000", database)
        self.assertIn("UnsupportedServerVersion", database)
        self.assertIn("current_setting('server_version_num')", database)

    def test_production_runtime_requires_external_pg(self):
        compose = (ROOT / "compose.production.yaml").read_text()
        database = (ROOT / "crates/crowdrelay-infra/src/database.rs").read_text()
        # Production intentionally consumes the externally managed database;
        # do not regress by re-introducing an application-owned postgres service.
        self.assertNotIn("image: postgres:", compose)
        self.assertIn("MIN_POSTGRES_SERVER_VERSION_NUM: i32 = 180_000", database)
        self.assertIn("current_setting('server_version_num')", database)

    def test_ci_and_local_runtime_pin_pg19_beta(self):
        compose = (ROOT / "docker-compose.yml").read_text()
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("CROWDRELAY_POSTGRES_IMAGE:-postgres:19beta3-alpine", compose)
        self.assertIn("postgres:19beta3-alpine", ci)
        for banned in ("postgres:16", "postgres:17", "postgres:18-alpine"):
            self.assertNotIn(banned, compose + ci)

    def test_pg19_io_worker_autoscaling_is_configured(self):
        compose = (ROOT / "docker-compose.yml").read_text()
        self.assertIn("io_min_workers=${CROWDRELAY_POSTGRES_IO_MIN_WORKERS:-", compose)
        self.assertIn("io_max_workers=${CROWDRELAY_POSTGRES_IO_MAX_WORKERS:-", compose)
        # io_workers was replaced by auto-scaling in 19; a stale flag would
        # crash the server at startup.
        self.assertNotIn("io_workers=", compose)
        self.assertIn("crowdrelay_postgres19:", compose)

    def test_restore_rehearsal_tracks_the_production_major(self):
        rehearsal = (ROOT / "ops/backup/restore-rehearsal.sh").read_text()
        # The default tracks whatever major production currently runs; it
        # deliberately lags the dev/CI pin until the external database moves.
        self.assertIn('PG18_IMAGE="${PG18_IMAGE:-postgres:', rehearsal)
        self.assertIn("--network none", rehearsal)
        self.assertIn("RESTORE_REHEARSAL=PASS", rehearsal)
        self.assertIn("sha256sum", rehearsal)
        self.assertNotIn("CROWDRELAY_DATABASE_URL", rehearsal)


if __name__ == "__main__":
    unittest.main()
