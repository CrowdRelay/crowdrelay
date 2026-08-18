# Signal Latarnik — native v1 contract

## Scope

Native Latarnik is a first-class Virya Signal mode backed by the existing CrowdRelay Beacon domain.
It does **not** create a second CRM, merge Beacon identity with a fan, or turn Latarnik into staff or
a street-team tier.

Canonical identity boundaries:

- `viryaos_beacons` remains relationship, verification and consent authority;
- `fan_id != beacon_id` and the device vaults/sessions remain independent;
- web and native clients exchange the same one-time invite capability into the same revocable Beacon
  session model;
- raw invite URLs/tokens and bearer tokens never enter campaign telemetry.

## Canonical invitation handoff

One HTTPS capability is used everywhere:

`https://virya.music/pl/latarnik?invite=<one-time-capability>`

(or `/latarnik` for EN).

The same URL can be delivered by email, copied, rendered as a QR or opened as an Android App Link.
If Virya Signal handles it, native code validates and exchanges the capability and stores the returned
bearer in a dedicated Stronghold vault before exposing an unlocked Latarnik session to WASM. Without
the app the existing web Latarnik performs the same exchange.

Exchange accepts optional `clientKind = web|android|ios`; missing means `web`. A reviewed invitation
job is attributed to the resulting session by UUID only. The transient profile attribution is cleared
as part of exchange.

## Mobile information architecture

Latarnik is a root mode, not a fan tab. Its four primary destinations are:

1. **Briefing** — bounded public VIRYA news plus relevant Latarnik state requiring attention.
2. **Radar** — nearby shows and the existing `interested/helping/declined` lifecycle.
3. **Press Room** — global/event-specific assets and bounded material requests.
4. **Dostęp / Access** — accreditation request status and selected physical-release allocations.

The fan and Beacon device profiles can coexist. Switching modes must never copy credentials or mutate
one identity when logging out/forgetting the other.

## Accreditation and benefits

Accreditation remains a `press_request(request_kind=accreditation)`. It is an availability/request
workflow, not compensation for coverage. Coverage submission is voluntary and never gates an invite,
press material, accreditation approval or continued Latarnik access.

Physical releases remain explicit bounded campaigns. Product copy must not promise that every active
Latarnik automatically receives every future physical release.

## Outreach campaign

The reviewed network workflow is:

`candidate → human review/consent evidence → preview → explicit queue/send → one-shot executor claim → delivery report → exchange`

Preview validates the exact selected IDs/radius/locale/TTL and copy without minting capabilities.
Default rollout is a 15–20 person wave; larger waves remain operator initiated and capped at 200.
No deployment action may automatically queue outreach.

Campaign conversion is measured from durable product actions rather than tracking pixels:

- exchanged;
- web / Android / iOS activation;
- currently active sessions;
- Beacon push enabled;
- helping/completed event engagement;
- submitted coverage.

These metrics derive from `source_invite_job_id` on sessions and do not retain invitation secrets.

## Native push

The existing `audience_kind=beacon` delivery pipeline is reused. Endpoints remain bound to the hash of
a live Beacon session. Selection and worker delivery continue to revalidate active profile, live
session and current Beacon consent. One FCM installation may legitimately have separate fan and
Beacon endpoints; disabling one audience must not disable the other.

## Release order

1. additive CrowdRelay migration/API and campaign preview/metrics;
2. virya.music web fallback, news feed, staff QR and Android association file;
3. Virya Signal native Stronghold/API/QR/App-Link flow;
4. internal Android E2E;
5. 2–3 controlled test profiles;
6. first reviewed 15–20 person outreach wave.

## Explicit non-goals for native v1

- no public Latarnik ranking, streaks or task points;
- no “publish for a free ticket” mechanic;
- no automatic discounts or guaranteed guestlist;
- no new CMS or CRM;
- no tracking pixels;
- no persisted raw invitation QR/link;
- no automatic mass outreach on deploy.
