# Virya mobile application contract

The private Virya mobile application uses CrowdRelay as its sole operational backend.

## Roles

- `owner`: uses the admin bearer and may issue/revoke admission passes and inspect all operational views.
- `staff`: uses the staff bearer and may scan admission passes, inspect ticket sales, redeem merch discounts, and manage concert check-in QR campaigns.
- `fan`: uses only public and fan-session endpoints. It never receives an operator credential.

The mobile application stores operator credentials in an encrypted local Stronghold vault protected by a local PIN. Credentials are consumed only by the Rust Tauri command layer and are not returned to the Leptos/WASM UI after unlock.

## Staff endpoints added for mobile

- `GET /v1/staff/events/{slug}/ticketing`
- `POST /v1/staff/coupons/redeem`
- `GET|POST /v1/staff/event-qr/campaigns`
- `GET /v1/staff/event-qr/overview`
- `POST /v1/staff/event-qr/campaigns/{campaign_id}/revoke`

Existing `POST /v1/staff/admission/redeem` remains the gate admission endpoint. Admin-only pass issuance and revocation remain under `/v1/admin/admission/*`.

## Security boundary

The static staff bearer is suitable for the first private band build, but it is an installation-wide credential: rotation invalidates every staff device. The next production hardening milestone is a short-lived, per-device session minted from a one-time pairing QR and backed by `workspace_member_sessions`.
