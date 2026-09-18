#!/usr/bin/env python3
"""Every segment filter a writer stores must be one the reader can parse.

`audience_segments.filter` is written by the autopilot's execution arms and read
back by `AudienceFilter` in `crates/crowdrelay-api/src/audience/models.rs`, which
carries `#[serde(deny_unknown_fields)]`. That attribute is correct — a filter
with a key nobody applies is a filter that silently means something other than
it says — but it makes the two sides one contract with no compiler between them:
the writers build `serde_json::json!` literals, so a key the struct does not have
compiles, inserts, and fails only when somebody clicks preview.

That happened. The show-growth merch lever wrote an `offer_contract` object into
the filter. Every preview of a merch segment then answered 503 from an arm that
logged nothing, and the console rendered it as "the audience backend may not
support this segment" — the one explanation that was never true, since the
backend held the segment and could not read it back.

This gate reads the struct's field list from the source and checks every
filter literal against it. It is deliberately source-reading rather than a Rust
test: the writers live in `crowdrelay-infra` and the type lives in
`crowdrelay-api`, which does not depend on it, so no unit test can see both.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MODELS = ROOT / "crates/crowdrelay-api/src/audience/models.rs"
CRATES = ROOT / "crates"

#: Filter literals are recognised by carrying a `statuses` key — the one field
#: every writer sets, and the cheapest way to tell an audience filter apart from
#: the dozens of other `json!` objects in the same files.
MARKER = '"statuses"'


def filter_fields() -> set[str]:
    """The fields `AudienceFilter` accepts, read from its declaration."""
    text = MODELS.read_text(encoding="utf-8")
    body = text.split("pub struct AudienceFilter {", 1)[1].split("\n}", 1)[0]
    fields = set(re.findall(r"(?m)^\s{4}(?:pub\s+)?([a-z_]+):", body))
    if len(fields) < 5:
        raise SystemExit(
            "AudienceFilter parsed to fewer than five fields — the declaration moved "
            "and this gate is now matching nothing"
        )
    return fields


def literals() -> list[tuple[Path, int, set[str]]]:
    """Every `json!` object that looks like a stored audience filter."""
    found: list[tuple[Path, int, set[str]]] = []
    for path in sorted(CRATES.rglob("*.rs")):
        text = path.read_text(encoding="utf-8", errors="replace")
        if "audience_segments" not in text and "segment_filter" not in text:
            continue
        for match in re.finditer(r"json!\(\{(.{0,1200}?)\}\)", text, re.S):
            body = match.group(1)
            if MARKER not in body:
                continue
            keys = set(re.findall(r'"([a-z_]+)"\s*:', body))
            line = text[: match.start()].count("\n") + 1
            found.append((path, line, keys))
    return found


def main() -> int:
    fields = filter_fields()
    candidates = literals()
    if not candidates:
        # A gate that matches nothing passes for the wrong reason. The writers
        # exist; if none is found, the search shape is wrong, not the code.
        print("AUDIENCE_SEGMENT_FILTERS=FAIL no filter literals found to check")
        return 1

    failures: list[str] = []
    for path, line, keys in candidates:
        # Nested objects contribute their own keys to this flat scan, which is
        # exactly what should fail: a nested object is itself an unknown field.
        unknown = sorted(keys - fields)
        if unknown:
            relative = path.relative_to(ROOT)
            failures.append(
                f"{relative}:{line}: segment filter carries {unknown}, which "
                f"AudienceFilter does not accept — every preview of the segments this "
                f"writes will fail to deserialize"
            )

    for failure in failures:
        print(failure)
    if failures:
        print(f"AUDIENCE_SEGMENT_FILTERS=FAIL literals={len(candidates)}")
        return 1
    print(
        f"AUDIENCE_SEGMENT_FILTERS=PASS literals={len(candidates)} fields={len(fields)}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
