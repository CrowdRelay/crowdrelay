# API and integration

The canonical contract is [`../openapi/openapi.yaml`](../openapi/openapi.yaml). All application endpoints are under `/v1`.

## Concert QR flow

1. A trusted server calls `POST /v1/admin/event-qr/campaigns` with an event slug, label, validity window and optional check-in cap.
2. The response contains a signed token while the campaign is enabled and not expired.
3. Put the token in a URL fragment, for example `https://virya.music/pl/live/show-slug/#checkin=<token>`. Fragments are not sent in HTTP requests or referrers.
4. The fan page removes the fragment, then calls `POST /v1/events/{slug}/check-in` with the token and an authenticated fan session.
5. A repeated scan for the same fan and event returns `200` with `created: false`.
6. Revoke immediately with `POST /v1/admin/event-qr/campaigns/{id}/revoke`.

The physical print is a bearer artifact. Use a narrow validity window, display it only at the venue and revoke it if photographed or leaked before the show.

## Frontend client

`packages/crowdrelay-js` is the dependency-free client. `integration/virya` is a copy-ready mirror used by the Virya Astro app. Run `make validate-contract-assets` after changing OpenAPI, bootstrap JSON or either client.

## Error handling

Admin and fan responses use private `no-store` caching. Invalid, expired and revoked check-in tokens intentionally return `404` so callers cannot distinguish token structure from campaign state. Capacity exhaustion returns `409`.
