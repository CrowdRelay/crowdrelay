# FAN_100 — canonical autonomous growth plan

## North Star

CrowdRelay must autonomously find people, convert them into confirmed fans, retain them, and create qualified referrals.

The primary operating target is at least **100 unique, real, confirmed, system-attributed organic fans per month per active tenant** once the tenant has completed the minimum growth setup.

The canonical counting contract is strict:

- baseline/imported/manual/staff/test identities do not count;
- pending or unconfirmed identities do not count;
- unattributed arrivals do not count;
- a fan counts only when canonical provenance ties the acquisition to a real CrowdRelay-owned action and a verified publication/delivery path.

Engineering activity, generated content, actions, posts, clicks, dashboards and PRs are intermediate state, not North Star progress.

## Product standard

CrowdRelay is not a dashboard with automation attached. It is an autonomous growth agent.

A successful tenant should not need to continuously:

- decide what the Brain should do next;
- manually move drafts between systems;
- manually pick channels;
- repeatedly approve work already covered by a standing approval;
- notice that a lane is dead and reroute it;
- tell the product that zero visitors means distribution is broken;
- clean up zombie tasks that no longer have authority;
- interpret proxy metrics and translate them into the next action.

Once the tenant has connected a supported profile and granted the relevant standing authority, the system owns the loop until a genuinely human decision is required.

## Day-0 cold-start contract

The first ten fans are a separate product problem from scaling an already-working loop.

A new tenant starts with little or no first-party outcome history. The Brain therefore cannot rely on historical ROI, large fan cohorts or previously earned channel standing to discover its first audience.

The Day-0 promise is:

> After a tenant connects at least one supported net-new distribution surface, supplies the minimum truthful brand/content context, and explicitly grants standing authority for autonomous publishing on that surface, CrowdRelay must be able to create and execute a safe first acquisition attempt without further babysitting.

The Brain may never invent credentials, consent, platform grants, moderation standing or operator authority. Those are one-time onboarding prerequisites. But after they are present, repeated manual publishing or per-post approval is a product failure.

### Day-0 minimum setup

The product must make these prerequisites explicit and bounded:

1. **Tenant identity and destination**
   - workspace identity/brand;
   - a canonical member/signup destination;
   - tracked-link support.

2. **At least one executable net-new rail**
   - currently, the strongest owned Day-0 candidates are connected Facebook/Instagram surfaces with valid publish credentials and explicit standing auto-post approval;
   - community surfaces may participate only when their own admission, standing and moderation rules allow it;
   - a lane that can only draft for a person is not an autonomous Day-0 rail.

3. **Grounded first content**
   - enough tenant-owned truth to produce the first useful post: current release/show/content asset, approved brand text, existing owned-profile material, or another truthful tenant source;
   - the system must not hallucinate voice, endorsements, anecdotes or artist facts.

4. **Explicit standing authority**
   - connection is not consent to publish;
   - the tenant must grant the relevant standing approval once;
   - after that, the system must not ask again for each post unless policy, reputation risk or a changed scope requires it.

If these are not satisfied, onboarding is incomplete. The product must identify the smallest missing prerequisite. It must not hide the state behind a healthy-looking empty dashboard or create unrelated busywork.

### Net-new reach is not “anything outbound”

While the canonical funnel says `expand_reach`, a route is valuable only if it can plausibly expose the tenant to people who are not already inside the owned audience.

**Net-new acquisition rails:**
- admitted external/community placements;
- public social distribution capable of reaching non-followers;
- explicitly consented amplification/collaboration routes whose authority is current and useful to this tenant.

**Existing-audience delivery:**
- Signal push;
- source email;
- owned Telegram;
- owned Discord;
- other direct channels whose audience is already subscribed/known.

Existing-audience delivery is useful for activation and retention. It must not satisfy the Brain's need for net-new reach.

### Day-0 execution loop

After the minimum setup is complete, the cold-start loop is:

`eligible tenant asset → choose executable net-new rail → create grounded acquisition content → attach action-owned tracked link → publish through standing authority → provider receipt → unique visitor → signup → confirmation → canonical fan acquisition`

The Brain must distinguish every failure point:

- **no executable rail** → fix onboarding/integration/authority, not content scoring;
- **draft but no publication** → repair exact delivery/hold;
- **publication but zero visitors after a mature window** → change distribution/content/target, do not add dashboards;
- **visitors but zero signup** → repair conversion;
- **signup but zero confirmation** → repair confirmation;
- **confirmed but zero activation** → repair activation;
- **activated but no return** → repair retention;
- **healthy retained cohort but no qualified referral** → repair referral/Latarnik mechanics.

A later stage may never distract from an earlier mature zero.

### First-publication acceptance

For a tenant that satisfies the Day-0 prerequisites, the product is not considered autonomously started until:

1. the Brain selects a net-new rail;
2. an action actually reaches the executor;
3. the provider returns a durable publication/delivery receipt;
4. the action owns a tracked acquisition link;
5. the organic funnel observes the publication and opens the traffic window.

A draft, generated asset, queued action or internal executor acknowledgement is not sufficient.

### First-visitor and first-fan acceptance

After a real publication:

- the first mature top-of-funnel decision is based on unique attributed visitors, not impressions alone;
- zero attributed visitors is a distribution/content failure to repair;
- the first confirmed system-attributed human is the first meaningful North Star success;
- only then does the system have real outcome evidence from which to improve selection.

The product should deliberately prove **+1**, then repeat to **10**, before assuming scale logic will reach 100.

## Cold-start content problem

The first grounded starter path is now implemented for current tenant-owned content sources.

When `join_ask_variants` is absent, CrowdRelay may seed exactly one deterministic starter from a fresh active release/video/event title or synced owned-social caption. The source line is preserved verbatim and the system adds only the neutral signup CTA owned by the product. Explicit tenant wording always wins. Blank or overlong source text still fails closed as `NoVariants`; the product does not truncate a claim or invent voice merely to escape the hold.

This closes the copywriter prerequisite for tenants that already supplied real content. The same existing join-ask executor, standing authority, cadence, tracked link, publication receipt and fan-attribution path remain authoritative.

Still-valid future grounding sources include tenant-approved bio/brand copy, press assets and explicit onboarding text supplied once. Those should widen safe coverage only when needed; they are not a reason to delay proving the current source-derived path in production.

## Autonomous rerouting

The Brain must consume real lane state before spending capacity.

- delivering lane → usable;
- quiet lane → one bounded probe;
- queued lane → let existing work drain;
- held/rate-limited/failing lane → route around it;
- platform scope and owned/community scope must never collide.

A blocked rail must not monopolize scarce candidate slots while another executable net-new rail exists.

## Reputation and authority

Cold-start does not justify spam.

Preserve:

- current consent;
- standing approvals;
- platform credentials/capabilities;
- moderator/removal holds;
- cooldowns and daily/contact ceilings;
- human-only relationship boundaries;
- tenant isolation;
- idempotency and duplicate-send protection;
- canonical attribution.

Reddit/community autonomy must remain earned from real human response and survival, not provider counters or bot activity. A fresh community account is not automatically a Day-0 unattended rail.

Peer bands, creators, venues, bookers and promoters are relationship evidence unless explicit applicable amplification authority makes them a real distribution route.

## All connected profiles

“Grow all connected profiles autonomously” does not mean post the same thing everywhere.

Every connected profile must be classified by its real role:

- **net-new discovery**;
- **existing-audience activation/retention**;
- **relationship/partner**;
- **measurement-only / unsupported write path**.

The Brain should use each profile for the stage it can actually move. Unsupported or manual-only profiles must not create a false impression of autonomous reach.

## Hourly implementation loop

Every FAN_100 implementation run must begin by reading:

- `ops/growth/FAN_100_PLAN.md`;
- `ops/growth/north-star-october-2026.md`;
- `ops/growth/README.md`;
- current `main`, open FAN_100 PRs and CI;
- current canonical funnel/lane evidence when available.

Until Day-0 is proven end-to-end, every run must explicitly ask:

1. Can a newly configured tenant produce a real provider-confirmed net-new publication without per-post babysitting?
2. If not, what is the earliest exact blocker?
3. Is the current work removing that blocker, or merely describing it?
4. Would this change plausibly help produce the next real attributed human?
5. Are we accidentally spending acquisition capacity on existing fans, relationship entities, internal work or dead lanes?

If the answer to #3 or #4 is no, choose different work.

Every run ends with either:
- a concrete update to the current open FAN_100 PR; or
- one new coherent FAN_100 PR from current `main`.

No audit-only runs.

## Prioritization order

### Phase 0 — prove Day-0 distribution
- [ ] one supported tenant can complete minimum growth setup without hidden prerequisites;
- [ ] at least one net-new lane is executable under standing authority;
- [x] first grounded acquisition post is generated without requiring per-post copywriting;
- [ ] provider publication receipt is durable and action-linked;
- [ ] tracked link is present and canonical funnel recognizes the publication;
- [ ] dead/manual-only lanes do not consume acquisition capacity.

### Phase 1 — prove +1 organic fan
- [ ] real unique attributed visitor;
- [ ] real signup;
- [ ] confirmation;
- [ ] canonical fan acquisition row;
- [ ] no staff/manual/test/baseline pollution.

### Phase 2 — repeat to 10
- [ ] measured zero causes a changed action rather than repeated spam;
- [ ] lane/content/target selection learns only from real outcome evidence;
- [ ] confirmation and activation recovery execute without restarting acquisition unnecessarily;
- [ ] reputation holds and contact ceilings remain intact.

### Phase 3 — scale 10 → 100/month
- [ ] allocate more capacity to proven net-new rails;
- [ ] maintain exploration without starving proven routes;
- [ ] activate and retain acquisition cohorts;
- [ ] introduce qualified referral/Latarnik multiplication;
- [ ] cut mature zero-yield lanes.

## Definition of done

The FAN_100 system is not done when it can explain why growth is blocked.

It is done when, for a correctly onboarded tenant, it repeatedly and autonomously converts:

`tenant truth + standing authority → net-new publication → real visitor → confirmed attributed fan → activation → retention → qualified referral`

with measured rerouting when any stage fails, and without requiring an operator to continuously drive the loop.
