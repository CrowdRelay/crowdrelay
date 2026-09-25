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

Structs that derive both are left alone. They round-trip their own JSON through
serde, rows already stored hold the tuple, and changing the format there needs
a reader that accepts both. The briefing's `deadline_at` reader shows how.
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


def unformatted() -> tuple[list[str], int]:
    found, judged = [], 0
    for path in sorted((ROOT / "crates").rglob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        if "/tests/" in relative:
            continue
        text = path.read_text(errors="ignore")
        for struct in STRUCT.finditer(text):
            attributes = struct.group(1)
            if not re.search(r"\bSerialize\b", attributes) or re.search(r"\bDeserialize\b", attributes):
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


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    print("SERIALIZED_DATES=" + ("PASS" if result.wasSuccessful() else "FAIL"))
    sys.exit(0 if result.wasSuccessful() else 1)
