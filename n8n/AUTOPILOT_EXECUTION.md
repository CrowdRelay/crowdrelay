# Autopilot execution wiring

These workflows are **write-side executors**, not dashboard demos. They are designed to be imported into the existing self-hosted n8n instance and connected to the signed CrowdRelay ingress.

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
VIRYA_MAIL_DELIVERY_URL=<canonical-mailer-endpoint>
VIRYA_MAIL_DELIVERY_TOKEN=<mailer-token>
VIRYA_BANDSINTOWN_FOLLOW_URL=<bandsintown-follow-url>
VIRYA_SPOTIFY_ARTIST_URL=<spotify-artist-url>
VIRYA_SPOTIFY_PLAYLIST_URL=<owned-spotify-playlist-url>
```

Keep all secrets in n8n credentials/environment. Do not put production values in exported workflow JSON.

## Workflows

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

### `autopilot-free-fan-campaign.example.json`

Runs every five minutes and executes due `show.growth.free_fan_push.v1` campaigns.

It uses the existing consent-filtered campaign delivery plan, claims each recipient exactly once, sends through the canonical mailer, records delivery receipts, and completes the campaign when no pending deliveries remain. A page is capped at 250 recipients; subsequent five-minute passes continue the same campaign safely.

The message contains real, configured CTAs for:

- Bandsintown follow
- Spotify artist profile
- optional owned Spotify playlist
- the canonical event ticket URL when present

No third-party contacts are imported. The recipient snapshot is already filtered by CrowdRelay's `marketing_consent` gate.

## Release playlist loop

Migration `0064_release_playlist_outreach.sql` creates the missing release-to-playlist edge. When `start_press` is recorded, eligible playlist targets receive a durable `release.playlist.v1` opportunity in the same database transaction. It is idempotent and refuses inactive, unverified, non-consenting or suppressed targets.

The existing Autopilot outreach executor then performs the actual email/form submission.

## Provider boundaries

Some provider-native actions do not expose an unattended write API. Those remain explicit manual steps in the `viryaos.show_growth.requested` receipt rather than pretending that a browser click is a stable integration. In particular, Spotify for Artists profile actions and Bandsintown dashboard-only actions should be configured through their official provider surfaces.

The autonomous layer **does** automate the surrounding free growth loop: consented owned email, public playlist/press outreach, event distribution checks, canonical CTAs, measurement, receipts and follow-up suppression.

Never automate artificial streams, fake followers, paid playlist placement, CAPTCHA bypass, credential sharing, or mass unsolicited community posting.
