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
| `viryaos.fan_lifecycle.message_requested` | deliver deterministic welcome/follow-up/reactivation copy to the already consented fan identity supplied by CrowdRelay |
| `viryaos.booking.outreach_requested` | deliver the already-authorized booking initial/follow-up message |
| `viryaos.outreach.requested` | deliver the already-authorized press, review, patronage or endorsement message |

For Gmail-backed booking/outreach, provider correlation is durable in CrowdRelay's execution-report ledger. The private executor writes the Gmail `threadId` as `provider_reference`, and inbound monitoring resolves that reference through CrowdRelay before reporting the deterministic `received` disposition. n8n keeps no durable business-correlation map and does not infer positive/negative intent.

Executors must use the CrowdRelay event ID or supplied business key as their idempotency key. They must not rescore a lead, change pricing policy, infer authority, or silently broaden the requested action. A provider failure is a delivery failure; it is not permission for n8n to invent an alternative business action.

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
