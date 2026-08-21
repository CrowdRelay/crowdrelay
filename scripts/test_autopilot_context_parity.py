#!/usr/bin/env python3
"""Every Autopilot context must be writable, not just readable.

The control overview lists policies straight from the database, so a context
appears in the operator UI whether or not the write path understands it.
`parse_context` answers 404 for anything it does not recognise, so a context
missing there renders as a normal row with a toggle that always fails -- and
the only visible symptom is a "not found" toast.

That is how `show_growth` stayed disabled: it was the one context the API
could not parse, so its policy was never once updated while every other
context advanced past version 1.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def read(rel: str) -> str:
    return (ROOT / rel).read_text(encoding="utf-8")


class AutopilotContextParityContract(unittest.TestCase):
    def contexts(self):
        """(variant, wire string) for every AutopilotContext."""
        model = read("crates/crowdrelay-application/src/autopilot/model.rs")
        block = model.split("pub const fn as_str(self)", 1)[1].split("\n    }", 1)[0]
        pairs = re.findall(r'Self::(\w+)\s*=>\s*"([a-z_]+)"', block)
        self.assertGreaterEqual(len(pairs), 17, "context enum did not parse")
        return pairs

    def test_every_context_can_be_parsed_by_the_write_path(self):
        parser = read("crates/crowdrelay-api/src/autopilot/validation.rs")
        block = parser.split("fn parse_context", 1)[1].split("\n}", 1)[0]
        unwritable = [s for _, s in self.contexts() if f'"{s}"' not in block]
        self.assertEqual(
            unwritable,
            [],
            f"contexts the operator UI shows but cannot update: {unwritable}",
        )

    def test_parser_invents_no_context_the_enum_lacks(self):
        parser = read("crates/crowdrelay-api/src/autopilot/validation.rs")
        block = parser.split("fn parse_context", 1)[1].split("\n}", 1)[0]
        parsed = set(re.findall(r'"([a-z_]+)"\s*=>\s*Some\(', block))
        known = {s for _, s in self.contexts()}
        self.assertEqual(parsed - known, set(), "parser accepts unknown contexts")


if __name__ == "__main__":
    unittest.main()
