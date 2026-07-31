# n8n integration

CrowdRelay emits durable, HMAC-signed webhook events through its transactional
outbox. n8n can be used as an optional delivery adapter for email, chat,
social-media, AI-enrichment, and other external providers.

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

## Recommended private layout

Keep production exports locally, for example:

```text
n8n/
  crowdrelay-*.json
  workflow-manifest.tsv
  import-workflows.sh
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
