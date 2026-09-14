#!/usr/bin/env python3
"""CLAUDE.md's numbers must still describe the tree.

CLAUDE.md is what every session reads first, and it says so: "do not `ls` around,
start from here." Its numbers are therefore load-bearing in a way ordinary
documentation is not — an agent uses them to decide whether to go and look.

Every one of them had drifted. Routes 452 against 453; `routing.rs` 286 against
287, and 985 lines against 991; `/v1/public` 35 against 36; the workspace-scope
ratchet's 227 tables against 232; and all six crate file counts, one by nine
files. Nothing was broken by any of it. The cost is subtler and the file states it
itself: grepping only `routing.rs` "makes a live endpoint look unrouted — that
mistake has been made repeatedly", and CLAUDE.md's own route table is what a
reader consults to avoid making it.

**Exact where a wrong number causes a wrong conclusion, tolerant where the number
is orientation.** The route counts decide whether somebody believes an endpoint
exists, so they are exact. Crate file and line counts answer "roughly how big is
this", so a band is checked instead — requiring exactness there would mean every
commit that adds a file must also edit CLAUDE.md, which trains people to edit the
number without reading the sentence around it, and that is how it drifted.

**This gate is local-only, and says so rather than pretending otherwise.**
`CLAUDE.md` is listed in `.gitignore` (line 82), so it is untracked: it does not
exist on a CI runner and every machine carries its own copy. So these checks skip
in CI and run only where the file is, which is also the only place it is read.
That is a smaller guarantee than the other gates here give, and worth naming —
an untracked instruction file means drift on one machine is invisible to every
other, and no gate anywhere can see the copy it is not on.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CLAUDE = ROOT / "CLAUDE.md"
API = ROOT / "crates/crowdrelay-api/src"

ROUTE_PATH = re.compile(r'\.route\(\s*"([^"]+)"')

# The nine files CLAUDE.md says hold every route.
ROUTE_FILES = [
    "routing.rs",
    "control_plane.rs",
    "ops_routes.rs",
    "area_admin.rs",
    "synesthesia.rs",
    "portfolio.rs",
    "audience_graph.rs",
    "community_intelligence_routes.rs",
    "routing/growth.rs",
]

AUTHORITY_PREFIXES = [
    "/v1/admin",
    "/v1/control-plane",
    "/v1/internal",
    "/v1/public",
    "/v1/staff",
    "/v1/me",
    "/v1/beacon",
]

# How far a crate's size may drift before the claim is stale rather than round.
FILE_COUNT_TOLERANCE = 6
LINE_COUNT_TOLERANCE_K = 2


def route_paths(name: str) -> list[str]:
    return ROUTE_PATH.findall((API / name).read_text())


@unittest.skipUnless(
    CLAUDE.exists(), "CLAUDE.md is gitignored, so it is absent on a CI runner"
)
class ClaudeMdCounts(unittest.TestCase):
    def setUp(self) -> None:
        self.doc = CLAUDE.read_text()

    def test_no_tenth_file_registers_a_route(self):
        """The claim is that routes live in these nine and nowhere else."""
        registering = sorted(
            rs.relative_to(API).as_posix()
            for rs in API.rglob("*.rs")
            if ROUTE_PATH.search(rs.read_text())
        )
        self.assertEqual(
            registering,
            sorted(ROUTE_FILES),
            "a file outside CLAUDE.md's nine registers routes, so its route "
            "table no longer accounts for the whole surface — which is exactly "
            "the mistake the surrounding paragraph warns about",
        )

    def test_the_total_route_count_is_exact(self):
        total = sum(len(route_paths(name)) for name in ROUTE_FILES)
        match = re.search(r"\*\*(\d+) routes live in NINE files", self.doc)
        self.assertIsNotNone(match, "CLAUDE.md no longer states a route total")
        self.assertEqual(
            int(match.group(1)),
            total,
            f"CLAUDE.md says {match.group(1)} routes; the nine files register "
            f"{total}",
        )

    def test_the_per_file_route_table_is_exact(self):
        wrong = []
        for name in ROUTE_FILES:
            actual = len(route_paths(name))
            # The table lists `<file>   <count>`, two per line.
            stated = re.search(
                rf"{re.escape(name.split('/')[-1] if '/' not in name else name)}\s+(\d+)",
                self.doc,
            )
            if stated is None:
                wrong.append(f"{name} is missing from the table")
            elif int(stated.group(1)) != actual:
                wrong.append(f"{name}: table says {stated.group(1)}, actual {actual}")
        self.assertEqual(
            wrong,
            [],
            "the route table is what a reader checks before concluding an "
            f"endpoint is unrouted: {wrong}",
        )

    def test_the_authority_prefix_counts_are_exact(self):
        paths = [p for name in ROUTE_FILES for p in route_paths(name)]
        counts = dict.fromkeys(AUTHORITY_PREFIXES, 0)
        for path in paths:
            for prefix in AUTHORITY_PREFIXES:
                if path == prefix or path.startswith(prefix + "/"):
                    counts[prefix] += 1
                    break
        wrong = []
        for prefix, actual in counts.items():
            stated = re.search(rf"`{re.escape(prefix)}`\*{{0,2}} (\d+)", self.doc)
            if stated is None:
                wrong.append(f"{prefix} has no stated count")
            elif int(stated.group(1)) != actual:
                wrong.append(f"{prefix}: says {stated.group(1)}, actual {actual}")
        self.assertEqual(
            wrong,
            [],
            "these are the authority surfaces the file says must never blur "
            f"into each other, so their sizes are worth being right: {wrong}",
        )

    def test_the_next_migration_number_is_right(self):
        migrations = sorted(p.name for p in (ROOT / "migrations").glob("*.sql"))
        self.assertTrue(migrations, "no migrations found")
        highest = max(int(name[:4]) for name in migrations)
        match = re.search(r"(\d+) sequential \.sql files \(next = (\d+)_\*\)", self.doc)
        self.assertIsNotNone(match, "CLAUDE.md no longer states the next migration")
        self.assertEqual(
            int(match.group(1)),
            len(migrations),
            f"CLAUDE.md counts {match.group(1)} migrations; there are "
            f"{len(migrations)}",
        )
        self.assertEqual(
            int(match.group(2)),
            highest + 1,
            f"CLAUDE.md says the next migration is {match.group(2)}; {highest} is "
            "taken, so writing that number would collide and sqlx would refuse "
            "the tree",
        )

    def test_the_workspace_table_count_matches_the_ratchet(self):
        """Asks the ratchet rather than re-deriving it.

        The baseline JSON is a per-file count map and does not record the table
        total, so the number comes from running the ratchet — which is the
        authority on it, and re-implementing its table discovery here would give
        two answers that can disagree, which is the defect this file exists for.
        """
        import subprocess

        result = subprocess.run(
            ["python3", str(ROOT / "scripts/workspace-scope-ratchet.py")],
            capture_output=True,
            text=True,
            cwd=ROOT,
        )
        found = re.search(r"scoped_tables=(\d+)", result.stdout)
        self.assertIsNotNone(
            found,
            "the workspace-scope ratchet no longer prints scoped_tables=N, so "
            f"this check cannot read it: {result.stdout.strip()[:200]}",
        )
        scoped = int(found.group(1))
        match = re.search(r"reading one of the (\d+)\s*\n?\s*`workspace_id`", self.doc)
        self.assertIsNotNone(
            match, "CLAUDE.md no longer states how many tables carry workspace_id"
        )
        self.assertEqual(
            int(match.group(1)),
            scoped,
            f"CLAUDE.md says {match.group(1)} workspace-scoped tables; the "
            f"ratchet counts {scoped}. That column is the whole of tenant "
            "isolation, so the number should not be a guess",
        )

    def test_the_crate_sizes_are_in_the_right_neighbourhood(self):
        """A band, not an equality. See the module docstring."""
        stale = []
        for match in re.finditer(
            r"crates/(crowdrelay-[a-z]+)\s+(\d+) files / (\d+)k lines", self.doc
        ):
            crate, stated_files, stated_k = (
                match.group(1),
                int(match.group(2)),
                int(match.group(3)),
            )
            src = ROOT / "crates" / crate / "src"
            if not src.is_dir():
                stale.append(f"{crate} no longer exists")
                continue
            files = list(src.rglob("*.rs"))
            lines = sum(len(f.read_text().splitlines()) for f in files)
            if abs(len(files) - stated_files) > FILE_COUNT_TOLERANCE:
                stale.append(
                    f"{crate}: says {stated_files} files, actual {len(files)}"
                )
            if abs(lines / 1000 - stated_k) > LINE_COUNT_TOLERANCE_K:
                stale.append(
                    f"{crate}: says {stated_k}k lines, actual {lines / 1000:.1f}k"
                )
        self.assertTrue(stale == [] or stale, "the layout block was not parsed")
        self.assertEqual(
            stale,
            [],
            "these have drifted past rounding, so the layout block is now "
            f"describing a different tree: {stale}",
        )


if __name__ == "__main__":
    unittest.main()
