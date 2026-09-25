#!/usr/bin/env python3
"""Stop persistence logic drifting further into the HTTP layer.

docs/ARCHITECTURE.md describes `crowdrelay-api` as "HTTP authorization
boundaries, validation and response contracts", and asks new vertical slices to
"keep policy in domain/application code and isolate SQL, HTTP and provider
details in infrastructure or adapters". The crate graph honours that
(`crowdrelay-application` holds zero sqlx call sites), but the API crate itself
has accumulated write statements that mutate state without passing through the
domain or application layer.

This is a ratchet, not a hard cap, and it deliberately tracks *writes* only:
a SELECT in the HTTP layer is a defensible read model, whereas an INSERT/UPDATE/
DELETE there is a domain invariant that no longer has a single home. Adding
writes to a file, or introducing writes in a new one, fails until the slice is
moved behind a repository or the baseline is deliberately raised in review.

A file whose writes shrank fails too, until `--write-baseline` lowers its entry:
a baseline above the count is room for a new write nobody reviewed (four on
2026-09-25). That flag only lowers or drops entries, never raises one.
"""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BASELINE_PATH = ROOT / "scripts/api-sql-ratchet.json"
API_SRC = ROOT / "crates/crowdrelay-api/src"
WRITE = re.compile(r"\b(INSERT\s+INTO|UPDATE\s+\w|DELETE\s+FROM)\b", re.IGNORECASE)


def production_body(source: str) -> str:
    """Strip `#[cfg(test)]` modules, keeping everything around them.

    Truncating at the first marker would let any code appended after a test
    module escape the ratchet entirely, so each attribute's module is removed by
    matching its braces.
    """
    out = source
    while True:
        marker = out.find("#[cfg(test)]")
        if marker == -1:
            return out
        brace = out.find("{", marker)
        if brace == -1:
            return out[:marker]
        depth = 0
        end = None
        for index in range(brace, len(out)):
            char = out[index]
            if char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
                if depth == 0:
                    end = index + 1
                    break
        if end is None:
            return out[:marker]
        out = out[:marker] + out[end:]


def measure() -> dict[str, int]:
    counts: dict[str, int] = {}
    for path in sorted(API_SRC.rglob("*.rs")):
        found = len(WRITE.findall(production_body(path.read_text(encoding="utf-8"))))
        if found:
            counts[path.relative_to(ROOT).as_posix()] = found
    return counts


def main() -> int:
    baseline = json.loads(BASELINE_PATH.read_text(encoding="utf-8"))
    allowed = {str(k): int(v) for k, v in baseline["maxWrites"].items()}
    current = measure()
    failures: list[str] = []

    for path, found in sorted(current.items()):
        limit = allowed.get(path)
        if limit is None:
            failures.append(
                f"{path} introduces {found} SQL write(s) in the HTTP layer; "
                "move the mutation behind a repository in crowdrelay-infra"
            )
        elif found > limit:
            failures.append(
                f"{path} grew to {found} SQL writes (baseline {limit}); "
                "new mutations belong behind a repository, not in the HTTP layer"
            )

    stale = [
        f"{path} has {current.get(path, 0)} SQL write(s) against a baseline of {limit}"
        for path, limit in sorted(allowed.items())
        if current.get(path, 0) < limit
    ]
    if "--write-baseline" in sys.argv:
        baseline["maxWrites"] = {
            path: min(limit, current[path])
            for path, limit in sorted(allowed.items())
            if current.get(path, 0) > 0
        }
        BASELINE_PATH.write_text(json.dumps(baseline, indent=2) + "\n", encoding="utf-8")
        print(f"API_SQL_RATCHET=BASELINE_WRITTEN lowered={len(stale)}")
        return 0
    if stale and not failures:
        failures = [
            *stale,
            "lower the baseline with `python3 scripts/api-sql-ratchet.py --write-baseline`",
        ]

    if failures:
        for failure in failures:
            print(f"API_SQL_RATCHET=FAIL {failure}", file=sys.stderr)
        print(
            "API_SQL_RATCHET=FAIL "
            "see docs/ARCHITECTURE.md: SQL belongs in infrastructure or adapters",
            file=sys.stderr,
        )
        return 1

    print(f"API_SQL_RATCHET=PASS files={len(current)} writes={sum(current.values())}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
