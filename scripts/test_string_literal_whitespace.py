r"""Regression guard: no mangled whitespace runs inside Rust string literals.

A squash once flattened `\` line continuations inside string literals,
leaving runs of 6-22 spaces mid-sentence ("what          everyone's
listening to"). Those strings ship to fans and operators — the run is not
formatting, it is a bug in output text.

This test fails on any single-line, non-raw string literal under
`crates/*/src` whose content matches the sweep regex:

    [A-Za-z.,;:—!?)]\s{6,}[A-Za-z(—]

Skipped: raw strings (`r"..."`, `r#"..."#`), `//` line comments, and
literals that span more than one line — a multi-line literal is allowed to
hold intentional block indentation.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# The sweep regex, applied to the literal's source text between quotes.
WHITESPACE_RUN = re.compile(r"[A-Za-z.,;:—!?)]\s{6,}[A-Za-z(—]")

# A single-line string literal: opening quote, no unescaped quote, closing
# quote — all on one line. Escapes are handled so `"a\"b"` stays one match.
LINE_LITERAL = re.compile(r'"(?:[^"\\]|\\.)*"')

# Raw strings, single-line only (multi-line raws are skipped by construction
# anyway — they cannot match LINE_LITERAL's same-line requirement either,
# but the opening line could otherwise contribute a bare `"` fragment).
# The `r` must open a literal — the lookbehind keeps the `r` at the end of
# `scanner"` or `or"` from being read as a raw-string prefix, which would
# otherwise mask `r" | "` and splice two literals into one fake offender.
RAW_LITERAL = re.compile(
    r'(?<![A-Za-z0-9_])r(#{0,})"(?:[^"\\]|\\.)*"\1'
)


def literals_on(line: str) -> list[tuple[int, str]]:
    """(column, source) of every normal string literal on one line.

    Raw strings are masked out first so `r"  spaced  "` is not read as a
    normal literal. A `//` that appears outside any literal ends the
    scannable part of the line, so comments cannot trip the guard.
    """
    masked = RAW_LITERAL.sub(lambda match: " " * len(match.group(0)), line)

    literal_spans = [match.span() for match in LINE_LITERAL.finditer(masked)]

    def inside_literal(position: int) -> bool:
        return any(start <= position < end for start, end in literal_spans)

    cut = len(masked)
    for match in re.finditer(r"//", masked):
        if not inside_literal(match.start()):
            cut = match.start()
            break

    return [
        (match.start(), match.group(0))
        for match in LINE_LITERAL.finditer(masked[:cut])
    ]


def offenders() -> list[str]:
    found: list[str] = []
    for source in sorted(REPO_ROOT.glob("crates/*/src/**/*.rs")):
        for number, line in enumerate(source.read_text().splitlines(), start=1):
            for _column, literal in literals_on(line):
                if WHITESPACE_RUN.search(literal):
                    found.append(f"{source.relative_to(REPO_ROOT)}:{number}: {literal[:80]}")
    return found


class StringLiteralWhitespaceTest(unittest.TestCase):
    def test_no_whitespace_runs_in_rust_string_literals(self) -> None:
        found = offenders()
        self.assertEqual(
            [],
            found,
            "mangled whitespace run(s) inside string literals:\n" + "\n".join(found),
        )


if __name__ == "__main__":
    unittest.main()
