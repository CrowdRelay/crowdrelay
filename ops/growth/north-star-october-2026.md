# October outcome: 100 new people acquired by CrowdRelay

Operator statement on 2026-10-02: there are 20 fans; some are band members, and every remaining Signal fan was invited personally. This is a seeded audience, not evidence of organic acquisition by CrowdRelay.

## Target and counting contract

By 2026-10-31, acquire 100 additional unique, real, confirmed fans from CrowdRelay activity. The target is not 100 total records, imports, messages, clicks, requests or installs. Do not count the original 20 again, reactivation of old identities, band/staff/test accounts, personally invited people, pending registrations or unattributed arrivals as progress toward this target. Unknown provenance remains unknown. Staff/manual exclusions must be explicit and durable; do not infer that every tracked click was independent organic discovery.

A credited acquisition needs a canonical deduplicated fan identity, current status/consent, an attributable arrival through an action-owned tracked link, and proof that the originating CrowdRelay publication/authorized placement actually went live. Keep source/action/channel identity. Activation, later retention, qualified referrals and unsubscribes are separate quality outcomes. Welcome activation does not increase the acquired-fan count.

## Change in work philosophy

Start each implementation checkpoint with the measured limiting stage of the existing organic funnel. Pick work that can move that stage. A new abstraction, a successful dispatch or a merged PR is not a growth result. Measure attributable confirmed acquisitions per day and cumulative toward 100, then activation and later return by acquisition cohort. About 3–4 confirmed people per day is the required pace from October 2; achievable traffic and conversion rates must be measured, not assumed.

The next checkpoint should make this goal and its exclusions visible using the existing acquisition/attribution ledgers, then give the autonomous engine a bounded next action for the actual bottleneck: unverified delivery, no audience visits, failed signup/confirmation, or missing fan value. Mature measured zero differs from an immature or uninstrumented window. Stop repeating failing lanes; preserve receipt reconciliation, existing approvals, consent, removal holds, contact ceilings and the booker's ownership. No bulk inviting archives, fabricated engagement or extra contact grants.

A production read after authorized deployment must establish the current baseline and usable publishing lanes. Use a real upcoming show or owned release with a concrete promise and a live tracked placement. Track the full path to confirmed acquisition; fix the largest observed loss before adding more machinery. Do not claim the target is guaranteed by code alone.


## Checkpoint — quality-first fresh-drop audience pockets

The fresh release/video surge no longer chooses its bounded community set by rotation alone. The relay still considers only screened, admitted, currently usable communities and still caps one source-bound spread at three communities, but selection now uses first-party 90-day outcome evidence inside a rested candidate pool:

- two slots prefer communities that previously produced durable fans, then attributed fan conversions, then distinct human interactions;
- one slot is deliberately reserved for a never-attempted community so exploration never disappears; a previously used zero-yield room is measured zero, not exploration;
- if there is not enough measured history, remaining slots fall back to the existing least-recently-drafted rotation.

This changes which audience pockets get the scarce fresh-drop slots without raising send ceilings, bypassing approval/standing rules, repeating a source into the same target, or treating follower/member counts as fan growth. A merged PR is still not progress toward 100; production acceptance is a higher share of delivered fresh-drop placements producing real tracked visitors, confirmed fans and later durable fans.

Next bottleneck after this checkpoint: make the autonomous loop consume the live organic-funnel stage so it expands reach only when traffic is the constraint and switches to conversion/activation recovery when people are already arriving.


## Checkpoint — organic funnel becomes autonomous control

The canonical verified-organic funnel now feeds a typed control signal back into Autopilot instead of ending at the ops readout.

The control is evidence-gated:
- publication must be verified, unambiguous and at least 24 hours old before top-of-funnel silence can steer anything;
- fresh acquisition stages use the last 30 days, so an old successful campaign cannot hide a current zero-traffic or zero-conversion failure;
- activation and retention use mature cohorts up to 90 days, preserving the existing D7/D30 definitions;
- measured zero is distinct from an immature or uninstrumented window.

Behavior changes:
- `no_observed_visitors` narrows Growth Intelligence to attributable new-fan reach and the discovery workers that replenish that reach; real outward actions rank ahead of more research;
- once visitors exist and the leak moves to signup, confirmation, activation or retention, Growth Intelligence stops buying more top-of-funnel and idle exploration is suppressed;
- join asks remain available for reach/conversion repair, but are held once the current leak is confirmation, activation or retention;
- existing Fan Lifecycle, confirmation/outbox recovery and other downstream contexts keep their own consent, cooldown, receipt and capability gates. The funnel signal does not bypass them.

This is a control-plane change, not a growth claim. Production acceptance is behavioral evidence that a mature downstream leak reduces new acquisition dispatches while the relevant downstream recovery lane proceeds, and that zero-visitor periods do the opposite.

Next checkpoint: make the chosen downstream recovery visible as one operator-facing causal trace — funnel directive → action actually dispatched → provider receipt → next funnel movement — and close any stage that still has no executable recovery path.
