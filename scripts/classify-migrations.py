#!/usr/bin/env python3
"""Classify pending SQLx migrations as expand-safe or contract-unsafe.

An *expand* migration only adds schema elements (tables, columns, indexes,
constraints, reference data). The previous binary works against the expanded
schema, so rolling back the image is safe without restoring the database.

A *contract* migration removes or reshapes schema elements (DROP TABLE, DROP
COLUMN, ALTER COLUMN TYPE, TRUNCATE, RENAME). The previous binary may not work
against the contracted schema, so rolling back requires restoring the database
from a snapshot.

Usage:
    python3 scripts/classify-migrations.py [--remote <ssh-host>] [--repo <path>]

Without --remote, classifies all local migration files (useful for CI).
With --remote, queries the production _sqlx_migrations table to find which
migrations are pending and classifies only those.

Output (JSON on stdout, logs on stderr):
    {
      "expand": ["0140_add_foo.sql", "0141_add_bar.sql"],
      "contract": ["0142_drop_legacy.sql"],
      "all_expand": false,
      "pending_count": 3
    }

Exit code:
    0  — all pending migrations are expand-safe
    1  — at least one pending migration is contract-unsafe
    2  — error (missing files, SSH failure, etc.)
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIGRATIONS_DIR = ROOT / "migrations"

# Patterns that indicate a contract (destructive) migration.
# These are checked case-insensitively against non-comment lines.
CONTRACT_PATTERNS = [
    re.compile(r"\bDROP\s+TABLE\b", re.IGNORECASE),
    re.compile(r"\bDROP\s+COLUMN\b", re.IGNORECASE),
    re.compile(r"\bDROP\s+INDEX\b", re.IGNORECASE),
    re.compile(r"\bDROP\s+FUNCTION\b", re.IGNORECASE),
    re.compile(r"\bDROP\s+VIEW\b", re.IGNORECASE),
    re.compile(r"\bDROP\s+TYPE\b", re.IGNORECASE),
    re.compile(r"\bDROP\s+EXTENSION\b", re.IGNORECASE),
    re.compile(r"\bDROP\s+SCHEMA\b", re.IGNORECASE),
    re.compile(r"\bALTER\s+TABLE\b.*\bDROP\s+CONSTRAINT\b", re.IGNORECASE),
    re.compile(r"\bALTER\s+TABLE\b.*\bDROP\b", re.IGNORECASE),
    re.compile(r"\bALTER\s+TABLE\b.*\bRENAME\b", re.IGNORECASE),
    re.compile(r"\bALTER\s+TABLE\b.*\bTYPE\b", re.IGNORECASE),
    re.compile(r"\bALTER\s+TABLE\b.*\bUSING\b", re.IGNORECASE),
    re.compile(r"\bALTER\s+TABLE\b.*\bSET\s+NOT\s+NULL\b", re.IGNORECASE),
    re.compile(r"\bTRUNCATE\b", re.IGNORECASE),
    re.compile(r"\bRENAME\s+TABLE\b", re.IGNORECASE),
    re.compile(r"\bRENAME\s+COLUMN\b", re.IGNORECASE),
]

# Patterns that are definitely expand-safe.
EXPAND_PATTERNS = [
    re.compile(r"\bCREATE\s+TABLE\b", re.IGNORECASE),
    re.compile(r"\bCREATE\s+INDEX\b", re.IGNORECASE),
    re.compile(r"\bCREATE\s+UNIQUE\s+INDEX\b", re.IGNORECASE),
    re.compile(r"\bALTER\s+TABLE\b.*\bADD\s+COLUMN\b", re.IGNORECASE),
    re.compile(r"\bALTER\s+TABLE\b.*\bADD\s+CONSTRAINT\b", re.IGNORECASE),
    re.compile(r"\bCREATE\s+EXTENSION\b", re.IGNORECASE),
    re.compile(r"\bCREATE\s+FUNCTION\b", re.IGNORECASE),
    re.compile(r"\bCREATE\s+TYPE\b", re.IGNORECASE),
    re.compile(r"\bCREATE\s+VIEW\b", re.IGNORECASE),
    re.compile(r"\bINSERT\s+INTO\b", re.IGNORECASE),
    re.compile(r"\bUPDATE\b.*\bSET\b", re.IGNORECASE),
]


def strip_comments(sql: str) -> str:
    """Remove SQL line comments (-- ...) and block comments (/* ... */)."""
    lines = []
    for line in sql.splitlines():
        # Remove -- comments (but not -- inside strings, which is rare in migrations)
        comment_idx = line.find("--")
        if comment_idx != -1:
            line = line[:comment_idx]
        lines.append(line)
    text = "\n".join(lines)
    # Remove block comments
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.DOTALL)
    return text


def classify_migration(filepath: Path) -> str:
    """Classify a single migration file as 'expand' or 'contract'.

    Conservative: if any contract pattern matches, it's contract.
    If no contract pattern matches, it's expand (even if we can't identify
    a specific expand pattern — empty or unusual migrations default to expand).
    """
    sql = filepath.read_text()
    clean = strip_comments(sql)

    for pattern in CONTRACT_PATTERNS:
        if pattern.search(clean):
            return "contract"

    return "expand"


def get_applied_migrations(remote: str, repo: str) -> set[str]:
    """Query the production _sqlx_migrations table for applied migration versions."""
    result = subprocess.run(
        [
            "ssh", "-T", remote,
            f"docker exec crowdrelay-db psql -U crowdrelay -d crowdrelay "
            f"-t -A -c \"SELECT version FROM _sqlx_migrations ORDER BY version;\""
        ],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode != 0:
        print(f"ERROR: failed to query applied migrations: {result.stderr}", file=sys.stderr)
        sys.exit(2)

    return {line.strip() for line in result.stdout.splitlines() if line.strip()}


def main() -> None:
    parser = argparse.ArgumentParser(description="Classify pending SQLx migrations")
    parser.add_argument("--remote", default=None, help="SSH host to query for applied migrations")
    parser.add_argument("--repo", default=str(ROOT), help="Repo root path")
    args = parser.parse_args()

    repo = Path(args.repo)
    migrations_dir = repo / "migrations"
    if not migrations_dir.is_dir():
        print(f"ERROR: migrations directory not found: {migrations_dir}", file=sys.stderr)
        sys.exit(2)

    all_migrations = sorted(migrations_dir.glob("*.sql"))
    if not all_migrations:
        print(json.dumps({"expand": [], "contract": [], "all_expand": True, "pending_count": 0}))
        return

    # Determine which migrations are pending
    if args.remote:
        applied_raw = get_applied_migrations(args.remote, args.repo)
        # SQLx stores versions as integers (1, 2, 3...) but filenames use
        # zero-padded prefixes (0001, 0002, 0003...). Normalize to integers.
        applied = {str(int(v)) for v in applied_raw if v.isdigit()}
        pending = []
        for m in all_migrations:
            version = m.stem.split("_")[0]
            if str(int(version)) not in applied:
                pending.append(m)
        print(f"Found {len(pending)} pending migrations (out of {len(all_migrations)} total)", file=sys.stderr)
    else:
        # Without remote access, classify all migrations
        pending = all_migrations
        print(f"Classifying all {len(all_migrations)} migrations (no --remote, assuming none applied)", file=sys.stderr)

    expand = []
    contract = []
    for m in pending:
        classification = classify_migration(m)
        if classification == "contract":
            contract.append(m.name)
            print(f"  CONTRACT: {m.name}", file=sys.stderr)
        else:
            expand.append(m.name)
            print(f"  expand:   {m.name}", file=sys.stderr)

    result = {
        "expand": expand,
        "contract": contract,
        "all_expand": len(contract) == 0,
        "pending_count": len(pending),
    }
    print(json.dumps(result, indent=2))

    if contract:
        sys.exit(1)


if __name__ == "__main__":
    main()
