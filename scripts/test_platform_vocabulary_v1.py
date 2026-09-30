#!/usr/bin/env python3
"""Pin the platform vocabulary to one source of truth per surface.

Migrations 0202-0206 each retyped the same CHECK constraint by hand to add a
single platform. The fifth one dropped 'meta' and 'google_ads', which would
have aborted the migration on any deployment holding a Meta connection —
ADD CONSTRAINT validates existing rows. Nothing caught it, because the Rust
enum that was supposed to mirror the constraint had itself drifted six values
behind.

So this compares the two, in both directions:

  fanbase_connections_platform_check          <-> domain::fanbase::Platform::ALL
  growth_metric_series_platform_check <-> domain::growth_metrics::MetricPlatform::ALL

The latest ALTER wins, so the check reads the effective constraint the way
Postgres would after a full migrate. A value present in the database but
missing from the enum means storage the code cannot parse; a value present in
the enum but missing from the database means an insert that will be rejected
at runtime. Both fail.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MIGRATIONS = ROOT / "migrations"

CONSTRAINTS = {
    "fanbase_connections_platform_check": (
        "crates/crowdrelay-domain/src/fanbase.rs",
        "Platform",
    ),
    "growth_metric_series_platform_check": (
        "crates/crowdrelay-domain/src/growth_metrics.rs",
        "MetricPlatform",
    ),
    # An objective judges a metric series, so it takes the same alphabet; the
    # constraint was born inline in 0092 and is re-typed from 0387 on.
    "growth_objectives_platform_check": (
        "crates/crowdrelay-domain/src/growth_metrics.rs",
        "MetricPlatform",
    ),
}

# Matches both spellings the migrations use:
#   CHECK (platform IN ('a', 'b'))
#   CHECK (platform = ANY (ARRAY['a', 'b']))
ADD_CONSTRAINT = re.compile(
    r"ADD\s+CONSTRAINT\s+(?P<name>\w+)\s+CHECK\s*\(\s*platform\s*"
    r"(?:IN|=\s*ANY)\s*\(\s*(?:ARRAY\s*)?\[?(?P<values>[^\])]*)",
    re.IGNORECASE,
)
# `ALTER TABLE x RENAME CONSTRAINT a TO b` — 0346 renamed the whole schema's
# constraints in one pass; without this the gate would compare a snapshot the
# live database no longer carries.
RENAME_CONSTRAINT = re.compile(
    r"RENAME\s+CONSTRAINT\s+(?P<old>\w+)\s+TO\s+(?P<new>\w+)",
    re.IGNORECASE,
)
QUOTED = re.compile(r"'([^']+)'")
# `Self::Merch => "merch",` and `Platform::Meta,` style entries.
AS_STR_ARM = re.compile(r"Self::(\w+)\s*=>\s*\"([^\"]+)\"")


def _constraint_renames() -> dict[str, str]:
    """`RENAME CONSTRAINT a TO b` as `a -> b`, in migration order.

    0346 renamed the whole schema's constraints in one pass; without following
    the chain the gate compares a snapshot under a name the live database no
    longer carries.
    """
    renames: dict[str, str] = {}
    for path in sorted(MIGRATIONS.glob("[0-9][0-9][0-9][0-9]_*.sql")):
        for match in RENAME_CONSTRAINT.finditer(path.read_text(encoding="utf-8")):
            renames[match.group("old").lower()] = match.group("new").lower()
    return renames


def _resolve(renames: dict[str, str], name: str) -> str:
    """A constraint's identity is its current name — follow renames forward."""
    seen: set[str] = set()
    while name in renames and name not in seen:
        seen.add(name)
        name = renames[name]
    return name


def effective_constraint(name: str) -> set[str] | None:
    """Values the constraint holds after every migration has been applied."""
    renames = _constraint_renames()
    wanted = _resolve(renames, name.lower())
    latest: set[str] | None = None
    for path in sorted(MIGRATIONS.glob("[0-9][0-9][0-9][0-9]_*.sql")):
        for match in ADD_CONSTRAINT.finditer(path.read_text(encoding="utf-8")):
            if _resolve(renames, match.group("name").lower()) == wanted:
                latest = set(QUOTED.findall(match.group("values")))
    return latest


def enum_storage_keys(rel: str, enum: str) -> set[str]:
    """Storage keys from the enum's `as_str`, which every lookup derives from."""
    source = (ROOT / rel).read_text(encoding="utf-8")
    start = source.index(f"impl {enum} {{")
    body = source[start : source.index("\n}\n", start)]
    arm_start = body.index("fn as_str")
    arm_body = body[arm_start : body.index("\n    }", arm_start)]
    return {value for _, value in AS_STR_ARM.findall(arm_body)}


class PlatformVocabularyV1(unittest.TestCase):
    def test_every_constraint_is_parsed(self) -> None:
        for name in CONSTRAINTS:
            self.assertIsNotNone(
                effective_constraint(name),
                f"no ADD CONSTRAINT found for {name}; the regex or the "
                f"migration spelling changed",
            )

    def test_constraint_and_enum_agree(self) -> None:
        for name, (rel, enum) in CONSTRAINTS.items():
            stored = effective_constraint(name)
            assert stored is not None
            declared = enum_storage_keys(rel, enum)
            self.assertEqual(
                stored - declared,
                set(),
                f"{name} accepts values {enum} cannot parse — add the variant "
                f"or drop it from the migration",
            )
            self.assertEqual(
                declared - stored,
                set(),
                f"{enum} declares values {name} would reject — add them in a "
                f"migration before the code writes them",
            )

    def test_migrations_never_narrow_the_connection_vocabulary(self) -> None:
        """A CHECK may gain values; losing one aborts the migration.

        ADD CONSTRAINT validates existing rows, so removing a value that a
        deployment still stores fails the migration and blocks the deploy.
        Dropping a platform is a data migration, not a constraint edit.
        """
        renames = _constraint_renames()
        for name in CONSTRAINTS:
            wanted = _resolve(renames, name.lower())
            seen: set[str] = set()
            for path in sorted(MIGRATIONS.glob("[0-9][0-9][0-9][0-9]_*.sql")):
                for match in ADD_CONSTRAINT.finditer(path.read_text(encoding="utf-8")):
                    if _resolve(renames, match.group("name").lower()) != wanted:
                        continue
                    values = set(QUOTED.findall(match.group("values")))
                    self.assertEqual(
                        seen - values,
                        set(),
                        f"{path.name} drops {sorted(seen - values)} from {name}; "
                        f"ADD CONSTRAINT validates existing rows, so this aborts "
                        f"the migration wherever such a row exists",
                    )
                    seen = values


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        counts = {
            name: len(effective_constraint(name) or ()) for name in CONSTRAINTS
        }
        summary = " ".join(f"{k.split('_platform')[0]}={v}" for k, v in counts.items())
        print(f"PLATFORM_VOCABULARY_V1=PASS {summary}")
    else:
        print("PLATFORM_VOCABULARY_V1=FAIL")
        sys.exit(1)
