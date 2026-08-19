from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class ReleaseHardeningContract(unittest.TestCase):
    def test_city_request_idempotency_is_durable_and_not_http_sql(self) -> None:
        api = (ROOT / "crates/crowdrelay-api/src/mobile_fan.rs").read_text()
        infra = (ROOT / "crates/crowdrelay-infra/src/mobile_fan.rs").read_text()
        self.assertIn('CITY_REQUEST_SCOPE: &str = "city_request"', infra)
        self.assertIn("request_hash", infra)
        self.assertIn("response_body", infra)
        self.assertIn("MobileFanStoreError::Conflict", api)
        self.assertNotIn("INSERT INTO cities", api)
        self.assertNotIn("INSERT INTO outbox_events", api)
        self.assertNotIn("UPDATE cities", api)
        self.assertNotIn("INSERT INTO nearby_gig_notifications", api)

    def test_city_request_openapi_documents_idempotent_replay_and_conflict(self) -> None:
        import yaml
        spec = yaml.safe_load((ROOT / "openapi/openapi.yaml").read_text())
        operation = spec["paths"]["/public/cities/requests"]["post"]
        self.assertEqual(8, spec["components"]["parameters"]["IdempotencyKey"]["schema"]["minLength"])
        self.assertIn("409", operation["responses"])
        self.assertEqual(
            ["approved"],
            operation["responses"]["200"]["content"]["application/json"]["schema"]["properties"]["status"]["enum"],
        )
        self.assertEqual(
            ["pending"],
            operation["responses"]["202"]["content"]["application/json"]["schema"]["properties"]["status"]["enum"],
        )

    def test_metrics_never_substitute_zeroes_for_failed_ops_snapshot(self) -> None:
        api = (ROOT / "crates/crowdrelay-api/src/lib.rs").read_text()
        self.assertNotIn("metrics_snapshot().await.unwrap_or_default()", api)
        self.assertIn("crowdrelay_ops_metrics_snapshot_available 0", api)
        self.assertIn("StatusCode::SERVICE_UNAVAILABLE", api)

    def test_timezone_validation_uses_real_iana_database(self) -> None:
        tenant = (ROOT / "crates/crowdrelay-api/src/tenant.rs").read_text()
        worker = (ROOT / "crates/crowdrelay-worker/src/push_delivery.rs").read_text()
        regional = (ROOT / "crates/crowdrelay-infra/src/regional.rs").read_text()
        self.assertIn("time_tz::timezones::get_by_name", regional)
        self.assertIn("is_known_iana_timezone(value)", tenant)
        self.assertIn("is_known_iana_timezone(&quiet_timezone)", worker)
        self.assertIn("Mars/Olympus", regional)

    def test_examples_and_ci_define_non_virya_regional_profile(self) -> None:
        required = (
            "CROWDRELAY_TENANT_REGION",
            "CROWDRELAY_TENANT_LOCALE",
            "CROWDRELAY_TENANT_TIMEZONE",
            "CROWDRELAY_TENANT_CURRENCY",
            "CROWDRELAY_TENANT_DATE_FORMAT",
            "CROWDRELAY_TENANT_NUMBER_FORMAT",
            "CROWDRELAY_TENANT_DATA_REGION",
        )
        for relative in (
            ".env.example",
            "deploy/env.production.example",
            ".github/workflows/ci.yml",
            ".github/workflows/performance.yml",
        ):
            text = (ROOT / relative).read_text()
            for key in required:
                self.assertIn(key, text, f"{relative} missing {key}")


if __name__ == "__main__":
    unittest.main()
