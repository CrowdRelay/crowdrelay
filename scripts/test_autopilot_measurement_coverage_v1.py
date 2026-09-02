#!/usr/bin/env python3
"""An action the brain takes often enough to matter must be measured.

`plan_measurements` in `crowdrelay-infra/src/autopilot/execution.rs` matches
every `AutopilotActionPayload` variant exhaustively, which sounds like full
coverage and is not: 26 of them shared a single `=> {}` arm that schedules
nothing. Production shows what that costs.

    action_kind                  succeeded   measured
    team.assignment.email               53          0
    content.artifact.request            20          0
    fan.lifecycle.message.request        9          0
    agent.run.request                    9          5
    signal.push.request                  4          0
    audience.campaign.request            4          4

108 succeeded actions, 9 measured. The brain acts and learns from almost
nothing, and no test failed, because an empty arm is indistinguishable from a
considered decision that this action has no observable outcome.

Some of those really have none — a team assignment email lands in a human's
inbox and the brain cannot see what happens next. The point is not to demand a
measurement for every variant. It is that adding a variant to the silent
bundle should be a deliberate act, so the list is pinned here: extending it
fails this test and asks for a reason.
"""
from __future__ import annotations

import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXECUTION = ROOT / "crates/crowdrelay-infra/src/autopilot/execution.rs"

# Variants that schedule no measurement, by deliberate choice. Adding to this
# list means asserting the brain cannot observe the outcome of that action.
KNOWN_UNMEASURED = {
    "AcceptLiveOpportunityTerms",
    "AdjustExperiment",
    "ApplyLiveOpportunity",
    "ChangeTicketCapacity",
    "CompleteShowTask",
    "CounterLiveOpportunityTerms",
    "EscalateEditorialPitch",
    "EscalateShowTask",
    "ExecuteReleaseMilestone",
    "IssueReferralCode",
    "PrepareFundingPackage",
    "RaiseGrowthDebt",
    "RaiseGrowthOpportunity",
    "RequestAgentContent",
    "RequestBeaconDiscovery",
    "RequestBeaconInviteBatch",
    "RequestBeaconOutreach",
    "RequestBookingTargetDiscovery",
    "RequestContentArtifact",
    "RequestFanLifecycleMessage",
    "RequestMerchBundle",
    "RequestMerchReorder",
    "RequestOutreachDiscovery",
    "RunPlayStep",
    "SendTeamAssignmentEmail",
    "SubmitFundingApplication",
    "VerifyPlaylistPlacement",
}

# Variants that must keep scheduling one. Each of these is an action whose
# whole purpose is a change the brain can observe; losing its measurement
# would silently reopen the learning gap this test exists to close.
MUST_BE_MEASURED = {
    "RequestSignalPush",
    "RequestAgentRun",
    "RequestAudienceCampaign",
    "RequestCommunityEngagement",
}


def planner_block() -> str:
    source = EXECUTION.read_text()
    start = source.find("let mut plans")
    end = source.find("for (kind, subject_id, baseline_value, due_at) in plans")
    if start == -1 or end == -1 or end <= start:
        raise AssertionError("measurement planner block not found in execution.rs")
    return source[start:end]


def arms() -> dict[str, int]:
    """Maps each payload variant to how many measurements its arm schedules."""
    block = planner_block()
    found: dict[str, int] = {}
    for arm in re.split(r"\n        (?=AutopilotActionPayload::)", block)[1:]:
        head = arm.split("=>")[0]
        pushes = arm.count("plans.push(")
        for name in re.findall(r"AutopilotActionPayload::(\w+)", head):
            found[name] = pushes
    return found


class MeasurementCoverage(unittest.TestCase):
    def test_the_planner_is_still_readable(self) -> None:
        self.assertTrue(arms(), "no payload arms found; the planner has moved")

    def test_actions_with_observable_outcomes_are_measured(self) -> None:
        found = arms()
        for variant in sorted(MUST_BE_MEASURED):
            self.assertIn(variant, found, f"{variant} is no longer handled")
            self.assertGreater(
                found[variant],
                0,
                f"{variant} schedules no measurement. Its entire purpose is a "
                f"change the brain can observe, so without one the brain keeps "
                f"taking this action and never learns whether it works",
            )

    def test_the_silent_bundle_does_not_grow_unnoticed(self) -> None:
        unmeasured = {name for name, pushes in arms().items() if pushes == 0}
        added = unmeasured - KNOWN_UNMEASURED
        self.assertEqual(
            added,
            set(),
            f"these payloads schedule no measurement and are not in the "
            f"reviewed list: {sorted(added)}. An empty arm looks identical to "
            f"a considered decision, which is how 26 variants accumulated one. "
            f"Either schedule a measurement or add it to KNOWN_UNMEASURED with "
            f"a reason the outcome cannot be observed",
        )

    def test_the_reviewed_list_does_not_rot(self) -> None:
        """A measured variant left in the list makes the list a lie."""
        found = arms()
        stale = {
            name
            for name in KNOWN_UNMEASURED
            if name in found and found[name] > 0
        }
        self.assertEqual(
            stale,
            set(),
            f"{sorted(stale)} now schedule measurements but are still listed as "
            f"unmeasured; remove them so the list keeps meaning something",
        )


if __name__ == "__main__":
    result = unittest.main(exit=False, verbosity=0).result
    if result.wasSuccessful():
        measured = sum(1 for pushes in arms().values() if pushes > 0)
        print(f"AUTOPILOT_MEASUREMENT_COVERAGE=PASS measured_variants={measured}")
    else:
        print("AUTOPILOT_MEASUREMENT_COVERAGE=FAIL")
        sys.exit(1)
