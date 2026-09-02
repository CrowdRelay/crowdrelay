#!/usr/bin/env python3
"""A compare-and-set claim must read whether it won.

Job claims are guarded by pinning the prior status:

    UPDATE viryaos_beacon_invite_delivery_jobs
    SET status='claimed', claim_token_hash=$3, ...
    WHERE workspace_id=$1 AND id=$2 AND status='queued'

That `AND status='queued'` is the entire race guard, and its result lives in
`rows_affected()`. The handler checked only for an error, so two workers
claiming the same job both received HTTP 200 and a claim token: the winner's
UPDATE matched a row, the loser's matched nothing, and neither was told.

The loser then delivered the same invites — beacons receive them twice — and
found out it had never held the claim only when its report failed token
validation, long after the mail had gone. A silent lost race in a claim is an
outbound-duplicate bug, not a bookkeeping one.

This checks the guard is still read where the race is real: the two beacon
invite-delivery job endpoints an external executor competes for.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INTERNAL = ROOT / "crates/crowdrelay-api/src/beacon_signal/network/internal.rs"


class ClaimRaceGuard(unittest.TestCase):
    def setUp(self) -> None:
        self.source = INTERNAL.read_text()

    def test_the_invite_job_claim_reads_rows_affected(self) -> None:
        self.assertIn(
            "rows_affected()",
            self.source,
            "the invite-delivery job claim no longer reads whether its "
            "compare-and-set matched. Two workers would both be told they won",
        )

    def test_a_lost_claim_is_refused_not_answered_with_a_token(self) -> None:
        self.assertRegex(
            self.source,
            r"if claimed == 0 \{",
            "nothing handles the zero-rows case, so a worker that lost the "
            "race still receives a claim token and delivers the invites",
        )
        # The refusal has to precede the response that hands out a token.
        lost = self.source.find("if claimed == 0 {")
        token_response = self.source.find("InviteJobClaimResponse")
        self.assertNotEqual(lost, -1, "lost-race branch not found")
        self.assertNotEqual(token_response, -1, "claim response not found")
        self.assertLess(
            lost,
            token_response,
            "the lost-race check runs after the claim token is returned, "
            "which is the same as not checking",
        )

    def test_the_claim_still_pins_the_prior_status(self) -> None:
        """Without this predicate there is no race guard to read."""
        self.assertRegex(
            self.source,
            r"UPDATE viryaos_beacon_invite_delivery_jobs[\s\S]{0,400}?AND status='queued'",
            "the claim no longer pins status='queued', so it would overwrite "
            "a claim another worker already holds",
        )

    def test_the_report_path_still_verifies_the_claim_token(self) -> None:
        """The second half of the same guarantee."""
        self.assertIn(
            "BeaconSignalError::Unauthorized",
            self.source,
            "the report endpoint no longer rejects a mismatched claim token, "
            "so a worker that lost the race could still close the job",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        print("CLAIM_RACE_GUARD=PASS")
    else:
        print("CLAIM_RACE_GUARD=FAIL")
        sys.exit(1)
