#!/usr/bin/env python3
"""Every platform a draft may declare must be one some executor claims.

`SocialPostItem.platform` in crowdrelay-agents is a six-value enum shared by four
templates, and the executors here claim by `template_id` — so which platforms are
legal depends on which template produced the draft, and the schema cannot know
that. A draft on a platform nobody claims becomes a succeeded action with no
artifact: work spent, nothing published, and before the measurement fix it was
scored as a real zero.

Measured 2026-09-13, this does not currently happen. Every `social_post` outcome in
production carries a platform its own template's executor claims — `social-post`
with facebook/instagram/x, `community-engager` with reddit, `telegram-poster` with
telegram, `discord-poster` with discord. The roadmap said "the orphan itself still
happens"; for the platform case it does not, and the orphans it saw were
`press-pitch`, which has no executor for an entirely different reason.

So this gate exists for the way it would start happening: somebody adds a value to
the enum — tiktok, bluesky, threads — and no executor claims it. That is a
one-line change in another repository with no failing test anywhere.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
AGENTS = ROOT.parent / "crowdrelay-agents"
STRUCTURED = AGENTS / "src/agent/structured.ts"

WORKER = ROOT / "crates/crowdrelay-worker/src"

# Which executor claims which platforms, and how it selects its work. The social
# executor filters on the draft's platform; the rest claim a whole template and
# accept whatever platform it carries.
SOCIAL_EXECUTOR = WORKER / "social_post_executor.rs"
TEMPLATE_CLAIMED = {
    "telegram": (WORKER / "telegram_executor.rs", "telegram-poster"),
    "discord": (WORKER / "discord_executor.rs", "discord-poster"),
}
# Reddit drafts are community engagement, claimed by the community executor
# through `RequestCommunityEngagement` rather than by platform.
COMMUNITY_PLATFORM = "reddit"


def schema_platforms() -> set[str]:
    source = STRUCTURED.read_text()
    match = re.search(r"platform: z\.enum\(\[([^\]]+)\]\)", source)
    if match is None:
        raise AssertionError("SocialPostItem.platform enum not found")
    return set(re.findall(r'"([a-z0-9-]+)"', match.group(1)))


def social_executor_platforms() -> set[str]:
    source = SOCIAL_EXECUTOR.read_text()
    match = re.search(
        r"payload->'draft'->>'platform' IN \(([^)]+)\)", source
    )
    if match is None:
        raise AssertionError("social executor platform predicate not found")
    return set(re.findall(r"'([a-z0-9-]+)'", match.group(1)))


class AgentsPlatformParity(unittest.TestCase):
    def setUp(self):
        if not STRUCTURED.is_file():
            self.skipTest(
                "crowdrelay-agents is not checked out beside this repository; "
                "the pairing is checked on the build host, which has both"
            )

    def test_every_declarable_platform_has_an_executor(self):
        claimed = social_executor_platforms() | set(TEMPLATE_CLAIMED)
        claimed.add(COMMUNITY_PLATFORM)
        unclaimed = schema_platforms() - claimed
        self.assertEqual(
            unclaimed,
            set(),
            f"a draft may declare {sorted(unclaimed)} and no executor claims it. "
            f"Either add an executor, or narrow the enum in "
            f"crowdrelay-agents/src/agent/structured.ts — a draft nobody publishes "
            f"is a succeeded action that reached nobody.",
        )

    def test_the_template_claimed_executors_still_claim_their_templates(self):
        # The map above asserts telegram and discord are covered. That is only
        # true while those executors still select on those template ids.
        for platform, (path, template_id) in TEMPLATE_CLAIMED.items():
            source = path.read_text()
            self.assertIn(
                f"t.template_id = '{template_id}'",
                source,
                f"{path.name} no longer claims '{template_id}', so {platform} "
                f"drafts are unclaimed",
            )

    def test_the_social_executor_claims_at_least_the_open_web_platforms(self):
        # Narrowing this predicate silently orphans whatever it drops.
        self.assertEqual(
            social_executor_platforms(),
            {"instagram", "facebook", "x"},
            "the social executor's claimed platforms changed; anything dropped is "
            "now an orphaned draft",
        )

    def test_the_enum_is_not_silently_empty(self):
        # If the regex stops matching, every assertion above passes vacuously.
        self.assertGreaterEqual(len(schema_platforms()), 6)


if __name__ == "__main__":
    unittest.main()
