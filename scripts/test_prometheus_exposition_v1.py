#!/usr/bin/env python3
"""Every metric line must be valid Prometheus exposition format.

A `\\`-continued Rust string literal keeps the source indentation, so

    "# HELP crowdrelay_x ...\\n\\
     # TYPE crowdrelay_x gauge\\n"

emits a line beginning with spaces. A Prometheus comment line has to start with
`#`, and a sample line has to start with the metric name — leading whitespace
makes it neither. That shipped to production and was found by reading the live
scrape, not by any test.

The rule is mechanical: inside a string literal that emits exposition text, no
line may begin with whitespace followed by `#` or by `crowdrelay_`. `concat!` of
separate literals has no continuation and therefore no indentation to leak, which
is what the fix uses.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
API_SRC = ROOT / "crates/crowdrelay-api/src"

# A continued line whose content starts a comment or a sample. The `\` at the end
# of the previous source line is what makes the indentation part of the string.
LEAKED_INDENT = re.compile(r'\\\n\s+(#\s*(?:HELP|TYPE)\b|crowdrelay_[a-z0-9_]+[ {])')


class PrometheusExposition(unittest.TestCase):
    def test_no_metric_line_carries_source_indentation(self):
        offenders = []
        for path in sorted(API_SRC.rglob("*.rs")):
            text = path.read_text()
            for match in LEAKED_INDENT.finditer(text):
                line = text[: match.start()].count("\n") + 2
                offenders.append(
                    f"{path.relative_to(ROOT).as_posix()}:{line} -> "
                    f"{match.group(1).strip()[:60]}"
                )
        self.assertEqual(
            offenders,
            [],
            "these emit a metric or comment line beginning with whitespace, which "
            "is not valid exposition format. Use concat! of separate literals — a "
            "continuation keeps the source indentation:\n  "
            + "\n  ".join(offenders),
        )

    def test_no_comment_line_is_preceded_by_spaces_in_a_literal(self):
        """The baked-in variant of the same fault.

        `cargo fmt` collapses a `\\`-continued literal into one line, turning the
        source indentation into real spaces inside the string. The regex above
        only sees the continuation, so this catches what fmt leaves behind.
        """
        offenders = []
        for path in sorted(API_SRC.rglob("*.rs")):
            text = path.read_text()
            for match in re.finditer(r"\\n +# (?:HELP|TYPE)", text):
                line = text[: match.start()].count("\n") + 1
                offenders.append(f"{path.relative_to(ROOT).as_posix()}:{line}")
        self.assertEqual(
            offenders,
            [],
            "a metric comment is preceded by spaces inside a string literal, so "
            "the emitted line does not start with '#': " + ", ".join(offenders),
        )

    def test_a_histogram_bucket_belongs_to_its_declared_family(self):
        """`<family>_bucket`, or the histogram has no buckets.

        The HTTP duration histogram declared
        `crowdrelay_http_request_duration_seconds` and emitted
        `crowdrelay_http_request_duration_bucket` — missing `_seconds`. Those
        buckets belong to a different, undeclared family, so
        `histogram_quantile` over the declared one returned nothing and p95
        latency was never computable. Valid exposition format, and useless.
        """
        for path in sorted(API_SRC.rglob("*.rs")):
            text = path.read_text()
            families = re.findall(r"# TYPE (crowdrelay_[a-z0-9_]+) histogram", text)
            for family in families:
                buckets = re.findall(r"(crowdrelay_[a-z0-9_]+)_bucket\{", text)
                self.assertIn(
                    family,
                    buckets,
                    f"{family} is declared a histogram in "
                    f"{path.relative_to(ROOT).as_posix()} and has no "
                    f"{family}_bucket series; found buckets for {sorted(set(buckets))}",
                )

    def test_every_help_has_a_type_beside_it(self):
        """A HELP without a TYPE is accepted by Prometheus and useless in a
        dashboard: the metric renders untyped."""
        for path in sorted(API_SRC.rglob("*.rs")):
            text = path.read_text()
            helps = set(re.findall(r"#\s*HELP (crowdrelay_[a-z0-9_]+)", text))
            types = set(re.findall(r"#\s*TYPE (crowdrelay_[a-z0-9_]+)", text))
            missing = sorted(helps - types)
            self.assertEqual(
                missing,
                [],
                f"{path.relative_to(ROOT).as_posix()} documents these metrics "
                f"without declaring a type: {missing}",
            )


if __name__ == "__main__":
    unittest.main()
