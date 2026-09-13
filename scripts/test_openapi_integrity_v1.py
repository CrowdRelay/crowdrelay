#!/usr/bin/env python3
"""The OpenAPI contract must not silently lose definitions.

`openapi/openapi.yaml` is the supported integration boundary. Two defects lived
in it and `validate-contract-assets` reported "validated OpenAPI (320 paths)"
for both, because neither is a schema violation — the document parses fine.

**A duplicated mapping key.** `components.parameters` was defined twice. YAML
keeps the last mapping for a duplicated key, so the first block vanished from the
effective document: `AreaPlayerId`, `ReservationId`, `DrawId` and `WinnerId`
disappeared while 15 `$ref`s still pointed at them. Most parsers accept this
without a word; Redoc was the first thing to say so, and only because somebody
opened the rendered spec in a browser.

**A reference to a response that was never defined.**
`#/components/responses/PublicProblem` appeared twice on a `/public/*` endpoint.
The defined responses are `Problem` and `PrivateProblem`.

Both break a generated client and neither breaks the file. That is exactly the
gap a contract gate is for.
"""
import re
import unittest
from pathlib import Path

try:
    import yaml
except ImportError:  # pragma: no cover - the runner always has it
    yaml = None

ROOT = Path(__file__).resolve().parents[1]
SPEC = ROOT / "openapi/openapi.yaml"


class _DuplicateDetectingLoader(yaml.SafeLoader if yaml else object):
    """A loader that records duplicated mapping keys instead of ignoring them."""


def _load_with_duplicates() -> tuple[dict, list[tuple[str, int, int]]]:
    duplicates: list[tuple[str, int, int]] = []

    def construct_mapping(loader, node, deep=False):
        seen: dict = {}
        for key_node, _ in node.value:
            key = loader.construct_object(key_node, deep=deep)
            line = key_node.start_mark.line + 1
            if key in seen:
                duplicates.append((str(key), line, seen[key]))
            seen[key] = line
        return yaml.SafeLoader.construct_mapping(loader, node, deep)

    _DuplicateDetectingLoader.add_constructor(
        yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, construct_mapping
    )
    document = yaml.load(SPEC.read_text(), Loader=_DuplicateDetectingLoader)
    return document, duplicates


@unittest.skipIf(yaml is None, "PyYAML is required to parse the contract")
class OpenApiIntegrity(unittest.TestCase):
    def setUp(self) -> None:
        self.document, self.duplicates = _load_with_duplicates()

    def test_the_spec_has_no_duplicated_mapping_key(self):
        report = [
            f"'{key}' at line {line} silently replaces the one at line {first}"
            for key, line, first in self.duplicates
        ]
        self.assertEqual(
            report,
            [],
            "a duplicated key removes everything the earlier one defined, and "
            "the file still parses: "
            + "; ".join(report),
        )

    def test_every_component_reference_resolves(self):
        raw = SPEC.read_text()
        components = self.document.get("components") or {}
        dangling = sorted(
            {
                f"#/components/{section}/{name}"
                for section, name in re.findall(r"#/components/(\w+)/(\w+)", raw)
                if name not in (components.get(section) or {})
            }
        )
        self.assertEqual(
            dangling,
            [],
            "these references point at definitions that do not exist, so any "
            f"generated client breaks on them: {dangling}",
        )

    def test_the_parameters_that_went_missing_are_present(self):
        """Named, because their absence was invisible for as long as it lasted."""
        parameters = (self.document.get("components") or {}).get("parameters") or {}
        for name in ("AreaPlayerId", "ReservationId", "DrawId", "WinnerId"):
            self.assertIn(
                name,
                parameters,
                f"{name} is referenced by the spec and must be defined in the "
                "single components.parameters block",
            )

    def test_error_responses_are_not_shared_cacheable(self):
        """Both problem responses must forbid storing.

        `Problem` and `PrivateProblem` carry the same schema and differ only by
        `Cache-Control` — `no-store` against `private, no-store`. Neither is a
        claim about the body, so which one an endpoint uses is a caching
        decision, not a disclosure one. What must hold either way is that an
        error keyed to one requester is never stored by a shared cache.
        """
        responses = (self.document.get("components") or {}).get("responses") or {}
        self.assertTrue(responses, "the contract defines no shared responses")
        for name, body in responses.items():
            header = ((body.get("headers") or {}).get("Cache-Control") or {})
            self.assertIn(
                "NoStore",
                header.get("$ref", ""),
                f"the {name} response must send a no-store Cache-Control",
            )


if __name__ == "__main__":
    unittest.main()
