# Signal — Latarnik v1

## Product intent

Latarnik is a low-friction professional relationship surface for verified Virya OS Beacons
(local press, radio, TV, reviewers, creators, photographers, promoters, venues, scene partners,
patrons and community contacts). It is deliberately **not** a fan tier and **not** a staff role.
The goal is to make useful local Virya material easier to discover and act on without turning a
media relationship into a newsletter or street-team obligation.

## Authority boundaries

- `viryaos_beacons` remains the authoritative CRM/relationship record.
- `viryaos_beacon_signal_profiles` owns Signal-specific preferences and lifecycle only.
- `viryaos_beacon_signal_sessions` owns revocable device/browser access.
- CrowdRelay owns eligibility, distance, idempotency and delivery decisions.
- n8n remains an executor/integration surface; it must not choose recipients or invent eligibility.
- Virya web is a static-first press-room UI and talks directly to canonical CrowdRelay endpoints.
- Virya Signal exposes the Latarnik entry point now; native Stronghold session adoption can replace
  the browser handoff later without changing the backend identity model.

## Implemented in v1

### Invitation and identity

- Admin can mint an invite only for an existing active, verified Beacon that accepts outreach and is
  not marked `do_not_contact`.
- Invite capabilities are URL-safe random values; only SHA-256 hashes are persisted.
- Invite exchange is one-time and transactional.
- Re-inviting a paused/revoked Beacon revokes every old device session before a new trust ceremony.
- Sessions expire after 180 days and are explicitly revocable.
- Logout revokes the current session and disables push endpoints bound to that session.

### Preferences and local radar

- Radius is operator/user bounded to 10–500 km; UI presets are 25/50/100/200/300 km.
- Locale and topics are bounded allow-lists.
- `/v1/beacon/me` exposes the Beacon profile, nearby published concerts, current preferences,
  press-room links and open press-request count.
- Nearby matching uses geodesic distance in PostgreSQL and never crosses workspace boundaries.

### Press room

- Static-first PL and EN pages: `/pl/latarnik` and `/latarnik`.
- Invite capability is removed from the browser URL immediately after exchange.
- Press room gives direct access to EPK, gallery, rider, Spotify and YouTube.
- A Latarnik can request press photos, WAV, clean version, interview, accreditation or a custom item.
- Staff can read the resulting bounded request queue through the admin API.

### Push

- Existing proven push transport/ACK pipeline is reused with a first-class `beacon` audience.
- Endpoint ownership is bound to a specific live Beacon session hash.
- Worker revalidates the session, active profile, active+verified Beacon, `accepts_outreach` and
  `!do_not_contact` before claim/delivery.
- Nearby emission is idempotent per `Beacon × event`, bounded to at most 100 candidates per call,
  default 20, and ranked by event time, relevance, relationship score and distance.
- The emitter does not use unbounded `join_all` or application-side fan-out.

## Deliberately not automatic in v1

Deploying v1 **does not** send 200 invitations and does not opt anybody into a new communication
channel. Bulk invitation outreach should be a separate explicit operator action with bounded waves,
preview, suppression and a delivery receipt. This prevents a release from accidentally becoming a
mass-mail event.

## Next polish slice

1. Native Virya Signal Latarnik session vault in Stronghold and direct FCM registration as
   `audience_kind=beacon` (browser contract remains compatible).
2. Bounded invitation-wave planner (for example 20–50 contacts per wave) using existing Beacon
   relevance/relationship/suppression data; n8n only executes the preselected mail action.
3. Event/release-specific press packs with multiple bio lengths, portrait/landscape assets,
   social crops, prewritten copy and one-click "download everything" fallback.
4. `Mogę pomóc / Nie tym razem` response capture and `BeaconOpportunity`-style per-event status.
5. Coverage URL capture after shows and relationship-score feedback from real published coverage.
6. Staff Signal surface for press requests and local Beacon response funnel.
7. Measured conversion dashboard: invited → exchanged → opened → interested → coverage, without
   exposing personal contact details in aggregate analytics.

## Safety / communication rule

Latarnik must be sold as **less friction and fewer irrelevant messages**, not as an ambassador
obligation. Location/radius relevance and explicit outreach preferences stay authoritative at every
stage, including at delivery time.
