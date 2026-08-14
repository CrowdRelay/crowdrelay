# n8n integration

CrowdRelay emits durable, HMAC-signed webhook events through its transactional
outbox. n8n can be used as an optional delivery adapter for email, chat,
social-media, Calendar, form-submission, and other external providers.

Production workflow exports are intentionally **not stored in this public
repository**. They commonly contain operator-specific details such as:

- credential references and credential names;
- channel, page, workspace, workflow, and provider identifiers;
- production domains and endpoint layout;
- message templates and operational routing;
- the exact integration topology used by a deployment.

Those values are not necessarily secrets by themselves, but publishing them
unnecessarily exposes deployment metadata and makes targeted reconnaissance
easier.

## Public example

A minimal, provider-neutral example is available at:

`n8n/examples/signed-event-branch.example.json`

It demonstrates only the branch boundary:

1. receive an event from a previously verified ingress;
2. validate the event type and schema version;
3. build a generic outbound action;
4. call an endpoint configured entirely through environment variables;
5. disable execution-data persistence.

The example contains no production domains, IDs, credential references,
provider names, or CrowdRelay operator data.

## VIRYA OS executor events

VIRYA OS keeps decisions in Rust and emits only provider-neutral execution intents. Private n8n branches may handle these events without implementing business rules:

| Event | Executor responsibility |
| --- | --- |
| `viryaos.calendar.upsert_requested` | idempotently create/update the requested Calendar item using `calendar_key` |
| `communication.campaign_due` | deliver the already-planned first-party communication campaign |
| `viryaos.opportunity.application_requested` | submit the already-qualified free/reversible application, then report `submitted`; never execute a payment |
| `viryaos.funding.package_requested` | render the application package from supplied canonical facts and report `package_ready` |
| `viryaos.funding.submission_requested` | submit only the explicitly human-approved ready package, then report `submitted` |
| `viryaos.autopilot.approval_requested` | notify the operator once that an action needs a decision |
| `viryaos.ops.status_changed` | deliver the already-deduplicated operational open/recovered status; do not implement queue thresholds or retry policy |
| `viryaos.fan_lifecycle.message_requested` | deliver deterministic welcome/follow-up/reactivation copy to the already consented fan identity supplied by CrowdRelay |
| `viryaos.booking.outreach_requested` | deliver the already-authorized booking initial/follow-up message |
| `viryaos.outreach.requested` | deliver the already-authorized press, review, patronage or endorsement message |
| `viryaos.beacon.discovery_requested` | scout public local sources for the requested event/market and return only source-backed Beacon candidates through the admin Beacon upsert; Gemini may summarize, never invent/verify a destination |
| `viryaos.beacon.outreach_requested` | deliver the already-authorized local Beacon pitch/follow-up; personalize tone/local hook only from supplied verified facts |
| `viryaos.show_growth.requested` | execute one already-selected external attendance lever; verify free listings/distribution, configure free audience-capture surfaces, use provider-native free fan pushes (Bandsintown Posts/free-quota Email Builder + Spotify Artist Pick manual step), run verified-Beacon partner cross-promo and factual social proof; return public receipts or explicit human `manual_steps`, never buy placement or invent proof |
| `viryaos.team.assignment_email_requested` | send the assigned band member one friendly what/why/deadline/link email; initial notification and reminders share the same provider-confirmed execution contract |

For Gmail-backed booking/outreach, provider correlation is durable in CrowdRelay's execution-report ledger. The private executor writes the Gmail `threadId` as `provider_reference`, and inbound monitoring resolves that reference through CrowdRelay before reporting the deterministic `received` disposition. n8n keeps no durable business-correlation map and does not infer positive/negative intent.


For non-idempotent provider calls (Gmail, Discord, Drive), the executor must first claim the exact action through `/v1/internal/autopilot/actions/{action_id}/execution-claim`. Only a `claimed` disposition may perform the provider call. `in_flight` or `ambiguous` is fail-closed: do not resend automatically. A successful provider call reports the returned `claim_token` with its terminal execution receipt. Calendar uses a deterministic provider event ID and remains safely replayable, but still reports provider completion.

Executors must use the CrowdRelay event ID or supplied business key as their idempotency key. They must not rescore a lead, change pricing policy, infer authority, or silently broaden the requested action. A provider failure is a delivery failure; it is not permission for n8n to invent an alternative business action.

For VIRYA outreach, Gemini is a **bounded copy adapter**, not a manager. A private n8n branch may give it the canonical facts and ask it to make a mail natural, concise and locally relevant, but the recipient, purpose, factual claims, allowed offer, send window, suppression state, approval state and follow-up cadence come from CrowdRelay and must be preserved verbatim in meaning. If personalization cannot be produced safely, send the deterministic fallback template or fail the executor receipt; never hallucinate a local connection.

## Recommended private layout

Keep production exports locally, for example:

```text
n8n/
  crowdrelay-*.json
  private-workflow-manifest.tsv
  ingress-routes.json
  deploy-production.sh
```

These paths are ignored by Git. Existing local files remain available after
they are removed from the Git index with `git rm --cached`.

Do not use `git clean -x` in this repository unless you have separately backed
up ignored operator files.

## Secretless production workflow attestation

The route manifest proves which event is mapped to which workflow ID, but it cannot prove the contents of a private n8n export. Before advertising a newly enabled capability, generate a smoke template from the exact private exports first. The template includes mapped workflows that are still `enabled=0`, so a provider/credential smoke can be completed **before** the manifest and heartbeat are flipped live. After the smoke passes, enable the mapping and generate the final public attestation bound to the same workflow SHA:

```bash
python3 scripts/generate_n8n_workflow_attestation.py \
  --manifest n8n/viryaos-production-workflow-manifest.tsv \
  --workflow-dir n8n/private-production-exports \
  --smoke-template-out /tmp/viryaos-smoke-template.json

# Fill the template only from an end-to-end smoke against the exact exported SHA.
# Then flip the verified production mapping to enabled=1 and generate the final attest:
python3 scripts/generate_n8n_workflow_attestation.py \
  --manifest n8n/viryaos-production-workflow-manifest.tsv \
  --workflow-dir n8n/private-production-exports \
  --smoke-results /tmp/viryaos-smoke-results.json \
  --output /tmp/viryaos-production-workflow-attestation.json
```

The generated artifact contains only workflow IDs and SHA-256 hashes, event/capability mappings, node **types/counts**, active state, execution-persistence settings, contract versions, and bound smoke-test booleans/timestamps. It deliberately excludes node names, parameters, URLs, credentials and credential references. Enabled workflows fail attestation when they are inactive, persist execution data, have stale smoke evidence, do not validate the event, skip a required execution claim, fail to queue/report provider receipts safely, or have not passed the provider credential check. The companion `.sha256` can be recorded as `WORKFLOW_ATTESTATION_SHA` in the n8n release-component metadata while the existing route-manifest SHA remains the heartbeat parity key.

## Security requirements

The public ingress must verify the webhook before invoking any branch:

1. retain the exact raw request body;
2. validate a short timestamp window;
3. calculate HMAC-SHA256 over `timestamp + "." + raw_body`;
4. compare signatures in constant time;
5. reject replayed event IDs durably;
6. return a non-2xx response if durable acceptance fails.

Store signing secrets, provider tokens, API keys, and credential material only
in the deployment secret store or protected n8n credentials.

For workflows that process email addresses, checkout tokens, claim links, QR
payloads, or access tokens, disable successful, failed, and manual execution
data persistence unless a carefully redacted audit trail is explicitly needed.

## ViryaOS executor runtime

See [`viryaos-executor-contract.md`](./viryaos-executor-contract.md) and [`viryaos-executor-manifest.tsv`](./viryaos-executor-manifest.tsv) for heartbeat capabilities, pre-send claims, provider execution receipts, campaign delivery safety, blue/green safety and release-ledger behavior. The concrete release mapping is [`viryaos-production-workflow-manifest.tsv`](./viryaos-production-workflow-manifest.tsv); its checked SHA is the release-ledger/heartbeat parity key. The legacy `import-workflows.sh` is deliberately fail-closed and is not a production deployment path.

### `team.email` activation state

`team.email` is now part of the production desired-state manifest (`VOSTEAMEMAIL001`, enabled). Deployment remains fail-closed: the executor must not include `team.email` in its live heartbeat until the exact private workflow export has passed the hash-bound pre-activation smoke (event validation, execution claim, provider receipt ordering, credential/provider check) and the workflow is active. The final public attestation must be generated from that same workflow SHA before the heartbeat is updated.
