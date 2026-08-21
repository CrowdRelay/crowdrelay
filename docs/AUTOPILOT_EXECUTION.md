# Autopilot execution wiring

These workflows are **write-side executors**, not dashboard demos. They are designed to be imported into the existing self-hosted n8n instance and connected to the signed CrowdRelay ingress.

Together they close the growth loop end to end: provider-specific workflows create consented campaigns, the delivery workers send them, receipts flow back into CrowdRelay, and the reconciler closes campaigns that have nothing left to send.

## Required environment

```text
CROWDRELAY_INTERNAL_BASE_URL=https://<private-crowdrelay-base>
CROWDRELAY_ADMIN_BASE_URL=https://<private-admin-crowdrelay-base>
CROWDRELAY_ADMIN_TOKEN=<admin-scoped-token>
CROWDRELAY_COMMERCE_TOKEN=<executor-scoped-commerce-token>
VIRYA_N8N_EXECUTOR_ID=<stable-executor-id>
VIRYA_OUTREACH_FROM_EMAIL=<sender-address>
VIRYA_CURRENT_PITCH_TITLE=<current-release-or-track-title>
VIRYA_CURRENT_PITCH_URL=<canonical-listen-url>
VIRYA_EPK_URL=<canonical-epk-url>
VIRYA_OUTREACH_FORM_ROUTES_JSON=<verified-free-form-route-map>
VIRYA_GROWTH_ROUTE_MANIFEST_JSON=<verified-free-growth-route-manifest>
VIRYA_MAIL_DELIVERY_URL=<canonical-mailer-endpoint>
VIRYA_MAIL_DELIVERY_TOKEN=<mailer-token>
VIRYA_BANDSINTOWN_FOLLOW_URL=<bandsintown-follow-url>
VIRYA_SPOTIFY_ARTIST_URL=<spotify-artist-url>
VIRYA_SPOTIFY_PLAYLIST_URL=<owned-spotify-playlist-url>
VIRYA_SPOTIFY_URL=<public-spotify-url>
VIRYA_SPOTIFY_GROWTH_SEGMENT_SLUG=<spotify-growth-segment>
VIRYA_BANDSINTOWN_GROWTH_SEGMENT_SLUG=<bandsintown-growth-segment>
VIRYA_AUTOPILOT_DIGEST_EMAIL=<kpi-digest-recipient>
SPOTIFY_ACCESS_TOKEN=<spotify-token>
SPOTIFY_ARTIST_ID=<spotify-artist-id>
BANDSINTOWN_APP_ID=<bandsintown-app-id>
BANDSINTOWN_ARTIST_NAME=<bandsintown-artist-name>
BANDSINTOWN_ARTIST_URL=<bandsintown-artist-url>
BANDSINTOWN_EVENT_ID=<bandsintown-event-id>
BANDSINTOWN_SMART_LINK=<bandsintown-smart-link>
```

Keep all secrets in n8n credentials/environment. Never export production values into workflow JSON.

## Outreach workflows

### `autopilot-outreach-executor.example.json`

Handles `viryaos.outreach.requested`.

1. Validate the server-owned intent.
2. Claim the durable action before any side effect.
3. Prefer an explicitly verified, HTTPS, free, no-login, no-CAPTCHA POST form.
4. Otherwise send through Gmail to the server-provided contact address.
5. Report a durable provider receipt with idempotency.

The form route is intentionally fail-closed. A route that is not explicitly verified/free, requires login, MFA or CAPTCHA, or uses an unsupported method is not executed automatically.

### `autopilot-outreach-reply-monitor.example.json`

Watches the outbound Gmail inbox and maps a reply through the provider reference. A reply is recorded as a durable `received` fact so the existing CrowdRelay follow-up scheduler stops automatically.

## Campaign delivery workflows

### `autopilot-free-fan-campaign.example.json`

Runs every five minutes and executes due `show.growth.free_fan_push.v1` campaigns.

It uses the existing consent-filtered campaign delivery plan, claims each recipient exactly once, sends through the canonical mailer, records delivery receipts, and completes the campaign when no pending deliveries remain. A page is capped at 250 recipients; subsequent five-minute passes continue the same campaign safely.

The message contains real, configured CTAs for:

- Bandsintown follow
- Spotify artist profile
- optional owned Spotify playlist
- the canonical event ticket URL when present

No third-party contacts are imported. The recipient snapshot is already filtered by CrowdRelay's `marketing_consent` gate.

### `autopilot-growth-campaign-delivery.example.json`

The generic delivery worker. It polls every five minutes and executes due campaigns for:

- `show.growth.free_fan_push.v1`
- `autopilot.spotify.follow.v1`
- `autopilot.bandsintown.follow.v1`

Execution is real:

1. fetch only scheduled growth campaigns;
2. load the consent-filtered delivery plan;
3. claim each fan delivery exactly once;
4. build the message from the provider executor's campaign content;
5. send through the canonical mailer with idempotency and bounded retries;
6. write the delivery receipt back to CrowdRelay;
7. finish the campaign when no pending deliveries remain.

A page is capped at 250 recipients. The five-minute poll naturally continues large campaigns without holding a long-running n8n execution.

### `autopilot-growth-campaign-reconciler.example.json`

A separate five-minute safety net for campaigns the delivery worker can never finish on its own: those with zero eligible recipients, and those whose deliveries already completed but whose campaign row was never closed.

It reads the delivery plan, keeps only campaigns with `pending_count == 0`, and posts the terminal `/complete` transition with the observed recipient/delivered/failed counts. It performs no sends, so it cannot double-deliver.

## Provider workflows

`autopilot-spotify-growth.example.json` and `autopilot-bandsintown-growth.example.json` read current provider state and create the appropriate consented campaign. `autopilot-growth-route-executor.example.json` submits verified free growth routes, and `autopilot-growth-daily-kpi.example.json` sends the measurement digest.

## Release playlist loop

Migration `0072_release_playlist_outreach.sql` creates the missing release-to-playlist edge. When `start_press` is recorded, eligible playlist targets receive a durable `release.playlist.v1` opportunity in the same database transaction. It only selects active, verified, outreach-accepting, non-suppressed playlist targets, and it is idempotent.

The existing bounded outreach executor then performs the actual email or verified free form submission. Existing reply monitoring stops follow-ups after an inbound reply.

## Provider boundaries

Some provider-native actions do not expose an unattended write API. Those remain explicit manual steps in the `viryaos.show_growth.requested` receipt rather than pretending that a browser click is a stable integration. In particular, Spotify for Artists profile actions and Bandsintown dashboard-only actions should be configured through their official provider surfaces.

The autonomous layer **does** automate the surrounding free growth loop: consented owned email, public playlist/press outreach, event distribution checks, canonical CTAs, measurement, receipts and follow-up suppression.

Never automate artificial streams, fake followers, paid playlist placement, CAPTCHA bypass, credential sharing, or mass unsolicited community posting.
