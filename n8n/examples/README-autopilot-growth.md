# Autopilot growth workflow bundle

The production executor remains the decision authority in CrowdRelay. These n8n exports are provider adapters only.

## Workflows

- `autopilot-outreach-executor.example.json` — real Gmail / verified free-form submission with claim + receipt.
- `autopilot-outreach-reply-monitor.example.json` — correlates inbound Gmail replies and closes follow-up loops.
- `autopilot-spotify-growth.example.json` — reads Spotify artist state, creates and schedules a consented first-party Spotify CTA campaign, and reports a receipt. Spotify Artist Pick remains a provider-dashboard step unless an official write API is available.
- `autopilot-bandsintown-growth.example.json` — reads Bandsintown artist + upcoming-event state, creates and schedules a consented first-party CTA campaign, and reports a receipt. Provider-dashboard follower posts / Email Builder remain explicit provider-native steps.
- `autopilot-growth-route-executor.example.json` — executes allowlisted, HTTPS, free, idempotent, no-login/no-CAPTCHA POST routes for show-growth levers when a verified route exists.
- `autopilot-growth-daily-kpi.example.json` — daily Spotify follower + Bandsintown follower + Autopilot runtime digest, synchronized before sending one email.

## Required environment

Common:

- `CROWDRELAY_INTERNAL_BASE_URL`
- `CROWDRELAY_ADMIN_BASE_URL`
- `CROWDRELAY_COMMERCE_TOKEN`
- `CROWDRELAY_ADMIN_TOKEN`
- `VIRYA_N8N_EXECUTOR_ID`

Spotify:

- `SPOTIFY_ARTIST_ID`
- `SPOTIFY_ACCESS_TOKEN`
- `VIRYA_SPOTIFY_URL` (optional)
- `VIRYA_SPOTIFY_GROWTH_SEGMENT_SLUG` (optional)

Bandsintown:

- `BANDSINTOWN_ARTIST_NAME`
- `BANDSINTOWN_APP_ID`
- `BANDSINTOWN_ARTIST_URL` (optional)
- `BANDSINTOWN_SMART_LINK` (optional)
- `BANDSINTOWN_EVENT_ID` (optional)
- `VIRYA_BANDSINTOWN_GROWTH_SEGMENT_SLUG` (optional)

Daily digest:

- `VIRYA_AUTOPILOT_DIGEST_EMAIL`

Verified growth routes:

- `VIRYA_GROWTH_ROUTE_MANIFEST_JSON`

The route manifest must contain only explicitly verified, free, HTTPS, idempotent POST routes that do not require login or CAPTCHA. New routes belong in deployment secrets, not in Git.

All workflows are shipped inactive and with execution-data persistence disabled. Activate only after the exact private export has passed the repository's existing n8n attestation/smoke process.
