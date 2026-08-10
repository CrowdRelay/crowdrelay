from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]

class Postgres18RuntimeContract(unittest.TestCase):
    def test_runtime_rejects_pre_18(self):
        database = (ROOT / "crates/crowdrelay-infra/src/database.rs").read_text()
        self.assertIn("MIN_POSTGRES_SERVER_VERSION_NUM: i32 = 180_000", database)
        self.assertIn("UnsupportedServerVersion", database)
        self.assertIn("current_setting('server_version_num')", database)

    def test_cutover_is_fail_closed_and_keeps_pg16(self):
        script = (ROOT / "ops/postgres18/migrate.sh").read_text()
        self.assertIn('PG18_IMAGE="${PG18_IMAGE:-postgres:18-alpine}"', script)
        self.assertIn('/var/lib/postgresql', script)
        self.assertIn('WRITER_CONTAINERS is required for cutover', script)
        self.assertIn('cutover requires explicit --cutover', script)
        self.assertIn('rollback requires --rollback', script)
        self.assertNotIn('docker rm -v "$OLD_CONTAINER"', script)
        self.assertNotIn('docker volume rm', script)
        self.assertIn('sha256sum', script)
        self.assertIn('diff -u "$COUNTS_BEFORE" "$COUNTS_AFTER"', script)


    def test_topology_audit_is_explicitly_read_only(self):
        audit = (ROOT / "ops/postgres18/audit-topology.sh").read_text()
        self.assertIn("POSTGRES_TOPOLOGY_AUDIT=PASS read_only=true", audit)
        self.assertIn("credentials redacted", audit)
        self.assertNotIn("CREATE ROLE", audit.upper())
        self.assertNotIn("CREATE DATABASE", audit.upper())
        self.assertNotIn("ALTER ROLE", audit.upper())
        self.assertNotIn("DROP DATABASE", audit.upper())

if __name__ == "__main__":
    unittest.main()
