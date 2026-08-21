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

## Campaign delivery

Growth campaigns are **not** delivered by a dedicated workflow, and no polling
worker ships here.

`execute_first_party_growth_campaign` creates the consent-filtered campaign,
schedules it, and writes a `communication.campaign_due` outbox event. That event
reaches the existing campaign executor through the verified webhook ingress, and
that executor already owns the whole delivery loop: load the delivery plan, claim
each recipient exactly once, send through the mailer, report the result, and
complete the campaign when nothing is pending.

Adding a second worker that lists campaigns on a schedule would put two
claimants on the same deliveries and would need an admin credential inside n8n.
The delivery plan is served by `/v1/internal/...` under the executor credential,
so no admin access is required anywhere in this path.

### Growth CTAs

The campaign executor must render the CTAs that arrive in the campaign content
rather than hard-coding provider URLs. For `show.growth.free_fan_push.v1` the
content carries:

```json
{
  "growth_ctas": {
    "bandsintown_follow_url": "env:VIRYA_BANDSINTOWN_FOLLOW_URL",
    "spotify_artist_url": "env:VIRYA_SPOTIFY_ARTIST_URL",
    "spotify_playlist_url": "env:VIRYA_SPOTIFY_PLAYLIST_URL"
  }
}
```

A value prefixed with `env:` names an n8n environment variable and must be
resolved at send time; anything else is used verbatim. A campaign whose CTAs
cannot be resolved should send without them rather than emit a broken link.

Recipients are already filtered by CrowdRelay's `marketing_consent` gate, and no
third-party contacts are imported.

### Zero-recipient campaigns

A campaign with no eligible recipients still has to reach a terminal state. The
campaign executor completes it directly with zero counts; there is no separate
reconciler, because a second worker calling `/complete` can close a campaign the
first one is still delivering.

## Provider workflows

`autopilot-spotify-growth.example.json` and `autopilot-bandsintown-growth.example.json` read current provider state and create the appropriate consented campaign. `autopilot-growth-route-executor.example.json` submits verified free growth routes, and `autopilot-growth-daily-kpi.example.json` sends the measurement digest.

## Release playlist loop

Migration `0072_release_playlist_outreach.sql` creates the missing release-to-playlist edge. When `start_press` is recorded, eligible playlist targets receive a durable `release.playlist.v1` opportunity in the same database transaction. It only selects active, verified, outreach-accepting, non-suppressed playlist targets, and it is idempotent.

The existing bounded outreach executor then performs the actual email or verified free form submission. Existing reply monitoring stops follow-ups after an inbound reply.

## Provider boundaries

Some provider-native actions do not expose an unattended write API. Those remain explicit manual steps in the `viryaos.show_growth.requested` receipt rather than pretending that a browser click is a stable integration. In particular, Spotify for Artists profile actions and Bandsintown dashboard-only actions should be configured through their official provider surfaces.

The autonomous layer **does** automate the surrounding free growth loop: consented owned email, public playlist/press outreach, event distribution checks, canonical CTAs, measurement, receipts and follow-up suppression.

Never automate artificial streams, fake followers, paid playlist placement, CAPTCHA bypass, credential sharing, or mass unsolicited community posting.
