# ViryaOS n8n executor contract

n8n is an executor, not the policy engine. CrowdRelay remains authoritative for decisions, quotas, idempotency, domain state and audit. Provider adapters should stay thin: validate the canonical event, perform one external side effect, then report what the provider actually did.

## Heartbeat

Each active blue/green instance posts `/v1/internal/autopilot/executors/heartbeat` using the commerce key. Use a 60–120 minute expiry and refresh well before expiry. Include the workflow-manifest SHA and **only capabilities whose route and provider adapter are actually live**. Do not hand-build this payload: use `scripts/build_n8n_executor_heartbeat.py` or `scripts/publish-n8n-heartbeat.sh` so the capability list is derived from the production manifest and the attestation SHA/timestamp/manifest binding are copied from the exact secretless attestation.

`team.email` is additionally fail-closed at the API boundary: a heartbeat advertising it is rejected unless `metadata.workflow_attestation_sha` is a SHA-256, `metadata.workflow_attestation_manifest_sha` equals the heartbeat manifest SHA, and `metadata.workflow_attested_at` is at most 14 days old. A successful heartbeat updates the `n8n` production component in the release ledger. Same-manifest heartbeats preserve attestation evidence; a manifest change clears old evidence automatically. Once the first executor registers, CrowdRelay permanently fails closed for missing/expired capabilities rather than silently returning to legacy mode.

Capabilities: `fan.lifecycle.message`, `merch.reorder`, `booking.outreach`, `merch.bundle`, `outreach.send`, `outreach.discovery`, `beacon.discovery`, `beacon.invite_batch`, `beacon.outreach`, `show.growth`, `content.artifact`, `show.escalation`, `promotion.budget`, `opportunity.application`, `funding.package`, `funding.submit`, `calendar.upsert`, `team.email`, `play.step`.

Do not advertise a capability merely because code for it exists in an export. The workflow must be active, reachable from the verified ingress/claim bridge and have a working provider credential/configuration.

### Attendance-growth execution

`show.growth` is deliberately an **external-lever capability**, not a second promotion policy engine. CrowdRelay chooses the event, lever, timing and safety contract. The executor may: publish/submit the canonical event to free or owned listings, ask an already verified venue/bill/scene Beacon for the supplied cross-promotion action, or format a factual social-proof packet for the requested channel.

For `beacon.discovery`, prioritize the supplied `priority_source_classes`: local metal media/podcasts, independent radio/music programmes, venue/promoter/support-band networks, record stores/rehearsal studios/music shops, tattoo/alternative-fashion/scene businesses, student/culture portals, moderated metal communities/forums and local live creators/photographers/reviewers. A generic local business is not a Scene Partner without public evidence of real scene relevance. Never scrape private member lists or personal contact data; community candidates should include public rules or a moderator contact when available.

For `free_listing_sweep`, treat the supplied `surface_classes` as a verification checklist rather than a demand to create duplicates. Check VIRYA's canonical show page, Bandsintown + canonical ticket link, Songkick Tourbox + canonical ticket link when artist access exists, Spotify live-event visibility (direct ticket-partner propagation or Bandsintown), the venue's free calendar/newsletter, and relevant free city/culture/scene calendars discovered for that market. Return successful public URLs. If a destination requires CAPTCHA, login, email verification or another unsupported human-only step, do not automate around it: return `metadata.manual_steps[]` with `destination`, `url`, `what_to_do` and `why_it_matters` so the operator has an actionable handoff. Also return checked/skipped surfaces with a reason so “success” cannot mean “we tried one site”.

For `audience_capture_setup`, keep Signal primary on VIRYA-owned surfaces while checking the provider-native Bandsintown Smart Link/Follow/Signup/Widget/QR capture surfaces. Also return a campaign-attributed VIRYA Signal/show QR manual step for the merch table, current shows or explicitly permitted partner surfaces when useful; the QR must preserve the normal consent flow and free-growth authority never buys printing/placement. Configure a presale signup only when a real presale exists; otherwise use a supported reminder only when truthful. Do not import Signal contacts into Bandsintown as part of this action. Account-login/2FA/CAPTCHA configuration becomes `manual_steps`.

For `free_fan_channel_push`, use only provider-native free reach already earned by VIRYA: location/RSVP-targeted Bandsintown Posts, Bandsintown Email Builder only while the provider-confirmed free quota is sufficient, a strong current live/Featured Video when that free surface is available, a Spotify Artist Pick operator step for the event, a YouTube artist Post to existing subscribers when Posts are available, and a Bandcamp Community message to existing followers when that audience exists. YouTube/Bandcamp account-level posting is a human step unless an official supported API exists; never scrape or import contacts just to broaden those audiences. Never invoke Bandsintown Boost, Promoted Email/Posts or another paid placement under this capability. If an official provider API is unavailable for an account-level action, return a human step rather than browser automation. For free-listing work, verify linked artist identities and downstream distribution health (including Bandsintown and Songkick partner graphs) instead of treating one successful source submission as proof that every downstream surface is live. Community/group promotion is manual or moderator-approved only: no automated cold posting, scraping or rule bypass.

It must not purchase placement, change ticket pricing, fabricate reviews/crowd/stream numbers, broaden the recipient list, or contact an unverified discovered candidate. Partner cross-promo should make one concrete authorized ask at a time. When no verified social proof exists, use the supplied local story/context instead of inventing proof. Gemini may only adapt wording/format from the supplied facts. First-party fan campaigns (ambassadors, high-intent last mile, merch pre-show/post-show) are scheduled inside CrowdRelay and therefore do not require this n8n capability.

### Play steps

`play.step` carries `crowdrelay.play.step_requested`: one step of one running campaign, for **one consented fan**, with the fan's contact details, the show it is about and a `template_key` naming the message. It is emitted per recipient on purpose — that is what makes the send idempotent, keeps the daily quota and the weekly owned-audience envelope meaningful, and bounds a play that goes wrong to one message rather than a segment.

CrowdRelay has already decided everything that matters: who is eligible, whether consent is current at the moment of dispatch, whether the step is still inside its window and whether the show is still on. The executor renders the named template from the supplied facts and sends it. It must not broaden the recipient list, substitute a different template, re-send a step it has already reported, or send at all once the event is past — a step delivered outside its moment is a different, worse message than one not delivered, and CrowdRelay records that omission as a skip rather than letting it arrive late.

## Target discovery (candidates, not targets)

Executors post what they found to `POST /v1/internal/autopilot/outreach/candidates` with the commerce key and an `Idempotency-Key`, up to 100 candidates per batch; CrowdRelay screens each one on write. The admin route of the same name stays for operator imports. Discovery is executor work, so it lives on the internal surface: requiring the admin key would hand an adapter authority over every admin route in order to post a list of playlist contacts.

An adapter may sweep unprompted, and it may also be *asked* to. `outreach.discovery` carries `crowdrelay.outreach.discovery_requested`, emitted when the agent has fewer confirmed submission routes than its policy floor. The event carries `requested_candidates`, the screening thresholds a sweep can pre-filter against, and the callback path. The sweep reads published data, contacts nobody and buys nothing, which is why it is `first_party_reversible`.

**Report the batch even when it is empty.** The internal route accepts an empty `candidates` array and the admin route does not, and that difference is the point: CrowdRelay tells a sweep that found nothing admissible — after which it stops asking — from one that was never answered, which stays an operator problem. It reads that from the ingestion itself, not from candidate rows, because both cases leave zero of those. An adapter that returns silently keeps the request alive for ever instead of backing it off.

**Report what the sweep read, in `sweep`.** `sources_read` is how many sources were queried, `items_seen` how many items they returned before any screening. Without them, an adapter that read two hundred playlists and found no published submission route posts exactly the same empty batch as one whose credential expired — and CrowdRelay would call the first a dry source worth widening and the second the same thing, sending an operator to fix the wrong end. The counts are treated as adapter claims: they change only which cause is reported, never authority, a cap or a screening outcome. They are refused as incoherent when `items_seen` is below the number of candidates posted, or above zero with `sources_read` at zero. The field is optional and the admin route rejects it outright — a human posting candidates by hand did not run a sweep.

Read a route, never work one out. `route_is_published` must be `true` only when the route was read verbatim out of text the curator published for that purpose — a submission line in a playlist description, a contact page, a reply. Guessing `firstname@domain` from a name and a website is refused as `route_inferred`, and a candidate refused once is never rediscovered, because the refusal is a stored row. Send the `evidence` snippet the route was read from: without it the candidate is refused as `evidence_missing`, since no human can check the extraction later.

Send the raw signals rather than a verdict. `follower_count`, `engagement_count`, `sells_placement` and `churns_indiscriminately` are inputs to CrowdRelay's screening, not conclusions; `fit_basis_points` is how well the candidate matches this band. Whether a candidate is admitted, and what a pitch through it would cost, is decided in CrowdRelay.

Register submission platforms with `POST /v1/admin/autopilot/outreach/submission-channels` before referencing them by `channel_slug`. The channel's `cost_model` decides the class of every pitch through it: `free` is ordinary third-party contact, `credit` and `fee` are spend and take the paid ceiling however small the amount, and `paid_placement` is refused outright at every autonomy level. An unknown slug is refused rather than defaulted to free.

Nothing is contacted as a result of ingestion. An operator confirms the route through `POST /v1/admin/autopilot/outreach/candidates/{candidate_id}/confirm`, and only then does an email route become an outreach target.

## The daily operator brief

`crowdrelay.ops.operator_brief` is a first-party notice to the band's own operator, not an audience message, and it rides the ops-alert channel and workflow that already carry `crowdrelay.ops.status_changed`. The workflow must branch on the event type: the payload is `headline`, `summary`, `snapshot` and `observed_at`, with no `alert_key` or `state`, because a brief is not a condition that opens and recovers.

CrowdRelay sends at most one a day and decides on its own whether there is anything worth saying; the executor delivers it and never suppresses, batches or re-sends it. Deliver it even while the agent is disabled — an operator whose agent is switched off with work waiting is precisely the reader the brief exists for, and a delivery path that goes quiet with the agent reproduces the silence it was built to break.

## Off-platform metric feeds

`GET /v1/admin/autopilot/growth-metrics/coverage` reports which of `spotify`, `youtube`, `bandsintown` and `social` the agent can currently see. A platform with no series reads as `missing`, not as quiet.

Feeds are ordinary metric ingestion: declare the series once with `POST /v1/admin/autopilot/growth-metrics/series` (platform, provider-neutral `metric_key`, `expected_interval_hours`, and a `value_tier` that is honest — follower counts are `vanity`), then post absolute observations to `POST /v1/admin/autopilot/growth-metrics/points`. Never post a delta: a delta makes a missed snapshot unrecoverable and double-counts on replay. `expected_interval_hours` is what makes a stopped feed detectable, so declare the interval the adapter actually runs at.

A worked example is `n8n/examples/autopilot-spotify-feed.example.json`: hourly client-credentials read of the artist object, hour-bucketed `captured_at`, a deterministic per-bucket `Idempotency-Key`, and an absent count posted as nothing rather than as zero. It deliberately declares only `spotify/followers` — monthly listeners are not in the documented Web API, and a number whose semantics were never confirmed against a real response gets no series. Bandsintown needs no adapter here: the event sync reads `tracker_count` server-side and feeds `bandsintown/trackers` itself.

## Delivery faults (bounces and spam complaints)

When the sending provider reports a bounce or a "marked as spam" complaint for an outreach message, POST it to `/v1/internal/autopilot/outreach/delivery-faults` with the commerce key and an `Idempotency-Key`. The body carries `fault` (`hard_bounce` | `soft_bounce` | `complaint`), `occurred_at`, the provider's own `provider_reference` when one exists, and **exactly one of** `target_id` or `contact_email`. Providers report addresses, not CrowdRelay's ids, so the email form is what webhook adapters normally use; an unknown address is refused rather than silently dropped, because a complaint about somebody missing from our tables is still a complaint about the sending domain.

A worked example is `n8n/examples/autopilot-delivery-faults.example.json`: one provider-mapping code node translates your ESP's field names into the fixed downstream contract, and the `Idempotency-Key` derives from the provider reference so a retried webhook is a replay.

The reference is what makes a retried webhook a replay instead of a second count: rows are deduplicated on it. CrowdRelay computes rates over a rolling window and closes the workspace's sending ceiling before the next wave when a rate crosses its threshold — so report promptly, because the halt is only ever as fresh as the last report. A hard bounce additionally finishes the address through the suppression that already exists; never keep sending to a target after one.

## Scene-node invite batches

`beacon.invite_batch` carries `crowdrelay.beacon.invite_batch_requested`: one verified scene node (latarnik) being asked to run invite codes for **one upcoming show in their own city**. The payload names the beacon (id, version, display name, contact email), the show (title, slug, start) and a `requested_count`; CrowdRelay issues the invite codes through its own machinery when the partner answers yes, so every signup that comes back is attributed and consented by construction.

A worked example is `n8n/examples/autopilot-beacon-invite-batch.example.json`: validate, claim once, compose the single-ask message from the payload facts (identifies sender, states where the address came from, explicit opt-out), send via the workspace Gmail credential, report the receipt with the claim token. The executor must never issue codes itself, invent signups, purchase or bot invites, broaden the ask beyond the named show, or re-ask on its own schedule: one batch per beacon per show is decided in CrowdRelay, and the cooldown lives there too.

## Provider execution claims

Before Gmail, Discord, Drive, or another provider call without a trustworthy request-idempotency primitive, POST `/v1/internal/autopilot/actions/{action_id}/execution-claim`. Only `claimed` may call the provider. `already_succeeded` is a no-op replay. `in_flight` and `ambiguous` must fail closed and require reconciliation instead of an automatic second provider call. Explicitly safe/idempotent provider operations may omit the claim when their provider key guarantees replay safety.

Terminal `succeeded|failed` reports for a claimed execution must include the exact `claim_token`; a mismatched or stale token is rejected. Receipt keys include the claim token when present so separate provider attempts cannot collapse into one ledger row.

## Execution receipts

For every external action emitted by CrowdRelay, report provider progress to `/v1/internal/autopilot/actions/{action_id}/execution-report` with a stable status-specific `receipt_key` (for example `{action_id}:{executor}:{status}`), executor id, one of `accepted|executing|succeeded|failed`, and the provider reference/error kind when present. Replays are idempotent; a receipt key reused for a different action/status is rejected.

For external actions, the core action becoming `succeeded` means the canonical intent was durably dispatched to the execution plane. **Provider-confirmed `succeeded` is the authoritative completion edge.** CrowdRelay creates external execution outcome/effect evidence only after that receipt. For team opportunity/funding actions, the same successful receipt also performs the corresponding `submitted`/`prepared` domain transition in CrowdRelay; executors must not send a second progress callback for the same transition.

Queue the receipt durably **before** attempting delivery to CrowdRelay. If receipt transport is temporarily unavailable after Gmail/Drive/Discord already accepted the side effect, retry the receipt rather than replaying the provider side effect. The API accepts delayed authenticated execution receipts for up to seven days so a bounded outage can drain safely without rewriting the provider timestamp.

## Optional enrichment must not block primary work

Ancillary capabilities such as deadline-calendar seeding must not prevent the primary provider action from executing when the ancillary executor is unavailable. Dedicated actions whose entire purpose is that capability (for example a release calendar milestone) remain strict and fail closed.

## Release ledger

The heartbeat records component `n8n` automatically. Other production components can use `/v1/internal/autopilot/release-components`; those writes are observability-only and must never gate a successful deploy.

## Circuit breaker

Three distinct `failed` action receipts from the same executor inside 15 minutes open a 15-minute executor circuit breaker. While open, that executor contributes no capabilities even if its heartbeat remains fresh. A later provider-confirmed `succeeded` receipt or guard expiry closes it. Heartbeats intentionally do not clear the guard, so a restart cannot immediately hide a provider outage.
