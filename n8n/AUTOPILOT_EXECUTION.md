# Autopilot execution wiring

The main branch already contains provider-specific Spotify/Bandsintown executors and the bounded outreach + reply-closure executors. This pass closes the remaining write-side gap: **campaign creation now has a real n8n delivery worker**.

## Required n8n environment

```text
CROWDRELAY_INTERNAL_BASE_URL=https://<private-crowdrelay-base>
CROWDRELAY_ADMIN_TOKEN=<admin-scoped-token>
CROWDRELAY_COMMERCE_TOKEN=<executor-scoped-token>
VIRYA_MAIL_DELIVERY_URL=<canonical-mailer-endpoint>
VIRYA_MAIL_DELIVERY_TOKEN=<mailer-token>
```

Keep secrets in n8n credentials/environment. Never export production values into workflow JSON.

## `autopilot-growth-campaign-delivery.example.json`

Import and activate this workflow in the existing n8n instance.

It polls every five minutes and executes due campaigns for:

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

## Playlist pitching

Migration `0072_release_playlist_outreach.sql` seeds `release.playlist.v1` opportunities when `start_press` is recorded. It only selects active, verified, outreach-accepting, non-suppressed playlist targets and is idempotent.

The existing bounded outreach executor then sends the actual email or verified free form. Existing reply monitoring stops follow-ups after an inbound reply.

## Provider boundaries

Spotify and Bandsintown provider-specific workflows remain responsible for reading current provider state and creating the appropriate consented campaign. Dashboard-only provider actions remain explicit manual steps where no supported unattended write API exists.

This automation does **not** create fake streams/followers, buy playlist placement, bypass CAPTCHA/MFA, scrape private contacts, or mass-post into moderated communities.
