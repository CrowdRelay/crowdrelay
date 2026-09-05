#!/usr/bin/env python3
"""CLAUDE.md is loaded into every session; wrong facts there cause wrong work.

The file is a map of a 100k-line workspace that no one reads end to end, and
nothing checked it, so it drifted for months. The damage is not cosmetic:

  * `migrations/ 204 sequential .sql files (next = 0207_*)` while the tree held
    228 files ending at 0230. An agent following that line writes
    `0207_whatever.sql`, which collides with a migration that shipped long ago
    and lands out of order in a directory whose whole contract is sequence.
  * `just ci # ... + runtime-contracts` names a recipe the justfile does not
    have. The command fails, and the reader has to go find the real name.
  * Crate sizes understated by up to 40%, so "start from here" pointed at a
    layout that no longer existed.

So the numbers that can be derived are derived here and compared. Two
tolerances, chosen so this catches rot without failing on every commit:

  * Exact, because a wrong value actively misleads and the correct one changes
    only in the same commit that changes the tree: the migration pointer and
    count, every `just` recipe named, every `scripts/` path named, every
    repository path named.
  * Banded, because these move on ordinary commits and only the order of
    magnitude is load-bearing: crate file and line counts, `routing.rs` length,
    and the per-prefix route counts.

Update CLAUDE.md when this fails. Do not widen a band to get green -- the band
is already wide enough that tripping it means the map is genuinely stale.

`CLAUDE.md` is in `.gitignore`, so it exists in a working tree and never on a CI
runner. Every check therefore skips when the file is absent rather than failing:
a gate that goes red on every CI run because it is checking a file the runner
was never given would be turned off within a day. Where it does run -- a
developer or agent session, which is the entire audience for the file -- it runs
in `just contract-tests` with everything else.
"""
from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CLAUDE_MD = ROOT / "CLAUDE.md"
JUSTFILE = ROOT / "justfile"
ROUTING = ROOT / "crates" / "crowdrelay-api" / "src" / "routing.rs"
MIGRATIONS = ROOT / "migrations"

TEXT = CLAUDE_MD.read_text() if CLAUDE_MD.is_file() else ""


class ClaudeMdTestCase(unittest.TestCase):
    """Base for every check here: skip when the untracked map is not present."""

    def setUp(self) -> None:
        if not CLAUDE_MD.is_file():
            self.skipTest("CLAUDE.md is gitignored and absent (expected on CI runners)")


class MigrationSequence(unittest.TestCase):
    """Not gated on CLAUDE.md: this is an invariant of the tree itself.

    The pointer check below tells a reader which number is free. This says that
    no two migrations ever claimed the same one, which stays true and worth
    asserting on a CI runner that has no CLAUDE.md to read.
    """

    def test_no_two_migrations_share_a_number(self) -> None:
        numbers = [path.name[:4] for path in sorted(MIGRATIONS.glob("*.sql"))]
        self.assertTrue(numbers, "no migrations found; the glob rotted")
        duplicates = sorted({n for n in numbers if numbers.count(n) > 1})
        self.assertEqual(duplicates, [], f"duplicate migration numbers: {duplicates}")

    def test_every_migration_is_numbered(self) -> None:
        unnumbered = [
            path.name
            for path in sorted(MIGRATIONS.glob("*.sql"))
            if not re.match(r"^\d{4}_", path.name)
        ]
        self.assertEqual(unnumbered, [], f"migrations without a 4-digit prefix: {unnumbered}")


# How far a counted value may sit from the documented one before the map counts
# as stale. Whichever of the two is larger wins, so small surfaces are not
# tripped by a single added route.
BAND_FRACTION = 0.20
BAND_FLOOR = 3


def within_band(documented: int, actual: int) -> bool:
    allowed = max(BAND_FLOOR, round(documented * BAND_FRACTION))
    return abs(documented - actual) <= allowed


API_SRC = ROOT / "crates" / "crowdrelay-api" / "src"


def registration_files() -> list[Path]:
    """Every file that registers routes, not just the one named `routing.rs`."""
    candidates = sorted(API_SRC.glob("*.rs")) + sorted((API_SRC / "routing").glob("*.rs"))
    return [
        path
        for path in candidates
        if ".route(" in path.read_text(encoding="utf-8", errors="replace")
    ]


def all_routes() -> set[str]:
    routes: set[str] = set()
    for path in registration_files():
        routes |= set(
            re.findall(r'"(/v1/[a-z0-9/_{}-]+)"', path.read_text(encoding="utf-8", errors="replace"))
        )
    return routes


def rust_inventory(crate: str) -> tuple[int, int]:
    """Returns (file count, line count) for a crate's `src` tree."""
    sources = sorted((ROOT / "crates" / crate / "src").rglob("*.rs"))
    lines = sum(
        len(path.read_text(encoding="utf-8", errors="replace").splitlines())
        for path in sources
    )
    return len(sources), lines


class MigrationPointer(ClaudeMdTestCase):
    """The one wrong number that corrupts the tree rather than confusing a reader."""

    def setUp(self) -> None:
        super().setUp()
        self.files = sorted(MIGRATIONS.glob("*.sql"))
        match = re.search(
            r"migrations/\s+(\d+) sequential \.sql files \(next = (\d{4})_\*\)", TEXT
        )
        self.assertIsNotNone(
            match,
            "CLAUDE.md no longer states the migration count and next number in the "
            "form `migrations/ N sequential .sql files (next = NNNN_*)`; that line is "
            "the only thing standing between a new agent and a colliding migration",
        )
        assert match is not None
        self.documented_count = int(match.group(1))
        self.documented_next = match.group(2)

    def test_the_count_is_exact(self) -> None:
        self.assertEqual(
            self.documented_count,
            len(self.files),
            f"CLAUDE.md says {self.documented_count} migrations, tree has {len(self.files)}",
        )

    def test_the_next_number_is_free_and_consecutive(self) -> None:
        highest = max(int(path.name[:4]) for path in self.files)
        expected = f"{highest + 1:04d}"
        self.assertEqual(
            self.documented_next,
            expected,
            f"CLAUDE.md points the next migration at {self.documented_next}_ but the "
            f"highest on disk is {highest:04d}; writing {self.documented_next}_ would "
            f"collide with a migration that already shipped. Expected {expected}_",
        )


class NamedThingsExist(ClaudeMdTestCase):
    """Every command and path the map names has to be real."""

    def test_every_just_recipe_named_exists(self) -> None:
        recipes = set(
            re.findall(r"^@?([a-z][a-z0-9-]*)\s*(?:[a-zA-Z*_ ]*)?:", JUSTFILE.read_text(), re.M)
        )
        self.assertIn("ci", recipes, "justfile parse produced no recipes; the regex rotted")
        for named in sorted(set(re.findall(r"`?just ([a-z][a-z0-9-]*)", TEXT))):
            self.assertIn(
                named,
                recipes,
                f"CLAUDE.md tells the reader to run `just {named}`, which the justfile "
                f"does not define",
            )

    def test_the_documented_ci_composition_matches_the_recipe(self) -> None:
        """`just ci # check + ... + runtime-contracts` named a recipe that was
        renamed to `policy-checks`. The `just <name>` check above cannot see it:
        the stale name sits in the comment describing what `ci` runs, not after
        the word `just`. So compare the documented composition to the recipe's
        real dependency list."""
        documented = re.search(r"just ci\s+# (.+)", TEXT)
        self.assertIsNotNone(documented, "CLAUDE.md no longer documents what `just ci` runs")
        assert documented is not None
        actual = re.search(r"^ci:(.*)$", JUSTFILE.read_text(), re.M)
        self.assertIsNotNone(actual, "the justfile no longer defines a `ci` recipe")
        assert actual is not None
        self.assertEqual(
            [part.strip() for part in documented.group(1).split("+")],
            actual.group(1).split(),
            "CLAUDE.md's description of `just ci` no longer matches the recipe's "
            "dependencies; a reader following it runs a recipe that does not exist",
        )

    def test_every_script_named_exists(self) -> None:
        for named in sorted(set(re.findall(r"scripts/([A-Za-z0-9_.-]+\.(?:py|sh|ts))", TEXT))):
            self.assertTrue(
                (ROOT / "scripts" / named).is_file(),
                f"CLAUDE.md names scripts/{named}, which does not exist",
            )

    def test_every_repository_path_named_exists(self) -> None:
        paths = set(re.findall(r"`(crates/[A-Za-z0-9_./-]+|docs/[A-Za-z0-9_./-]+|openapi/[A-Za-z0-9_./-]+)`", TEXT))
        # Brace expansions name a directory plus a set of files; check the
        # directory and each member rather than the literal string.
        for expansion, members in re.findall(r"`([A-Za-z0-9_./-]+/)\{([a-z_,]+)\}\.rs`", TEXT):
            for member in members.split(","):
                paths.add(f"{expansion}{member}.rs")
        self.assertTrue(paths, "no repository paths found in CLAUDE.md; the regex rotted")
        for path in sorted(paths):
            self.assertTrue(
                (ROOT / path).exists(),
                f"CLAUDE.md names {path}, which does not exist",
            )

    def test_every_workflow_named_exists(self) -> None:
        match = re.search(r"\.github/workflows/\{([a-z,-]+)\}\.yml", TEXT)
        self.assertIsNotNone(match, "CLAUDE.md no longer lists the CI workflows")
        assert match is not None
        for workflow in match.group(1).split(","):
            self.assertTrue(
                (ROOT / ".github" / "workflows" / f"{workflow}.yml").is_file(),
                f"CLAUDE.md names the {workflow} workflow, which does not exist",
            )

    def test_the_policy_script_count_is_exact(self) -> None:
        match = re.search(r"runs (\d+) Python/bash policy scripts", TEXT)
        self.assertIsNotNone(match, "CLAUDE.md no longer states how many policy scripts run")
        assert match is not None
        body: list[str] = []
        collecting = False
        for line in JUSTFILE.read_text().splitlines():
            if line.startswith("@policy-checks:"):
                collecting = True
                continue
            if collecting and line and not line[0].isspace():
                break
            if collecting:
                body.append(line)
        actual = sum(1 for line in body if re.match(r"\s+(python3|bash)\s", line))
        self.assertEqual(
            int(match.group(1)),
            actual,
            f"CLAUDE.md says {match.group(1)} policy scripts, `just policy-checks` runs {actual}",
        )


class LayoutIsRoughlyRight(ClaudeMdTestCase):
    """Banded: the map has to point at the right order of magnitude."""

    def test_crate_inventory(self) -> None:
        rows = re.findall(
            r"^crates/(crowdrelay-[a-z]+)\s+(\d+) files / (\d+)k lines", TEXT, re.M
        )
        self.assertEqual(
            len(rows), 6, "the crate layout block no longer lists all six crates"
        )
        for crate, documented_files, documented_k in rows:
            files, lines = rust_inventory(crate)
            self.assertTrue(
                within_band(int(documented_files), files),
                f"CLAUDE.md says {crate} has {documented_files} files, tree has {files}",
            )
            self.assertTrue(
                within_band(int(documented_k), round(lines / 1000)),
                f"CLAUDE.md says {crate} is {documented_k}k lines, tree has "
                f"{round(lines / 1000)}k",
            )

    def test_routing_file_length(self) -> None:
        match = re.search(r"routing\.rs` \((\d+) lines", TEXT)
        self.assertIsNotNone(match, "CLAUDE.md no longer states the routing.rs length")
        assert match is not None
        actual = len(ROUTING.read_text().splitlines())
        self.assertTrue(
            within_band(int(match.group(1)), actual),
            f"CLAUDE.md says routing.rs is {match.group(1)} lines, it is {actual}",
        )

    def test_route_prefix_counts(self) -> None:
        """Counted across every registration file, not just `routing.rs`.

        This test used to count `routing.rs` alone, which is where the map got
        its numbers and where they went wrong: `routing.rs` holds 283 of 441
        routes, so `/v1/admin` read as 142 rather than 172 and the 118-route
        `/v1/control-plane` surface was absent entirely. A gate that measures
        the same wrong denominator as the document it checks agrees with it
        forever.
        """
        routes = all_routes()
        documented = dict(
            (prefix, int(count))
            for prefix, count in re.findall(r"`?(/v1/[a-z-]+)`?\*{0,2} (\d+)", TEXT)
        )
        self.assertTrue(documented, "CLAUDE.md no longer lists the route prefix counts")
        for prefix, count in sorted(documented.items()):
            actual = sum(1 for route in routes if route.split("/")[:3] == prefix.split("/")[:3])
            self.assertTrue(
                within_band(count, actual),
                f"CLAUDE.md says {prefix} has {count} routes, the API registers {actual}",
            )

    def test_every_operator_endpoint_named_is_registered(self) -> None:
        """The capability map has to point at endpoints that exist.

        It was added because six capabilities were reported missing in one
        session while live in production. A map that rots into naming dead
        endpoints is worse than none: it would send the next reader looking for
        a 404 and confirm the belief it exists to correct.

        Path parameters are compared by shape, since the map writes
        `{trace_id}` where the router writes whatever it named the segment.
        """
        registered = {re.sub(r"\{[a-z_]+\}", "{}", route) for route in all_routes()}
        named = {
            re.sub(r"\{[a-z_]+\}", "{}", match)
            for match in re.findall(r"`(/v1/control-plane/[a-z0-9/_{}-]+)`", TEXT)
        }
        self.assertTrue(named, "CLAUDE.md no longer names any operator endpoint")
        missing = sorted(path for path in named if path not in registered)
        self.assertEqual(
            missing,
            [],
            f"CLAUDE.md names operator endpoints the API does not register: {missing}",
        )

    def test_every_registration_file_is_named(self) -> None:
        """A new router file has to be added to the map before it can hide in it.

        Nine files register routes. Grepping only `routing.rs` for a handler has
        repeatedly produced the conclusion that a live endpoint is unrouted --
        against the ops timeline and against the autopilot dry-run preview, both
        of which are live in production behind auth.
        """
        for path in registration_files():
            self.assertIn(
                path.name,
                TEXT,
                f"{path.relative_to(ROOT)} registers routes and CLAUDE.md does not "
                f"name it, so anyone grepping the documented files will miss them",
            )


if __name__ == "__main__":
    unittest.main()
