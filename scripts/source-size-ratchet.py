#!/usr/bin/env python3
"""Prevent known large source files from silently growing back.

This is a ratchet, not an arbitrary LOC style rule: current large files are
recorded explicitly and may shrink freely. New >1200-line source files fail
until deliberately reviewed and added to the baseline.

Each baseline carries an `allowanceLines` overhead. Without it the ratchet
tripped on a handful of lines, so fixing a bug in a tracked file meant either
deleting the comment explaining the fix or editing the baseline in the same
commit — and a baseline edited by reflex stops being a review signal at all.
The allowance absorbs ordinary maintenance; anything past it is real growth and
still has to be argued for.

It fails downward too. An entry left at a file's old size is headroom nobody
reviewed: in September 2026, 31 of 64 entries named files already under the
threshold and five more sat below their record: 11,928 lines those files could
regrow without anyone being asked. So a tracked file that is gone, back
under the threshold, or more than the allowance below its record fails until
`--write-baseline` lowers the record. That flag only lowers or drops entries; a
new large file is still added by hand, in review.
"""
from __future__ import annotations
import json
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[1]
baseline_path = root / "scripts/source-size-ratchet.json"
baseline = json.loads(baseline_path.read_text())
tracked = {str(k): int(v) for k, v in baseline["maxLines"].items()}
allowance = int(baseline.get("allowanceLines", 0))
extensions = {".rs", ".ts", ".tsx", ".js", ".jsx", ".astro", ".gd", ".py"}
ignore_parts = {"node_modules", "target", "dist", ".git", ".baseline", "vendor", ".venv", ".claude", ".worktrees"}
errors: list[str] = []
large: dict[str, int] = {}
for path in root.rglob("*"):
    if not path.is_file() or path.suffix not in extensions:
        continue
    # Check the path relative to root: the checkout itself may live under a
    # `.worktrees/` directory, and matching the absolute parts would skip
    # every file in it — a ratchet that cannot see the code cannot fail.
    rel_path = path.relative_to(root)
    if any(part in ignore_parts for part in rel_path.parts):
        continue
    rel = rel_path.as_posix()
    lines = sum(1 for _ in path.open("r", encoding="utf-8", errors="ignore"))
    if lines > 1200:
        large[rel] = lines
        if rel not in tracked:
            errors.append(f"new large source needs review/baseline: {rel}={lines}")
lowered: dict[str, int] = {}
stale: list[str] = []
for rel, maximum in tracked.items():
    path = root / rel
    lines = (
        sum(1 for _ in path.open("r", encoding="utf-8", errors="ignore"))
        if path.exists()
        else 0
    )
    if lines > maximum + allowance:
        errors.append(
            f"ratchet exceeded: {rel}={lines} > {maximum}+{allowance} allowance"
        )
    if lines > 1200:
        lowered[rel] = min(maximum, lines)
    if lines <= 1200:
        stale.append(f"baseline entry no longer needed: {rel}={lines} (record {maximum})")
    elif lines + allowance < maximum:
        stale.append(f"baseline record too high: {rel}={lines} (record {maximum})")
if "--write-baseline" in sys.argv:
    baseline["maxLines"] = dict(sorted(lowered.items()))
    baseline_path.write_text(json.dumps(baseline, indent=2) + "\n")
    print(f"SOURCE_SIZE_RATCHET=BASELINE_WRITTEN tracked={len(lowered)} dropped_or_lowered={len(stale)}")
    raise SystemExit(0)
if stale:
    errors.extend(stale)
    errors.append(
        "lower the records with `python3 scripts/source-size-ratchet.py --write-baseline`"
    )
if errors:
    print("SOURCE_SIZE_RATCHET=FAIL")
    for error in errors:
        print(f"- {error}")
    raise SystemExit(1)
print(
    f"SOURCE_SIZE_RATCHET=PASS tracked={len(tracked)} currently_large={len(large)} "
    f"threshold=1200 allowance={allowance}"
)
