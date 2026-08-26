#!/usr/bin/env python3
"""Reddit discovery adapter v1 contract: supply without judgment.

The adapter is deliberately dumb — fetch, normalize, upsert with raw evidence.
These pins keep it dumb in the right ways:

- dark by default: no queries configured means no network calls, ever;
- NSFW listings are excluded at the adapter boundary;
- every imported place carries a raw scan payload for audit and replay;
- the worker is wired into the runtime behind a shutdown watch.
"""
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

ADAPTER = ROOT / "crates/crowdrelay-worker/src/discovery.rs"
MAIN = ROOT / "crates/crowdrelay-worker/src/main.rs"


class DiscoveryRedditContract(unittest.TestCase):
    def test_dark_by_default(self):
        source = ADAPTER.read_text()
        self.assertIn("pub fn enabled(&self)", source)
        self.assertIn('CROWDRELAY_DISCOVERY_REDDIT_QUERIES', source)
        main = MAIN.read_text()
        self.assertIn("discovery_config.enabled()", main)

    def test_nsfw_excluded_at_the_boundary(self):
        source = ADAPTER.read_text()
        self.assertIn('.filter(|child| !child.data.over18)', source)
        # The filter must sit BEFORE normalization, not after.
        self.assertLess(
            source.index('.filter(|child| !child.data.over18)'),
            source.index('NormalizedSubreddit::from_listing'),
        )

    def test_every_import_carries_raw_scan_evidence(self):
        source = ADAPTER.read_text()
        self.assertIn('evidence_kind: "scan"', source)
        self.assertIn('"reddit_public_search"', source)
        self.assertIn('payload: &sub.raw', source)

    def test_politeness_spacing_is_pinned(self):
        source = ADAPTER.read_text()
        self.assertRegex(source, r"REQUEST_SPACING: Duration = Duration::from_secs\(\d+\)")
        self.assertIn("tokio::time::sleep(REQUEST_SPACING)", source)


if __name__ == "__main__":
    unittest.main()
