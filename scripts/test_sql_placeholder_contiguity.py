#!/usr/bin/env python3
"""Reject dynamic sqlx queries whose PostgreSQL placeholders contain gaps.

A gap such as `$1, $3, $4` can leave an unused bind with no inferable SQL type and
only fail when the query is prepared against a real Postgres instance.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
QUERY = re.compile(
    r"sqlx::query(?:_as|_scalar)?(?:\s*::\s*<[^;]+?>)?\s*\(\s*r#\"(?P<sql>.*?)\"#\s*\)",
    re.DOTALL,
)
PLACEHOLDER = re.compile(r"\$(\d+)")


class SqlPlaceholderContiguityTests(unittest.TestCase):
    def test_dynamic_sqlx_postgres_placeholders_are_contiguous(self) -> None:
        problems: list[str] = []
        for path in sorted((ROOT / "crates").glob("**/*.rs")):
            source = path.read_text()
            for match in QUERY.finditer(source):
                numbers = sorted({int(value) for value in PLACEHOLDER.findall(match.group("sql"))})
                if not numbers:
                    continue
                expected = list(range(1, max(numbers) + 1))
                if numbers != expected:
                    line = source.count("\n", 0, match.start()) + 1
                    problems.append(
                        f"{path.relative_to(ROOT)}:{line}: placeholders={numbers}, expected={expected}"
                    )
        self.assertEqual([], problems, "\n".join(problems))


if __name__ == "__main__":
    unittest.main()
