#!/usr/bin/env python3
"""Prevent known large source files from silently growing back.

This is a ratchet, not an arbitrary LOC style rule: current large files are
recorded explicitly and may shrink freely. New >1200-line source files fail
until deliberately reviewed and added to the baseline.
"""
from __future__ import annotations
import json
from pathlib import Path

root = Path(__file__).resolve().parents[1]
baseline_path = root / "scripts/source-size-ratchet.json"
baseline = json.loads(baseline_path.read_text())
tracked = {str(k): int(v) for k, v in baseline["maxLines"].items()}
extensions = {".rs", ".ts", ".tsx", ".js", ".jsx", ".astro", ".gd", ".py"}
ignore_parts = {"node_modules", "target", "dist", ".git", ".baseline", "vendor"}
errors: list[str] = []
large: dict[str, int] = {}
for path in root.rglob("*"):
    if not path.is_file() or path.suffix not in extensions or any(part in ignore_parts for part in path.parts):
        continue
    rel = path.relative_to(root).as_posix()
    lines = sum(1 for _ in path.open("r", encoding="utf-8", errors="ignore"))
    if lines > 1200:
        large[rel] = lines
        if rel not in tracked:
            errors.append(f"new large source needs review/baseline: {rel}={lines}")
for rel, maximum in tracked.items():
    path = root / rel
    if not path.exists():
        continue  # a completed extraction/removal is always an improvement
    lines = sum(1 for _ in path.open("r", encoding="utf-8", errors="ignore"))
    if lines > maximum:
        errors.append(f"ratchet exceeded: {rel}={lines} > {maximum}")
if errors:
    print("SOURCE_SIZE_RATCHET=FAIL")
    for error in errors:
        print(f"- {error}")
    raise SystemExit(1)
print(f"SOURCE_SIZE_RATCHET=PASS tracked={len(tracked)} currently_large={len(large)} threshold=1200")
