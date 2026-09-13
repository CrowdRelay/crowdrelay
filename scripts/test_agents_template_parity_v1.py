#!/usr/bin/env python3
"""Every template CrowdRelay can dispatch must be one the agent service will run.

`WorkerTemplate` in crowdrelay-domain is what the brain ranks and dispatches. The
agent service's catalog is what it will actually accept — and it filters
`disabled` templates out, so `POST /tasks` answers `404 template 'x' not found`
for one of them. A dispatch against a disabled template is budget spent on a
guaranteed failure.

This has already happened twice. The roadmap recorded seven wasted dispatches on
2026-09-08 and said the two lists "agree today, and only because both were fixed
by hand". They did not agree: measured 2026-09-13, production held **16 failed**
`agent.run.request` actions for `telegram-scanner` — the largest failure count of
any template — and `bandcamp-scanner`, `metal-archives-scanner` and
`telegram-scanner` were all still ranked by `autopilot/cycle/preview` at positions
2, 3 and 4.

Nothing compared them because they live in two repositories. This does.

`KNOWN_DISABLED` is a ratchet, not an exemption list. It may shrink freely — by
enabling a template in the agent service, or by removing it from `WorkerTemplate`
so the brain stops ranking something that cannot run. Adding to it means admitting
a new template the brain will waste budget on, which is a review decision and not
a way to get a build green.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
AGENTS = ROOT.parent / "crowdrelay-agents"
CATALOG = AGENTS / "src/templates/catalog.ts"
TEMPLATE_DIR = AGENTS / "src/templates"
WORKER_TEMPLATE = ROOT / "crates/crowdrelay-domain/src/worker_template.rs"

# Templates CrowdRelay still ranks that the agent service refuses to run.
# Disabled there because they need browsing or scraping tools it does not provide.
# Shrink this. Never grow it.
KNOWN_DISABLED = {
    "bandcamp-scanner",
    "metal-archives-scanner",
    "telegram-scanner",
}


def crowdrelay_slugs() -> set[str]:
    source = WORKER_TEMPLATE.read_text()
    # The slugs are the string literals the enum maps itself to.
    return set(re.findall(r'=> "([a-z0-9-]+)"', source))


def agents_template_files() -> dict[str, Path]:
    found: dict[str, Path] = {}
    for path in sorted(TEMPLATE_DIR.glob("*.ts")):
        match = re.search(r'^\s+id: "([a-z0-9-]+)",', path.read_text(), re.MULTILINE)
        if match:
            found[match.group(1)] = path
    return found


def agents_disabled() -> set[str]:
    return {
        slug
        for slug, path in agents_template_files().items()
        if re.search(r"^\s+disabled: true,", path.read_text(), re.MULTILINE)
    }


class AgentsTemplateParity(unittest.TestCase):
    def setUp(self):
        if not CATALOG.is_file():
            self.skipTest(
                "crowdrelay-agents is not checked out beside this repository; "
                "the pairing is checked on the build host, which has both"
            )

    def test_every_dispatchable_template_exists_in_the_agents_catalog(self):
        missing = crowdrelay_slugs() - set(agents_template_files())
        self.assertEqual(
            missing,
            set(),
            f"CrowdRelay can dispatch templates the agent service has never heard "
            f"of: {sorted(missing)}. POST /tasks answers 404 and the dispatch "
            f"budget is spent on a guaranteed failure.",
        )

    def test_no_new_template_is_disabled_behind_the_brains_back(self):
        disabled_and_dispatchable = crowdrelay_slugs() & agents_disabled()
        self.assertEqual(
            disabled_and_dispatchable,
            KNOWN_DISABLED,
            "the set of templates the brain ranks but the agent service refuses "
            "changed. Shrinking is the goal; growing means new wasted dispatches, "
            "and 16 of them for telegram-scanner is what this gate exists for.",
        )

    def test_the_ratchet_names_only_templates_that_are_really_disabled(self):
        # A stale entry is worse than none: it hides the fact that a template was
        # re-enabled and the brain could be ranking it for real.
        stale = KNOWN_DISABLED - agents_disabled()
        self.assertEqual(
            stale,
            set(),
            f"KNOWN_DISABLED names templates the agent service now accepts: "
            f"{sorted(stale)}. Remove them from the ratchet.",
        )

    def test_the_agents_catalog_is_not_silently_empty(self):
        # If the id regex stops matching, every assertion above passes vacuously.
        self.assertGreaterEqual(len(agents_template_files()), 10)
        self.assertGreaterEqual(len(crowdrelay_slugs()), 10)


if __name__ == "__main__":
    unittest.main()
