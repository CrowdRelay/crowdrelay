#!/usr/bin/env python3
"""Every idempotency key claimed with a lease must be reclaimable.

The pattern across this workspace is: insert a key as `in_progress` with a
lease, do the work, mark it `completed`. The lease exists for exactly one
reason -- a request can die between claiming the key and completing it, and
something has to let the next attempt take the key over. Writing the lease and
never reading it back turns a transient crash into a permanent block on that
key, and the failure is invisible until someone retries.

That shipped twice. `mobile_fan` city requests wedged for the 24 hours of their
retention, on the first screen of onboarding. `releases` was worse: the key is
derived from the release rather than supplied by the caller, so every retry
collides with the same row, and the retention is ten years -- a release that
crashed mid-announcement could never be announced at all.

The check is per crate, not per file, because the claim and the reclaim are
often in sibling modules: `referrals` claims in `referrals.rs` and reclaims in
`referrals/repository.rs`, and `events` does the same.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

CLAIM = re.compile(r"(?s)INSERT INTO idempotency_keys.*?\"#")
RECLAIM = re.compile(
    r"(?s)UPDATE idempotency_keys.*?SET.*?lease_owner.*?lease_expires_at\s*<=\s*now\(\).*?\"#"
)
READS_LEASE = re.compile(r"lease_expires_at\s*<=\s*now\(\)")


def rust_sources() -> list[Path]:
    return [
        path
        for path in (ROOT / "crates").rglob("*.rs")
        if "target" not in path.parts
    ]


class IdempotencyLeaseRecovery(unittest.TestCase):
    def test_every_leased_claim_can_be_taken_over_after_it_expires(self) -> None:
        by_crate: dict[str, dict[str, object]] = {}
        for path in rust_sources():
            source = path.read_text(encoding="utf-8")
            if "idempotency_keys" not in source:
                continue
            crate = path.relative_to(ROOT / "crates").parts[0]
            entry = by_crate.setdefault(
                crate, {"claims": [], "reads": False, "reclaims": False}
            )
            if any("lease_expires_at" in stmt for stmt in CLAIM.findall(source)):
                entry["claims"].append(str(path.relative_to(ROOT)))
            if READS_LEASE.search(source):
                entry["reads"] = True
            if RECLAIM.search(source):
                entry["reclaims"] = True

        self.assertTrue(by_crate, "no idempotency call sites found; the scan is wrong")
        broken = [
            f"{crate} claims a leased key in {entry['claims']} but never "
            + ("reads the lease back" if not entry["reads"] else "reclaims an expired one")
            for crate, entry in sorted(by_crate.items())
            if entry["claims"] and not (entry["reads"] and entry["reclaims"])
        ]
        self.assertEqual(
            [],
            broken,
            "a leased idempotency claim with no recovery turns a crashed request "
            "into a permanently blocked key: " + "; ".join(broken),
        )


if __name__ == "__main__":
    unittest.main()
