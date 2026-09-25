#!/usr/bin/env python3
"""A timestamp that leaves Rust as JSON is an RFC 3339 string, not serde's tuple.

With the `time` features this workspace enables, a bare `OffsetDateTime`
serializes as `[2026, 268, 7, 0, 0, 0, 0, 0, 0]`: year, day of year, time,
offset. Nothing outside Rust reads that. The console's booking journey ran
`new Date(...)` on a gig-plan letter's `approved_at` and showed "approved
recently" for every one of them. The daily briefing cast a stored deadline with
`::timestamptz` and aborted team handoffs for a whole day (2026-09-24).

This covers every struct that derives `Serialize` but not `Deserialize`. Such a
struct only writes: an HTTP response, an outbox payload, an audit or decision
snapshot. Each of its `OffsetDateTime` fields must carry
`#[serde(with = "time::serde::rfc3339")]` (or `rfc3339::option`), and each
`Date` field `#[serde(with = "crowdrelay_domain::iso_date")]` (or `::option`),
or an explicit `serialize_with` or `skip`. A bare `Date` is `[2026, 268]`: the
public night page printed exactly that.

Structs that derive both write JSON and read it back, and rows already stored
hold the tuple. They use `crowdrelay_domain::wire_time`, which writes text and
reads either shape: passes, coupons, referral rewards, attestations, approval
tokens and roster briefs all reached a client that way. The exceptions are
named in `INTERNAL_ROUND_TRIP` with the reason they stay: brain-internal
records that never leave Rust.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
STRUCT = re.compile(r"((?:#\[[^\]]*\]\s*)*)(?:pub(?:\([^)]*\))?\s+)?struct\s+(\w+)\s*\{")
FIELD = re.compile(
    r"((?:#\[[^\]]*\]\s*)*)(?:pub(?:\([^)]*\))?\s+)?(\w+)\s*:\s*((?:Option<\s*)?(?:time::)?(?:OffsetDateTime|PrimitiveDateTime|Date)\b)"
)
FORMATTED = re.compile(r"serde\([^)]*\b(with|serialize_with|skip|skip_serializing)\b")


def braced(text: str, open_brace: int) -> str:
    depth, index = 1, open_brace + 1
    while depth and index < len(text):
        depth += {"{": 1, "}": -1}.get(text[index], 0)
        index += 1
    return text[open_brace + 1 : index - 1]


# Round-trip types that never leave Rust: the brain's own evidence and
# experiment records, persisted as JSON and read back only by the brain.
# Adding a name here is a claim that no client, payload or SQL cast reads it.
INTERNAL_ROUND_TRIP = {
    "FanOutcome",
    "GrowthEvidence",
    "EvidenceEvent",
    "ExperimentAssignment",
    "ExperimentDesign",
    "FanProvenanceEvent",
    "LifecycleTransition",
    "GrowthHypothesis",
    "ValidationWindow",
}


def unformatted(round_trip: bool = False) -> tuple[list[str], int]:
    found, judged = [], 0
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        if "/tests/" in relative:
            continue
        text = path.read_text(errors="ignore")
        for struct in STRUCT.finditer(text):
            attributes = struct.group(1)
            if not re.search(r"\bSerialize\b", attributes):
                continue
            if bool(re.search(r"\bDeserialize\b", attributes)) != round_trip:
                continue
            if round_trip and struct.group(2) in INTERNAL_ROUND_TRIP:
                continue
            body = braced(text, struct.end() - 1)
            for field in FIELD.finditer(body):
                judged += 1
                if FORMATTED.search(field.group(1)):
                    continue
                line = text[: struct.end() + field.start(2)].count("\n") + 1
                found.append(f"{relative}:{line} {struct.group(2)}.{field.group(2)}")
    return found, judged


class SerializedDates(unittest.TestCase):
    def test_write_only_timestamps_are_rfc3339(self) -> None:
        found, judged = unformatted()
        self.assertGreater(judged, 50, f"only {judged} timestamp fields judged; the scan broke")
        self.assertEqual(
            found,
            [],
            "these timestamps serialize as serde's tuple; add "
            '#[serde(with = "time::serde::rfc3339")] (or ::option):\n  ' + "\n  ".join(found),
        )

    def test_round_trip_timestamps_write_text_and_read_both(self) -> None:
        found, judged = unformatted(round_trip=True)
        self.assertGreater(judged, 10, f"only {judged} round-trip timestamp fields judged")
        self.assertEqual(
            found,
            [],
            "these round-trip timestamps still write serde's tuple; use "
            '#[serde(with = "crowdrelay_domain::wire_time")] (or ::option, ::date), '
            "or name the type in INTERNAL_ROUND_TRIP with the reason:\n  " + "\n  ".join(found),
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    print("SERIALIZED_DATES=" + ("PASS" if result.wasSuccessful() else "FAIL"))
    sys.exit(0 if result.wasSuccessful() else 1)
