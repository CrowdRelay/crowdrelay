# CrowdRelay n8n executor contract

n8n is an executor, not the policy engine. CrowdRelay remains authoritative for decisions, quotas, idempotency, domain state and audit. Provider adapters should stay thin: validate the canonical event, perform one external side effect, then report what the provider actually did.

## Heartbeat

Each active blue/green instance posts `/v1/internal/autopilot/executors/heartbeat` using the commerce key. Use a 60–120 minute expiry and refresh well before expiry. Include the workflow-manifest SHA and **only capabilities whose route and provider adapter are actually live**. Do not hand-build this payload: use `scripts/build_n8n_executor_heartbeat.py` or `scripts/publish-n8n-heartbeat.sh` so the capability list is derived from the production manifest and the attestation SHA/timestamp/manifest binding are copied from the exact secretless attestation.

`team.email` is additionally fail-closed at the API boundary: a heartbeat advertising it is rejected unless `metadata.workflow_attestation_sha` is a SHA-256, `metadata.workflow_attestation_manifest_sha` equals the heartbeat manifest SHA, and `metadata.workflow_attested_at` is at most 14 days old. A successful heartbeat updates the `n8n` production component in the release ledger. Same-manifest heartbeats preserve attestation evidence; a manifest change clears old evidence automatically. Once the first executor registers, CrowdRelay permanently fails closed for missing/expired capabilities rather than silently returning to legacy mode.

Capabilities: `fan.lifecycle.message`, `merch.reorder`, `booking.outreach`, `merch.bundle`, `outreach.send`, `outreach.discovery`, `beacon.invite_batch`, `beacon.outreach`, `beacon.release.mail`, `beacon.network.discovery`, `beacon.network.invite`, `latarnik.invite`, `show.growth`, `content.artifact`, `show.escalation`, `ops.alert`, `promotion.budget`, `opportunity.application`, `opportunity.terms`, `funding.package`, `funding.submit`, `calendar.upsert`, `team.email`, `play.step`, `play.step.third_party`, `playlist.verify`, `booking.discovery`, `agent.content`, `agent.content.press_pitch`, `community.engage`, `representation.approach`, `gig.outreach`, `booking_agent.approach`.

The last three were missing from this list while CrowdRelay routed by them, and that omission is what the `agent.content_requested` refusals were: an executor registering from this list could not advertise a capability it does not mention, so the events reached a webhook that had never been told about them and answered 422. `scripts/test_executor_capability_parity_v1.py` now compares this list against `executor_capability_for_event`, so the two cannot drift again.

Do not advertise a capability merely because code for it exists in an export. The workflow must be active, reachable from the verified ingress/claim bridge and have a working provider credential/configuration.

### Verified subworkflow ingress

Every workflow entered through `n8n-nodes-base.executeWorkflowTrigger` must declare `parameters.inputSource = "passthrough"`. The verified ingress has already authenticated and normalized the canonical event; the subworkflow's first job is to receive that event unchanged. An empty trigger configuration is not an equivalent shorthand on the deployed n8n version: it is rejected as an incomplete node and the executor never reaches its validation, claim, provider action or outcome report.

`scripts/test_n8n_execute_workflow_trigger_contract_v1.py` scans every JSON workflow under `n8n/examples` and fails when a subworkflow trigger loses this setting. New executors therefore inherit the same ingress contract as the existing working outreach executors instead of rediscovering the production failure one workflow at a time.

### Attendance-growth execution

`show.growth` is deliberately an **infrastructure-growth capability**, not a second promotion policy engine. CrowdRelay chooses the event, lever, timing and safety contract. This generic capability handles deterministic free/owned setup such as listing distribution and audience-capture surfaces.

Partner and scene outreach execute through `crowdrelay.beacon.outreach_requested`, which names a concrete verified `beacon_id` and carries suppression, consent/relevance and relationship-phase checks. Stale generic relationship actions must also be refused.

Relationship-sensitive promotion does **not** use this generic route. Venue, promoter, creator, bill-mate and scene-partner contact must travel through a named-target CrowdRelay action (for example `RequestBeaconOutreach` with an exact `beacon_id`, or the booking/outreach loops with an exact target). The executor never chooses a relationship destination from a generic `partner_cross_promo`, `grassroots_scene_relay` or `social_proof_relay` intent.

Every `crowdrelay.show_growth.requested` payload also carries `acts` — the announced bill in play order as `{slug, name}[]`, empty when no bill was declared. The generic show-growth executor may use those facts only for deterministic listing/capture work. They are context, not permission to contact the named acts. Cross-promotion with a bill-mate is represented by that act's named Beacon/booking action instead.

For `beacon.discovery`, prioritize the supplied `priority_source_classes`: local metal media/podcasts, independent radio/music programmes, venue/promoter/support-band networks, record stores/rehearsal studios/music shops, tattoo/alternative-fashion/scene businesses, student/culture portals, moderated metal communities/forums and local live creators/photographers/reviewers. A generic local business is not a Scene Partner without public evidence of real scene relevance. Never scrape private member lists or personal contact data; community candidates should include public rules or a moderator contact when available.

The payload also carries `seed_entities` — real entities the event already names and the sweep must resolve *first*, before open scouting: non-sibling acts on the bill (`kind: "scene_partner"`, `role: "bill_mate"`) and the host venue (`kind: "venue"`, `role: "host_venue"`). Each seed names a public entity — resolve its public contact/channel and ingest it as a Beacon candidate of the supplied kind, with the seed's `evidence` as the provenance note. Seeds are identities, not instructions to write copy: the same `require_verifiable_contact` and `deduplicate_before_upsert` rules apply, and a seed that resolves to nothing reportable is returned under `skipped_with_reason`, never invented. The seed list is deduplicated by name and capped at 24 bill-mates plus the venue — a festival bill beyond the cap keeps its earliest-billed names.

For `free_listing_sweep`, treat the supplied `surface_classes` as a verification checklist rather than a demand to create duplicates. Check VIRYA's canonical show page, Bandsintown + canonical ticket link, Songkick Tourbox + canonical ticket link when artist access exists, Spotify live-event visibility (direct ticket-partner propagation or Bandsintown), the venue's free calendar/newsletter, and relevant free city/culture/scene calendars discovered for that market. Return successful public URLs. If a destination requires CAPTCHA, login, email verification or another unsupported human-only step, do not automate around it: return `metadata.manual_steps[]` with `destination`, `url`, `what_to_do` and `why_it_matters` so the operator has an actionable handoff. Also return checked/skipped surfaces with a reason so “success” cannot mean “we tried one site”.

For `audience_capture_setup`, keep Signal primary on VIRYA-owned surfaces while checking the provider-native Bandsintown Smart Link/Follow/Signup/Widget/QR capture surfaces. Also return a campaign-attributed VIRYA Signal/show QR manual step for the merch table, current shows or explicitly permitted partner surfaces when useful; the QR must preserve the normal consent flow and free-growth authority never buys printing/placement. Configure a presale signup only when a real presale exists; otherwise use a supported reminder only when truthful. Do not import Signal contacts into Bandsintown as part of this action. Account-login/2FA/CAPTCHA configuration becomes `manual_steps`.

For `free_fan_channel_push`, use only provider-native free reach already earned by VIRYA: location/RSVP-targeted Bandsintown Posts, Bandsintown Email Builder only while the provider-confirmed free quota is sufficient, a strong current live/Featured Video when that free surface is available, a Spotify Artist Pick operator step for the event, a YouTube artist Post to existing subscribers when Posts are available, and a Bandcamp Community message to existing followers when that audience exists. YouTube/Bandcamp account-level posting is a human step unless an official supported API exists; never scrape or import contacts just to broaden those audiences. Never invoke Bandsintown Boost, Promoted Email/Posts or another paid placement under this capability. If an official provider API is unavailable for an account-level action, return a human step rather than browser automation. For free-listing work, verify linked artist identities and downstream distribution health (including Bandsintown and Songkick partner graphs) instead of treating one successful source submission as proof that every downstream surface is live. Community/group promotion is manual or moderator-approved only: no automated cold posting, scraping or rule bypass.

It must not purchase placement, change ticket pricing, fabricate reviews/crowd/stream numbers, broaden the recipient list, or contact an unverified discovered candidate. Partner cross-promo should make one concrete authorized ask at a time. When no verified social proof exists, use the supplied local story/context instead of inventing proof. Gemini may only adapt wording/format from the supplied facts. First-party fan campaigns (ambassadors, high-intent last mile, merch pre-show/post-show) are scheduled inside CrowdRelay and therefore do not require this n8n capability.

### Play steps

`play.step` carries `crowdrelay.play.step_requested`: one step of one running campaign, for **one consented fan**, with the fan's contact details, the show it is about and a `template_key` naming the message. It is emitted per recipient on purpose — that is what makes the send idempotent, keeps the daily quota and the weekly owned-audience envelope meaningful, and bounds a play that goes wrong to one message rather than a segment.

The capability splits on the step's own class. The in-process executor claims `play.step` and serves the owned-audience and first-party kinds itself: fan-facing steps write `fan_push_deliveries` in the action's own transaction, the listing sweep and the pre-save check record what they found on the step row. One kind reaches outside the workspace — `release_curator_wave`, the curator outreach — and it parks behind `play.step.third_party`, which nothing in-process advertises on purpose: claiming the unsplit name would let a curator pitch ride a push delivery that cannot send it.

For those owned-audience steps the emit is the audit, not the delivery — the push rows are the send. The external lane for a play step is email: `crowdrelay.play.step_requested` for a `step_kind` an executor serves carries the fan's contact details and the named template, and the executor renders and sends it. `autopilot-play-step-mail.example.json` is that lane. It must not broaden the recipient list, substitute a different template, re-send a step it has already reported, or send at all once the event is past — a step delivered outside its moment is a different, worse message than one not delivered, and CrowdRelay records that omission as a skip rather than letting it arrive late.

`play.step.third_party` carries the same event for the step kinds that contact someone outside the consented audience — the curator wave. An executor claiming it gets a third-party send, not a fan message: the payload names the release and the wave rather than a consented fan, and the send is to a contact the workspace verified, never to a harvested address.

### Event campaigns (communication.campaign_due)

`communication.campaign_due` carries a campaign reference, not the copy:
`campaign_id`, `campaign_slug`, `channel`, `segment_id`, `template_key`. The
words live on the campaign row — `communication_campaigns.content` carries
`subject` and `body`, composed in CrowdRelay and approved by the operator
verbatim.

The executor expands `segment_id` into the consented fans it may write to,
reads `content.subject`/`content.body`, and sends them unchanged. It does not
render a template, substitute wording, or add paragraphs — the band approved
the sentences; a send is not a second draft. `template_key` still travels for
the ledger and for rows written before the copy moved in-repo; a campaign
whose `content` carries `subject`/`body` is sent verbatim, and one without
them may fall back to the key only while its copy predates this contract.

| `template_key` | When it is sent |
| --- | --- |
| `event.announcement.v1` | The show is published and the city has not heard yet. |
| `event.interest_reminder.v1` | Fans said they are coming and have not bought a ticket. |
| `event.last_call.v1` | Same audience, inside the last-call window before doors. |
| `event.day_of.v1` | Ticket buyers, on the day. |
| `event.thank_you.v1` | Fans who were in the room, minus anyone whose scan already got the welcome. |

### Fan lifecycle messages

`crowdrelay.fan_lifecycle.message_requested` carries one message for one
consented fan, with the fan's contact details and a `template_key` naming
which message it is. CrowdRelay has already decided that this fan should hear
from us, that consent is current, and that the marketing cooldown allows it.

The payload also carries `brand.wordmark`, resolved by CrowdRelay from the
workspace's own brand identity, and the fan's locale. Render the tenant's name
from `brand.wordmark`; never compile a band name into the executor. When a
referral URL is present, send `fan.referral_url` verbatim — hostname and route
construction belong to CrowdRelay. The same holds for `fan.install_url` on the
install ask: it is a tracked redirect the ledger counts, so send it verbatim or
the ask teaches nothing.
The executor renders the named template and sends it.

Eight keys exist. An executor that handles some by name and lets the rest fall
through to a default is the failure this list exists to prevent: three of these
were being rendered as the Synesthesia follow-up, including the one that thanks
somebody for a referral that converted — the payoff of the only compounding
loop the product has.

| `template_key` | When it is sent |
| --- | --- |
| `crowdrelay.fan.welcome.v1` | First contact after signup. |
| `crowdrelay.synesthesia.follow_up.v1` | Completed Synesthesia and has not bought a ticket. |
| `crowdrelay.fan.reactivation.v1` | Gone quiet past the dormancy window. |
| `crowdrelay.fan.first_ticket_thanks.v1` | Bought their first ticket. The single best moment to turn a buyer into a fan. |
| `crowdrelay.fan.returning_thanks.v1` | Bought enough shows to count as a returning fan. |
| `crowdrelay.fan.referral_thanks.v1` | A referral they made converted. |
| `crowdrelay.fan.referral_invite.v1` | Has referred nobody; asks them to. Carries `fan.referral_code` **and the complete tenant-native `fan.referral_url`**. Send `fan.referral_url` verbatim; never construct a hostname or referral path in the executor. |
| `crowdrelay.fan.signal_install_ask.v1` | Confirmed fan with no Signal install (app or identified web session); asks them to open it. Carries **the complete tracked `fan.install_url`** — the tenant's `/l/` redirect to the Signal page. Send it verbatim; a link the executor rebuilds bypasses the click ledger, and a missing `fan.install_url` is a send-stopper, not a detail to work around. Every send of this template waits for a person's approval in CrowdRelay before it reaches the executor. |

**Fail on a key you do not know.** A default branch sends the wrong message,
which is worse than sending none: it is indistinguishable from working, and the
fan reads a thank-you for something they did not do. Throw instead, and the
failed execution says which key arrived.

`scripts/test_lifecycle_template_contract_v1.py` fails when this table and the
keys CrowdRelay can emit disagree, so a new template cannot ship without landing
here first.

### Double opt-in confirmations

`fan.confirmation_requested` is the pending-fan confirmation mail every
import path sends. `event_version` stays `1`; two payload fields were added
and old executors may ignore them:

- `data.locale` — the fan's own locale when the entry carried one, else the
  tenant's crew locale, else `null`. An executor may branch on it for the
  mail's language; `null` means "unmeasured", not "English".
- `data.invitation` — `{ "reason": string|null, "source_label": string }`.
  `reason` is the operator's one line typed at promote time ("we are moving
  the list to Signal — confirm if you want to keep hearing from us") and is
  meant to be printed verbatim in the mail body when present.
  `source_label` is `archive` for addresses recovered from the Drive/Gmail/
  GitHub/CSV/sheet connectors and `web` otherwise, so the executor can pick
  the archive-invitation wording instead of the signup one.

### Crew-mail one-click approvals

`team.assignment.email` payloads — a single-ask notice and the morning
briefing alike — carry signed one-click links when the deployment sets
`CROWDRELAY_PUBLIC_API_ORIGIN`. Unset means the fields are simply absent;
an executor must not synthesize a link itself.

- `data.approve_url` / `data.skip_url` — present when the mail fronts one
  pending approval. Both fields hold the same URL,
  `{api_origin}/v1/public/approvals/{token}`: the link is the credential and
  the verdict rides in the POST body, so the mail renders it as two buttons
  or two named links to the same address. A notice for plain work carries
  neither field.
- `data.pending_approvals` — the briefing's per-ask list,
  `[{ "action_id", "approve_url", "skip_url" }]`, parallel to the asks the
  body enumerates. An ask whose window has no end mints no link and is
  absent from the list — it is decided in the panel, not from a mail that
  cannot lapse.

The links die with the ask's own `approval_expires_at` — an expired one
answers 410 and a forged one 404, so the executor never has to guard them.
GET renders the ask; the verdict only lands on POST, so link previewers
cannot vote.

## Target discovery (candidates, not targets)

Executors post what they found to `POST /v1/internal/autopilot/outreach/candidates` with the commerce key and an `Idempotency-Key`, up to 100 candidates per batch; CrowdRelay screens each one on write. The admin route of the same name stays for operator imports. Discovery is executor work, so it lives on the internal surface: requiring the admin key would hand an adapter authority over every admin route in order to post a list of playlist contacts.

An adapter may sweep unprompted, and it may also be *asked* to. `outreach.discovery` carries `crowdrelay.outreach.discovery_requested`, emitted when the agent has fewer confirmed submission routes than its policy floor. The event carries `requested_candidates`, the screening thresholds a sweep can pre-filter against, and the callback path. The sweep reads published data, contacts nobody and buys nothing, which is why it is `first_party_reversible`.

**Report the batch even when it is empty.** The internal route accepts an empty `candidates` array and the admin route does not, and that difference is the point: CrowdRelay tells a sweep that found nothing admissible — after which it stops asking — from one that was never answered, which stays an operator problem. It reads that from the ingestion itself, not from candidate rows, because both cases leave zero of those. An adapter that returns silently keeps the request alive for ever instead of backing it off.

**Report what the sweep read, in `sweep`.** `sources_read` is how many sources were queried, `items_seen` how many items they returned before any screening. Without them, an adapter that read two hundred playlists and found no published submission route posts exactly the same empty batch as one whose credential expired — and CrowdRelay would call the first a dry source worth widening and the second the same thing, sending an operator to fix the wrong end. The counts are treated as adapter claims: they change only which cause is reported, never authority, a cap or a screening outcome. They are refused as incoherent when `items_seen` is below the number of candidates posted, or above zero with `sources_read` at zero. The field is optional and the admin route rejects it outright — a human posting candidates by hand did not run a sweep.

Read a route, never work one out. `route_is_published` must be `true` only when the route was read verbatim out of text the curator published for that purpose — a submission line in a playlist description, a contact page, a reply. Guessing `firstname@domain` from a name and a website is refused as `route_inferred`, and a candidate refused once is never rediscovered, because the refusal is a stored row. Send the `evidence` snippet the route was read from: without it the candidate is refused as `evidence_missing`, since no human can check the extraction later.

Send the raw signals rather than a verdict. `follower_count`, `engagement_count`, `sells_placement` and `churns_indiscriminately` are inputs to CrowdRelay's screening, not conclusions; `fit_basis_points` is how well the candidate matches this band. Whether a candidate is admitted, and what a pitch through it would cost, is decided in CrowdRelay.

Register submission platforms with `POST /v1/admin/autopilot/outreach/submission-channels` before referencing them by `channel_slug`. The channel's `cost_model` decides the class of every pitch through it: `free` is ordinary third-party contact, `credit` and `fee` are spend and take the paid ceiling however small the amount, and `paid_placement` is refused outright at every autonomy level. An unknown slug is refused rather than defaulted to free.

Nothing is contacted as a result of ingestion. An operator confirms the route through `POST /v1/admin/autopilot/outreach/candidates/{candidate_id}/confirm`, and only then does an email route become an outreach target.

## Off-platform metric feeds

`GET /v1/admin/autopilot/growth-metrics/coverage` reports which of `spotify`, `youtube`, `bandsintown` and `social` the agent can currently see. A platform with no series reads as `missing`, not as quiet.

Feeds are ordinary metric ingestion: declare the series once with `POST /v1/admin/autopilot/growth-metrics/series` (platform, provider-neutral `metric_key`, `expected_interval_hours`, and a `value_tier` that is honest — follower counts are `vanity`), then post absolute observations to `POST /v1/admin/autopilot/growth-metrics/points`. Never post a delta: a delta makes a missed snapshot unrecoverable and double-counts on replay. `expected_interval_hours` is what makes a stopped feed detectable, so declare the interval the adapter actually runs at.

A worked example is `n8n/examples/autopilot-spotify-feed.example.json`: hourly client-credentials read of the artist object, hour-bucketed `captured_at`, a deterministic per-bucket `Idempotency-Key`, and an absent count posted as nothing rather than as zero. It deliberately declares only `spotify/followers` — monthly listeners are not in the documented Web API, and a number whose semantics were never confirmed against a real response gets no series. Bandsintown needs no adapter here: the event sync reads `tracker_count` server-side and feeds `bandsintown/trackers` itself.

## Delivery faults (bounces and spam complaints)

When the sending provider reports a bounce or a "marked as spam" complaint for an outreach message, POST it to `/v1/internal/autopilot/outreach/delivery-faults` with the commerce key and an `Idempotency-Key`. The body carries `fault` (`hard_bounce` | `soft_bounce` | `complaint`), `occurred_at`, the provider's own `provider_reference` when one exists, and **exactly one of** `target_id` or `contact_email`. Providers report addresses, not CrowdRelay's ids, so the email form is what webhook adapters normally use; an unknown address is refused rather than silently dropped, because a complaint about somebody missing from our tables is still a complaint about the sending domain.

A worked example is `n8n/examples/autopilot-delivery-faults.example.json`: one provider-mapping code node translates your ESP's field names into the fixed downstream contract, and the `Idempotency-Key` derives from the provider reference so a retried webhook is a replay.

The reference is what makes a retried webhook a replay instead of a second count: rows are deduplicated on it. CrowdRelay computes rates over a rolling window and closes the workspace's sending ceiling before the next wave when a rate crosses its threshold — so report promptly, because the halt is only ever as fresh as the last report. A hard bounce additionally finishes the address through the suppression that already exists; never keep sending to a target after one.

## Representation approaches

`representation.approach` carries `crowdrelay.representation.approach_requested`: the band has chosen a consented agent or label from their own representation contacts and asked CrowdRelay to broker an approach. CrowdRelay has already decided everything that matters — the target consented with a stated basis, is verified and active, is not on do-not-contact, the band's listing is published, and the monthly approach allowance still has room.

The payload names the target (`target_id`, `display_name`, `kind`), the `contact_email` to send to, the band's published `listing` (act name, genre tags, cities, claims with value/tier/basis, published dates, seeking), an optional `note` the band wrote, and `draft` — the finished `subject` and `body`, composed inside CrowdRelay at request time so the approver read the exact words the target gets. **Send `draft.body` verbatim and refuse a payload whose draft is missing or empty.** The executor composes nothing: the listing inside the payload is evidence the letter already cites, not material for a second letter — and a template rendered after the approval would be a different letter than the one the band approved. The contact email exists only inside this payload: it is never shown to the band, so an approach can never be used to harvest the address. Do not re-send a target, widen the ask beyond representation, or contact the address for any other purpose — the allowance and cooldown live in CrowdRelay. `examples/autopilot-representation-approach-executor.example.json` is the reference handler.

## Booking-agent applications

`booking_agent.approach` carries `crowdrelay.booking_agent.approach_requested`: the band is applying to a screened booking agent for representation — a season's ask, not a pitch for one night. CrowdRelay has already decided everything that matters — the agent is active, not on do-not-contact, the route was confirmed by a person, the season is unspent, and the first-party draw evidence cleared the floor at dispatch time.

The payload names the agent (`agent_id`, `agent_name`, `agency`), the `contact_email` to send to, the `evidence` snapshot the letter argues from (shows played, paid tickets, distinct and repeat buyers, cities reached, the best single night — each null only when the reading could not be taken, never a zero invented for it), an optional `note` the band wrote, and `draft` — the finished `subject` and `body`, composed inside CrowdRelay at request time so the approver read the exact words the agent gets. **Send `draft.body` verbatim and refuse a payload whose draft is missing or empty.** The executor composes nothing: the evidence inside the payload is what the letter already cites, not material for a second letter — and a template rendered after the approval would be a different letter than the one the band approved. The contact email exists only inside this payload: it is never shown to the band, so an application can never be used to harvest the address. Do not re-send an agent, widen the ask beyond representation, or contact the address for any other purpose — the one-letter-per-season rule lives in CrowdRelay. `examples/autopilot-booking-agent-approach-executor.example.json` is the reference handler.

## Booking-agent replies

`booking_agent.approach` also carries `crowdrelay.booking_agent.reply_requested`: the band approved an answer to an agent who wrote back to the season's application. Same transport, same brokered mailbox, same claim/receipt rules as `approach_requested` — the events are separate so an executor that does not know replies skips this one rather than mistaking it for a pitch.

The payload names `action_id`, `agent_id`, `agent_name`, `agency`, `contact_email`, `answers_interaction_id` (the inbound interaction this letter closes), `reply_disposition` (audit only — never quoted), and **`draft`, the reply `subject` and `body` composed in CrowdRelay when the draft was requested**. **Send `draft.body` verbatim and refuse a payload whose draft is missing or empty.** The draft is deliberately a scaffold — the ledger holds the reply's filed disposition, not its words — so nothing downstream may extend it into a guess at what they said; the operator completes it on the approval card before it ever reaches an executor. `Re:` subjects are how the operator sees the difference at a glance, so they must not be rewritten away.

## Latarnik invitations

`latarnik.invite` carries `crowdrelay.latarnik.invite_requested`: one letter to one person the band already works with, asking whether they also want the dates before they are public. It was bundled onto `beacon.outreach` on the premise that a beacon-pitch transport exists to share — none does, no pitch branch was ever built, and keeping the mapping let an unbuilt branch hold a ready letter hostage. The split also reads more honestly: this executor sends an approved draft verbatim, while a pitch executor would have to compose from the personalization contract — not the identical thing the bundling assumed.

The payload names `action_id`, `beacon_id`, `recipient_name`, a one-entry `recipients` array with the address resolved inside the sending lock, the `reason` the letter opens with, and `draft` — the finished `subject` and `body`. **Send `draft.body` verbatim and refuse a payload whose draft is missing or empty.** The executor composes nothing: the whole value of this letter is that a person read it before it went, and a template rendered after the approval would be a different letter to somebody who knows the band personally. `examples/autopilot-latarnik-invite-executor.example.json` is the reference handler.

Upstream refuses far more often than it sends. Nobody cold is ever asked, nobody is asked twice, nobody who unsubscribed is approached again through their other role, and nobody is asked inside three weeks of any other contact. An executor receiving one of these can assume every one of those checks passed inside the sending transaction.

## Gig outreach

`gig.outreach` carries `crowdrelay.gig.outreach_requested`: the band approved a gig proposal for one room, and the proposal names everybody who books it. CrowdRelay has already decided everything that matters — the proposal was approved on its stated reasons, every recipient's version was re-pinned inside the sending transaction, and the contact governor reserved every window or the whole letter stayed unwritten. The executor's job is narrower than a send: it is *one letter to all of them*, because the alternative — two promoters comparing notes on a night the third never heard about — is the failure the all-or-none reservation exists to prevent.

The payload names `action_id`, `city_id`, the `venue`, the `template_key`, the `recipients` in ranked order — each with `target_id`, `target_name`, `target_kind` and the `contact_email` resolved inside the lock — and **`draft`, the finished letter: `subject` and `body`, exactly as the operator approved them**. `opening_line` and `reasons` still travel for the receipt and the ledger, but the executor composes nothing from them.

This is the contract's sharpest rule and it changed on 2026-09-18. The executor used to build the letter itself — greeting, the band's self-description, the bullet list, the closing ask, the signature — so a band approved one sentence and a promoter read five paragraphs nobody at the band had seen. Composition now happens in CrowdRelay at approval time. **An executor must send `draft.body` verbatim and must refuse a payload whose draft is missing or empty rather than writing one.** It sends a single message whose recipient set is the full list. It must not split the letter into per-promoter sends, drop a recipient it judges secondary, invent evidence the reasons do not state, or contact any address for another purpose. A claim disposition other than `claimed` (`in_flight`, `ambiguous`) fails closed; `already_succeeded` is a no-op replay. The receipt carries the full recipient set in metadata so the ledger records who the letter actually reached. `examples/autopilot-gig-outreach-executor.example.json` is the reference handler.

Two letters share the event and the capability. `gig.proposal.v1` is the band asking a room for a night that does not exist yet. `support.slot.ask.v1` is the headliner's own workspace asking its promoter to confirm a named labelmate for a slot already on the bill — the payload additionally carries `letter` (`support_slot_ask` with the `support_act`, `event_id` and `show_date` the approval was made on) and the same fields flattened for the template. The ask confirms a name for a place the promoter offered; a handler that renders it as a booking proposal announces a night that is already held, so the two templates are not interchangeable and the key must be dispatched on, not defaulted.

## Booking outreach

`booking.outreach` carries `crowdrelay.booking.outreach_requested`: the band approved a booking ask for one city — an anchor target plus the same-city contacts the selector ranked next — and the approval names everybody the letter goes to. Same all-or-none rule as gig outreach: every recipient's version was re-pinned inside the sending transaction and the contact governor reserved every window, so the executor delivers one letter to the full `recipients` set or delivers nothing.

The payload names `action_id`, `city_id`, the anchor `target_id` and `contact_email`, the `recipients` list (each `target_id`, `target_name`, `contact_email`), `phase` (`initial` or `followup`), the `proposed_window` when one was derived, `first_line_fact` (the evidence line, or null — never a fabricated one), `template_key` (`booking.opportunity.v1` / `booking.followup.v1`), and **`draft`, the finished letter: `subject` and `body`, composed in CrowdRelay when the action was written**. **Send `draft.body` verbatim and refuse a payload whose draft is missing or empty.** `first_line_fact` and `proposed_window` travel for the receipt and the ledger — the letter already cites them; they are not material for a second letter. `examples/autopilot-booking-outreach-executor.example.json` is the reference handler.

## Relationship outreach

`outreach.send` carries `crowdrelay.outreach.requested`: the band approved a pitch to one named relationship target — a playlist, a station, an editor, a creator — and the approval names the address the letter goes to. The target's version was re-pinned inside the sending transaction and the contact governor reserved the window, so the executor delivers the letter to that address or delivers nothing.

The payload names `action_id`, `opportunity_id`, `target_id`, `target_name`, `contact_email`, `phase` (`initial` or `follow_up`), `template_key`, `wave_id` when the pitch belongs to a wave, `evidence` (the numbers measured at request time — for the receipt and the ledger), and **`draft`, the finished letter: `subject` and `body`, composed in CrowdRelay when the action was written**. The pitch itself is the tenant's own release plan — **send `draft.body` verbatim and refuse a payload whose draft is missing or empty rather than composing from env vars, template keys or evidence**. A verified free-form route, when the target has one, takes the same `draft.body` as its `{{message}}` field — the transport changes, the words do not. A claim disposition other than `claimed` (`in_flight`, `ambiguous`) fails closed; `already_succeeded` is a no-op replay. `examples/autopilot-outreach-executor.example.json` is the reference handler.

## Outreach replies

`outreach.send` also carries `crowdrelay.outreach.reply_requested`: the band approved an answer to somebody who wrote back to an earlier pitch. Same transport, same mailbox, same claim/receipt rules as `crowdrelay.outreach.requested` — the events are separate so an executor that does not know replies skips this one rather than mistaking it for a pitch.

The payload names `action_id`, `target_id`, `target_name`, `target_kind`, `contact_email`, `answers_interaction_id` (the inbound interaction this letter closes), `reply_disposition`, `sheet_verdict` when one was imported (audit only — never quoted), and **`draft`, the reply `subject` and `body` composed in CrowdRelay when the action was written**. **Send `draft.body` verbatim and refuse a payload whose draft is missing or empty.** The draft is deliberately a scaffold — the system holds the sheet's verdict, not the reply's text — so nothing downstream may extend it into a guess at what they said. When the executor can thread by provider reference it should reply on the existing conversation; a fresh message with the `Re:` subject is the acceptable fallback. `Re:` subjects are how the operator sees the difference at a glance, so they must not be rewritten away.

## Opportunity applications

`opportunity.application` carries `crowdrelay.opportunity.application_requested`: the band approved an application to one scored live opportunity — a festival, showcase, contest or support slot — and the approval read the letter that goes to the organiser.

The payload names `action_id`, `opportunity_id`, the opportunity's `kind`, `score`, `title`, `organization`, `contact_email`, fee and deadline facts, `send_evidence`, the `payment_execution_allowed: false` marker, and **`draft`, the finished application: `subject` and `body`, composed in CrowdRelay when the action was written**. Guardrails stay live at the executor: refuse the event if `payment_execution_allowed` is not `false`, an `application_fee_minor` is nonzero, or `requires_contract`/`exclusive` is set. **Send `draft.body` verbatim and refuse a payload whose draft is missing or empty — nothing downstream may compose an application, including the fallback template this capability used to carry.** A claim disposition other than `claimed` fails closed; `already_succeeded` is a no-op replay; an ambiguous provider error retains the claim rather than retrying into a duplicate submission. `examples/autopilot-opportunity-application-executor.example.json` is the reference handler.

## Scene-node invite batches

`beacon.invite_batch` carries `crowdrelay.beacon.invite_batch_requested`: one verified scene node (latarnik) being asked to run invite codes for **one upcoming show in their own city**. The payload names the beacon (id, version, display name, contact email), the show (title, slug, start) and a `requested_count`; CrowdRelay issues the invite codes through its own machinery when the partner answers yes, so every signup that comes back is attributed and consented by construction.

A worked example is `n8n/examples/autopilot-beacon-invite-batch.example.json`: validate, claim once, compose the single-ask message from the payload facts (identifies sender, states where the address came from, explicit opt-out), send via the workspace Gmail credential, report the receipt with the claim token. The executor must never issue codes itself, invent signups, purchase or bot invites, broaden the ask beyond the named show, or re-ask on its own schedule: one batch per beacon per show is decided in CrowdRelay, and the cooldown lives there too.

## Drafted content, including press pitches

`crowdrelay.agent.content_requested` carries content an agent drafted and the brain approved for sending. The payload names `template_id`, `task_id`, the `draft` itself, and — when the template is a press pitch — `recipient_email`, `recipient_name` and `recipient_target_id`, resolved from `agent_outreach_targets` before the action was created.

**This event currently has no handler and production refuses it.** Measured 2026-09-13: four deliveries to `/webhook/crowdrelay-events-v1` returned HTTP 422, the newest that day, alongside 485 delivered events of other types. 422 is `http_permanent_status`, so the outbox stops retrying and the delivery is recorded `cancelled` — the pitch is simply gone. `crowdrelay.community.engagement_requested` is refused the same way. Every press pitch the brain has ever drafted ended here.

An executor for this event should branch on `template_id`: `press-pitch` is an email to `recipient_email`, and any other template is channel content whose own channel executor owns it. Two rules are absolute. Send to `recipient_email` and to nothing else — never to a list, never to an address the executor resolved itself; the recipient was chosen in CrowdRelay from a screened target and substituting another one sends cold mail to somebody nobody screened. And report the receipt as **Execution receipts** below requires, because until a provider-confirmed `succeeded` arrives, CrowdRelay creates no evidence and the measurement window never opens: the pitch would be sent and the brain would still learn nothing from it.

Until a handler exists, answering `2xx` and dropping the event is worse than the 422. The refusal is at least recorded, and `delivery.growth_event_refused` in `ops/attention` now reports it as critical.

**A press pitch now requires its own capability, `agent.content.press_pitch`.** The capability every other drafted template needs, `agent.content`, is advertised unconditionally by the CrowdRelay worker, because the social, telegram and discord executors run in-process there and claim `social-post`, `telegram-poster` and `discord-poster` respectively. A pitch is not work any of those three can do, so it no longer rides their capability: a `press-pitch` action parks with `awaiting_executor` rather than being dispatched, marked `succeeded` with no artifact, and emitted to a consumer that refuses it. The pending-approval list reports `executor_ready: false` for it, so the state is visible before an operator spends an approval, and the stale sweep cancels it with `no_executor` after the grace window instead of recording it as done.

An n8n executor that handles pitches must therefore register `agent.content.press_pitch` in its heartbeat capabilities. Registering it unparks the queue with no change in CrowdRelay — and registering it before the handler works is the one thing not to do, because that resumes emission to a consumer that will refuse it. Channel templates are unaffected and keep flowing through `agent.content`.

`examples/autopilot-press-pitch-executor.example.json` is the handler. It claims once with `capabilities:['agent.content.press_pitch']`, refuses any event that arrived without `send_evidence` (that would mean the dispatch gate was bypassed upstream), sends exactly one Gmail send to `recipient_email` with `replyTo` set to `VIRYA_PRESS_REPLY_TO` (falling back to `VIRYA_OUTREACH_FROM_EMAIL`) so replies land on the monitored inbox the reply monitor already watches, and reports a `press_pitch:{action_id}:email` receipt carrying the send's `send_evidence` in metadata so the trace keeps the provenance the gate demanded.

## Provider execution claims

Before Gmail, Discord, Drive, or another provider call without a trustworthy request-idempotency primitive, POST `/v1/internal/autopilot/actions/{action_id}/execution-claim`. Only `claimed` may call the provider. `already_succeeded` is a no-op replay. `in_flight` and `ambiguous` must fail closed and require reconciliation instead of an automatic second provider call. Explicitly safe/idempotent provider operations may omit the claim when their provider key guarantees replay safety.

Terminal `succeeded|failed` reports for a claimed execution must include the exact `claim_token`; a mismatched or stale token is rejected. Receipt keys include the claim token when present so separate provider attempts cannot collapse into one ledger row.

## Execution receipts

For every external action emitted by CrowdRelay, report provider progress to `/v1/internal/autopilot/actions/{action_id}/execution-report` with a stable status-specific `receipt_key` (for example `{action_id}:{executor}:{status}`), executor id, one of `accepted|executing|succeeded|failed`, and the provider reference/error kind when present. Replays are idempotent; a receipt key reused for a different action/status is rejected.

**A `content.artifact` receipt is not complete until it says where the artifact landed.** A terminal `succeeded` report for `crowdrelay.content.artifact_requested` must carry `metadata.artifact_delivery` with at least one non-blank of `url` (the artifact's public address), `surface` (where it was delivered — a channel, a draft store, a CMS), or `reference` (the provider-side id). A report whose metadata only proves a notification webhook accepted the request — "Discord was told an artifact was requested" — does not count the artifact as produced: CrowdRelay books such requests as failed and retries them under the bounded attempt cap. On 2026-09-28 a premiere drew fifty-nine artifact requests that all reported success through one Discord notify webhook while zero artifacts ever reached a fan-facing surface; this clause is why that cannot read as done again.

For external actions, the core action becoming `succeeded` means the canonical intent was durably dispatched to the execution plane. **Provider-confirmed `succeeded` is the authoritative completion edge.** CrowdRelay creates external execution outcome/effect evidence only after that receipt. For team opportunity/funding actions, the same successful receipt also performs the corresponding `submitted`/`prepared` domain transition in CrowdRelay; executors must not send a second progress callback for the same transition.

Queue the receipt durably **before** attempting delivery to CrowdRelay. If receipt transport is temporarily unavailable after Gmail/Drive/Discord already accepted the side effect, retry the receipt rather than replaying the provider side effect. The API accepts delayed authenticated execution receipts for up to seven days so a bounded outage can drain safely without rewriting the provider timestamp.

## Optional enrichment must not block primary work

Ancillary capabilities such as deadline-calendar seeding must not prevent the primary provider action from executing when the ancillary executor is unavailable. Dedicated actions whose entire purpose is that capability (for example a release calendar milestone) remain strict and fail closed.

## Release ledger

The heartbeat records component `n8n` automatically. Other production components can use `/v1/internal/autopilot/release-components`; those writes are observability-only and must never gate a successful deploy.

## Circuit breaker

Three distinct `failed` action receipts from the same executor inside 15 minutes open a 15-minute executor circuit breaker. While open, that executor contributes no capabilities even if its heartbeat remains fresh. A later provider-confirmed `succeeded` receipt or guard expiry closes it. Heartbeats intentionally do not clear the guard, so a restart cannot immediately hide a provider outage.

## Scout registry sync (inbound wake: n8n → CrowdRelay)

Most of this contract covers work CrowdRelay emits and the executor performs. The scout-registry sync runs the other way: n8n is the merge layer between the team's spreadsheets, and CrowdRelay is where the merged data lands durable.

`examples/scout-registry-sync.example.json` is the workflow. It reads the GitHub registry snapshot (`scout_runs/current.json` — the same rows `database_festivals.xlsx` renders, versioned and validated in-repo), reads the three Google workbooks, dedupes the snapshot's opportunity rows against everything the band already knows, appends the survivors to the one writable tab, then wakes CrowdRelay.

### Source roles — read every run, write only one

| Source | Role | This job may |
| --- | --- | --- |
| `wojciechbator/crowdrelay-db` (`database_festivals.xlsx` / `scout_runs/current.json`) | SCOUT_PL candidate snapshot produced by the scouting pipeline | read |
| MASTER sheet | canonical CRM — entities, contacts, opportunities | read |
| SCOUT sheet (`SCOUT.OPPORTUNITIES`) | the tab humans triage; the ONLY append target | read + append ≤20 rows/run |
| SCOUT AUTO sheet | per-run machine snapshot — a report, not a ledger | read |

Dedupe follows the scout's own FLOW_MAP rule — URL + title + organizer — enlarged to the whole known universe: every dedupe key, destination URL, opportunity title lead, and public email across SCOUT, MASTER, SCOUT AUTO, and the snapshot itself. A match in doubt counts as known; a duplicate append is the failure mode the job exists to prevent. Appends are capped at 20 per run and ordered strong-fit first.

`VIRYA_SCOUT_SYNC_COMMIT=TRUE` turns writes on; without it the run is a dry-run that reports what it would have appended and writes nothing. `CONTACTS`, `ORGANIZERS` and `CANONICAL_APPEND` rows never reach a Google Sheet — they are Postgres-only, picked up by the rescan below, and in the workflow they serve only to enrich a new row's `Public Contact`.

### The wake: `POST /v1/internal/registry/sync`

After the merge (commit or dry-run — a re-scan of an unchanged source is free), the job calls the internal wake instead of touching Postgres:

```
POST {base}/v1/internal/registry/sync
Authorization: Bearer {CROWDRELAY_COMMERCE_TOKEN}
Idempotency-Key: scout-registry-sync:{mode}:{date}
```

No body. `202 {"scan":"requested"}` means both listeners were notified; `503` means the database refused the notify — retry the POST, the wake itself is idempotent. The handler issues `pg_notify` on the two channels the workers already subscribe to: `gdrive_contacts` (Drive workbooks: SCOUT, SCOUT AUTO, MASTER, PROMO…) and `github_registry` (the registry repo). It is fire-and-forget — the workers decide what actually changed; a missed wake is covered by the next scheduled sweep, and a doubled one costs one no-change scan.

Do not write raw `pg_notify` from n8n, and do not connect to Postgres at all: the route is the abstraction, and the intake owns parsing, dedupe, policy, persistence and audit. On the CrowdRelay side the same files re-run through the shared sheet intake — `OPPORTUNITIES → team_opportunities`, `META_TARGETS → beacons`, `SUPPORT_TARGETS → peer_acts`, every email column → `drive_contacts` — each on its own conflict key, so an unchanged file is a no-op and a row-level refusal retries instead of sealing the file as consumed.

### Environment

`VIRYA_SCOUT_SHEET_ID`, `VIRYA_MASTER_SHEET_ID`, `VIRYA_SCOUT_AUTO_SHEET_ID` (Google Sheets OAuth credential — anonymous reads return 401), `VIRYA_REGISTRY_REPO`/`VIRYA_REGISTRY_REF` (default `wojciechbator/crowdrelay-db@main`), `VIRYA_SCOUT_SYNC_COMMIT`, `CROWDRELAY_INTERNAL_BASE_URL` (or `CROWDRELAY_API_URL`), `CROWDRELAY_COMMERCE_TOKEN`, `VIRYA_OPS_ALERT_WEBHOOK_URL`.
