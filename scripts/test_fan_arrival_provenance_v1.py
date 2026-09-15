"""Contract tests for fan-arrival provenance.

`fan_acquisition_events` is how a fan's first touch is remembered, and the
channel-ROI readout counts it — so a fan-creation path that writes no
acquisition row makes an arrival channel invisible, which reports as zero
rather than as unmeasured. The two are not the same and the readout cannot
tell them apart on its own.

Until 2026-09-15 only `POST /v1/fans` wrote one. This suite pins the rule the
rest of the paths now follow: every `INSERT INTO fans` in production code
either carries provenance in the same statement (the bulk CTEs), calls
`record_fan_arrival` in the same transaction, or is a named exception with the
port decision documented. A new fan-creation path has to choose — it cannot
slip in silently.
"""

from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
CRATES = ROOT / "crates"

INSERT_RE = re.compile(r"INSERT\s+INTO\s+fans\b", re.IGNORECASE)

# The signup path's own INSERT lives here; `insert_acquisition_event` runs
# in the same transaction via `persist_fan_signup_inner`.
SIGNUP_PATH = "crates/crowdrelay-infra/src/acquisition/persistence_methods.rs"

# Paths that instrument their INSERT — the bulk sites fold provenance into
# the same statement, the single-row site calls `record_fan_arrival`.
INSTRUMENTED = {
    "crates/crowdrelay-infra/src/fan_import.rs": "fan_acquisition_events",
    "crates/crowdrelay-infra/src/fanbase/ingestion.rs": "fan_acquisition_events",
    "crates/crowdrelay-infra/src/concert_qr.rs": "record_fan_arrival",
}

# Paths that still create fans with no provenance row. Each needs either a
# port through application→infra (the api-sql ratchet forbids a new write in
# crowdrelay-api) or a deliberate decision that the path is not an arrival
# channel. Listed so a reviewer can see the debt instead of rediscovering it.
KNOWN_EXCEPTIONS = {
    # Ticket buyers — the conversion channel. Needs the api→infra port.
    "crates/crowdrelay-api/src/ticketing/payments.rs",
    # Synesthesia reward claims. Needs the api→infra port.
    "crates/crowdrelay-api/src/synesthesia/rewards.rs",
}


def production_fan_insert_sites() -> list[str]:
    """Every non-test .rs file containing `INSERT INTO fans` in production
    code — text after the first `#[cfg(test)]` is a test mod by convention
    in this workspace, so it is not scanned (fixtures seeding `fans` inside
    `outbox/repository.rs` and `reminders.rs` would otherwise read as
    production debt)."""
    sites: list[str] = []
    for path in CRATES.rglob("*.rs"):
        rel = path.relative_to(ROOT).as_posix()
        name = path.name
        if (
            "/tests/" in rel
            or name.startswith("test_")
            or name in ("tests.rs", "test.rs")
            or name.endswith(("_test.rs", "_tests.rs"))
        ):
            continue
        text = path.read_text(encoding="utf-8")
        text = text.split("#[cfg(test)]", 1)[0]
        if INSERT_RE.search(text):
            sites.append(rel)
    return sorted(sites)


class FanArrivalProvenanceContract(unittest.TestCase):
    def test_every_fan_insert_path_is_classified(self) -> None:
        """A fan INSERT must be instrumented, the signup path, or a named
        exception — an unlisted site is a new arrival channel that decided
        nothing."""
        expected = set(INSTRUMENTED) | KNOWN_EXCEPTIONS | {SIGNUP_PATH}
        actual = set(production_fan_insert_sites())
        self.assertEqual(
            actual,
            expected,
            f"fan-INSERT sites drifted: new={sorted(actual - expected)} "
            f"gone={sorted(expected - actual)} — instrument the new path or "
            f"name it an exception here",
        )

    def test_instrumented_paths_still_write_provenance(self) -> None:
        """An instrumented path that loses its marker quietly returns to
        invisible — the marker itself is pinned, not just the file list."""
        for rel, marker in INSTRUMENTED.items():
            body = (ROOT / rel).read_text(encoding="utf-8")
            self.assertIn(marker, body, f"{rel} lost {marker}")

    def test_provenance_is_only_written_for_new_rows(self) -> None:
        """An acquisition row means 'this is how the fan arrived'. Writing one
        for an ON CONFLICT hit fabricates a second arrival — the bulk sites
        must select from RETURNING, the single-row site must gate on it."""
        for rel in ("fan_import.rs", "fanbase/ingestion.rs"):
            body = (ROOT / "crates/crowdrelay-infra/src" / rel).read_text(
                encoding="utf-8"
            )
            self.assertIn("ON CONFLICT (workspace_id, normalized_email) DO NOTHING", body)
            self.assertIn("RETURNING id", body)
            self.assertIn("FROM created", body)
        qr = (ROOT / "crates/crowdrelay-infra/src/concert_qr.rs").read_text(
            encoding="utf-8"
        )
        arrival = qr.split("record_fan_arrival", 1)[0]
        self.assertIn("Some((id, status))", arrival)

    def test_source_vocabulary_stays_named_not_free(self) -> None:
        """The readout groups by `source`; a free-form string per call site
        fragments the channel it is supposed to measure."""
        for rel, marker in INSTRUMENTED.items():
            if marker != "record_fan_arrival":
                continue
            body = (ROOT / rel).read_text(encoding="utf-8")
            self.assertIn('"concert_qr"', body)
        ingest = (ROOT / "crates/crowdrelay-infra/src/fanbase/ingestion.rs").read_text(
            encoding="utf-8"
        )
        self.assertIn("'fanbase_ingest'", ingest)
        imp = (ROOT / "crates/crowdrelay-infra/src/fan_import.rs").read_text(
            encoding="utf-8"
        )
        self.assertIn('format!("fan_import:{origin}")', imp)


if __name__ == "__main__":
    unittest.main()
