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

## Checkpoint — funnel recovery becomes executable and causal

The autonomous funnel now owns the downstream repair loop instead of only suppressing more top-of-funnel work.

One mature funnel snapshot governs the entire Autopilot cycle. During a downstream leak (`repair_confirmation`, `activate_fans`, `retain_fans`) Fan Lifecycle gets first access to the scarce owned-audience envelope; Growth Intelligence, join asks and Content Supply see the same directive, so one cycle cannot simultaneously decide "repair activation" and "expand acquisition" from different reads.

Content Supply is narrowed rather than globally stopped:
- existing consented-fan lanes such as source email and Signal push may still run when they are useful to activation/retention;
- fresh public/community fan-out is held while confirmation, activation or retention is the measured limiting stage;
- booking, promoter, representation and other relationship-sensitive work is not reclassified as fan acquisition and keeps its existing authority rules.

`repair_confirmation` now has a bounded executable path:
- only a CrowdRelay-attributed pending fan whose latest canonical double-opt-in delivery is terminally dead/cancelled can qualify;
- delivered or still in-flight confirmation mail is never retried merely because the person did not click;
- the latest consent state is rechecked at execution;
- the public access resend and autonomous retry share one per-fan advisory lock, so the worker cannot invalidate a link the fan just requested;
- execution revalidates that the exact failed event is still latest, rotates the token once, and emits one action-owned canonical `fan.confirmation_requested`;
- at most one autonomous retry is allowed per attributable acquisition episode. A second failure does not become an email loop.

Transactional confirmation recovery is not counted as marketing engagement, activation or a growth experiment. It creates no fake growth evidence/measurement rows and does not impose the ordinary marketing-touch cooldown that would delay the subsequent welcome.

The existing learning-loop readout now carries one causal chain without a parallel metrics table:
`decision-time organic funnel → action → executor receipt/webhook transport receipt → current organic funnel`.
A webhook 2xx remains transport evidence, not a claim of inbox delivery or fan outcome.

Production acceptance for this checkpoint is:
1. a real terminally failed attributed confirmation is retried exactly once;
2. a delivered-but-unconfirmed fan is not retried automatically;
3. after confirmation, Fan Lifecycle can proceed to activation without the auth retry imposing a marketing cooldown;
4. while activation/retention is the mature leak, new community/public acquisition work is observably held before it consumes the cycle ahead of recovery.

Next highest-leverage step: prove this recovery loop against live production cohorts and then close the next measured non-executable stage. If `activate_fans` is the live limiter, verify that welcome/Signal/show-recall receipts create attributable activation; if `retain_fans` is the limiter, verify dormant reactivation is actually eligible and delivered before adding any new retention machinery.

## Checkpoint — scarce fan-out follows the measured funnel stage

The content-supply fan-out now spends outward capacity on work that can move the
stage the canonical organic funnel says is limiting.

- `expand_reach`: public social/community reach remains executable, while
  fresh-drop Signal pushes and source email to already-consented fans are held;
- `repair_conversion` and `repair_confirmation`: both generic public fan-out
  and unrelated existing-fan blasts are held because those stages have dedicated
  recovery paths;
- `activate_fans` and `retain_fans`: public acquisition fan-out is held while
  consented owned-audience delivery remains available.

Internal precursor work remains allowed because it reaches nobody and does not
consume outward touch capacity. This closes a product-level failure mode where the
system correctly diagnosed "we need new people" but could still spend scarce fan
touches telling the existing audience about the same drop.

Production acceptance is practical: during a mature `expand_reach` window, the
next executable Content Supply actions should be routes capable of reaching
non-fans, not Signal/email delivery to people already counted in the audience.

## Checkpoint — cold-start reach means net-new audience, not any outbound channel

The first-ten problem is distribution, not another scoring layer. While the canonical
funnel says `expand_reach`, Content Supply now distinguishes routes that can plausibly
introduce CrowdRelay to a new person from routes that mostly deliver to an audience the
tenant already owns.

- community placements and public social posts remain net-new reach candidates;
- Signal, source email, owned Telegram and owned Discord are existing-audience delivery;
- `expand_reach` does not spend scarce fan-out on those existing-audience routes;
- activation and retention may still use them once the measured bottleneck moves
  downstream.

This does not grant publishing authority. Credentials, standing approvals, moderator
holds, lane health and deployment kill switches remain binding. It only prevents the
Brain from diagnosing "we need new people" and then spending its next action talking to
people already inside an owned channel.

