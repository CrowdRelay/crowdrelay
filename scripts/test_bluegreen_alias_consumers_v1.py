#!/usr/bin/env python3
"""Long-lived consumers of the API must use the colour-independent alias.

`compose.bluegreen.yaml` publishes two aliases per colour:

    green: [crowdrelay-api-green, crowdrelay-api-active]
    blue:  [crowdrelay-api,       crowdrelay-api-active]

So `crowdrelay-api` is not a neutral service name — it is the *blue* alias
(`BLUE_ALIAS` in `scripts/deploy-bluegreen.sh`). Anything pointing at it keeps
working until the first deploy flips to green, and then resolves nothing.

That is not hypothetical. The Rekor proof anchor was configured with
`CROWDRELAY_INTERNAL_URL=http://crowdrelay-api:8080`. It ran 797 consecutive
failing health checks over three days, unable to reach the API, while Rekor
itself was fine — and `test_rekor_inventory_v12.py` asserted the broken value,
so CI stayed green the whole time. Prometheus scraped the same blue alias,
which meant `up{job="crowdrelay-api"} == 0` fired on every deploy instead of on
real outages: the monitoring that should have caught it was blind for the same
reason.

Only files that run against the blue-green stack are governed.
`deploy/reverse-proxy/*.example` are examples for `compose.production.yaml`,
which does publish a real `crowdrelay-api` alias, and `deploy-bluegreen.sh` and
`crowdrelayctl` name colours deliberately because orchestrating them is their
job.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

ACTIVE = "crowdrelay-api-active"

# Consumers that outlive a single deploy and must survive a colour flip.
GOVERNED = (
    "deploy/observability/prometheus.yml",
    "deploy/rekor-anchor.env.example",
    "proofs/rekor-anchor/relayer/.env.example",
)

# A colour-specific alias followed by the API port, not already part of a
# longer alias such as `crowdrelay-api-active`.
COLOUR_ALIAS = re.compile(r"\bcrowdrelay-api(?:-green)?:8080\b")


class BlueGreenAliasConsumers(unittest.TestCase):
    def test_governed_files_exist(self) -> None:
        for name in GOVERNED:
            self.assertTrue((ROOT / name).is_file(), f"{name} is missing")

    def test_blue_alias_is_still_what_we_think_it_is(self) -> None:
        """If the deploy stops calling blue `crowdrelay-api`, revisit this."""
        deploy = (ROOT / "scripts/deploy-bluegreen.sh").read_text()
        self.assertIn('BLUE_ALIAS="crowdrelay-api"', deploy)
        self.assertIn(f'ACTIVE_ALIAS="{ACTIVE}"', deploy)

    def test_no_governed_consumer_points_at_a_colour(self) -> None:
        offenders = []
        for name in GOVERNED:
            for number, line in enumerate((ROOT / name).read_text().splitlines(), 1):
                stripped = line.strip()
                if stripped.startswith("#") or stripped.startswith("//"):
                    continue
                if COLOUR_ALIAS.search(line):
                    offenders.append(f"{name}:{number}: {stripped}")
        self.assertEqual(
            offenders,
            [],
            "these point at a colour-specific alias and will resolve nothing "
            f"after a blue-green flip; use {ACTIVE}: " + "; ".join(offenders),
        )

    def test_each_governed_consumer_names_the_active_alias(self) -> None:
        """Catches a consumer that drops the API reference altogether."""
        for name in GOVERNED:
            self.assertIn(
                ACTIVE,
                (ROOT / name).read_text(),
                f"{name} no longer targets {ACTIVE}",
            )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print(f"BLUEGREEN_ALIAS_CONSUMERS=PASS files={len(GOVERNED)}")
    else:
        print("BLUEGREEN_ALIAS_CONSUMERS=FAIL")
        sys.exit(1)
