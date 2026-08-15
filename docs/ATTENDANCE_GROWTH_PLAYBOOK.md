# Attendance Growth / Demand Loop

VIRYA's bottleneck is not a lack of places to store contacts. It is **demand and distribution**: enough relevant people must hear about the right show, from enough trusted local surfaces, early enough to decide and buy.

CrowdRelay therefore treats attendance growth as a deterministic show-level loop, not as a generic “post more” task. It combines first-party Signal activation with Beacons, venue/bill cross-promotion, listing hygiene, factual social proof and merch follow-through.

## What we copy from professional promoters

Professional concert promotion repeatedly uses a few mechanisms that do not require a large ad budget:

1. **Distribution hygiene.** The event and ticket link appear consistently wherever the audience already looks instead of living on one band page.
2. **Borrowed local audiences.** Venue, support acts, promoters, scene communities and media are distribution partners, not decoration on a poster.
3. **Several waves, not one launch post.** Announcement, context/story, collaboration, reminder and last-mile messages have different jobs.
4. **Earned media.** Interviews, previews, reviews, radio/TV/podcasts, patronage and ticket giveaways create third-party proof.
5. **Factual social proof.** Real live material, real press, real reviews, real audience activity and real milestones reduce the “unknown band” problem. Never fabricate proof.
6. **First-party activation.** People who already joined Signal, bought a ticket, expressed interest, referred friends or attended before are more valuable than a large cold reach number.
7. **A clear local reason.** “VIRYA has a show in your city and here is why your audience may care” beats a generic band biography.
8. **Post-show continuation.** Attendance, photos, press and merch interest become inputs to the next show instead of disappearing after load-out.

Money can amplify these loops, but it does not replace them. CrowdRelay defaults to free/owned/earned actions. Paid placement remains a separately governed promotion-budget decision.

## The deterministic 60-day loop

The `show_growth` bounded context evaluates one snapshot per upcoming show. Each lever is one-shot/idempotent for that event unless its own domain explicitly models a follow-up.

### T-60 to T-49: coverage before hype

**Free listing sweep**

- verify canonical title/date/venue/ticket URL first;
- make the show healthy on VIRYA's own show page and ticket CTA;
- verify a Bandsintown event with the canonical ticket link, because that same listing can feed Spotify live-event discovery when the ticketing partner is not integrated directly;
- verify Spotify live-event visibility instead of assuming that a Bandsintown/ticketing sync propagated correctly;
- verify a **Songkick Tourbox** event with the canonical ticket link when VIRYA has artist/manager access; Songkick provides another free discovery graph and can distribute artist-managed dates to partners such as Deezer, Bandcamp and SoundCloud;
- treat Bandsintown as a **publish-once distribution source** for supported downstream discovery surfaces (Spotify plus Google/YouTube/Apple/Shazam/Amazon Music) instead of creating duplicate manual posts; verify propagation health and return only real drift/human-only fixes;
- verify that the Bandsintown artist profile is actually linked to the correct Spotify, YouTube Official Artist Channel and Apple Music artist identities; for YouTube, verify the account-level concert/ticketing setting instead of assuming distribution is enabled;
- after Songkick Tourbox publication, verify its downstream discovery graph (including Deezer/Bandcamp/SoundCloud where supported) rather than counting the Tourbox form submission itself as reach;
- submit/update the venue calendar/newsletter where that surface is free and appropriate;
- discover and submit relevant free city/culture and scene calendars for the event market;
- deduplicate before publishing and return the public URLs as executor receipts;
- if a destination needs a CAPTCHA, login, email verification or another human-only step, return a small `manual_steps` checklist (destination, URL, what to do, why it matters) rather than bypassing it or silently dropping the surface;
- never buy placement without a separately approved promotion budget.

This is deliberately a **surface ladder**, not a static list of Polish websites. The executor re-discovers the useful free local calendars for each market. A Wrocław show and a Prague show should not inherit the same destination spreadsheet forever.

**Free audience-capture setup**

After distribution is healthy, use provider-native free capture surfaces without replacing Signal as VIRYA's first-party home:

- keep **VIRYA Signal** as the primary signup/relationship CTA on owned VIRYA pages;
- make sure the Bandsintown event has a usable Smart Link and canonical ticket/event facts;
- use Bandsintown Follow/Signup/Widget capture where it is native and useful, plus its QR surface for printed/social material;
- generate a campaign-attributed **VIRYA Signal / canonical show QR** for the merch table, current shows and partner surfaces where the owner/venue explicitly permits placement; keep the normal Signal consent flow and never buy printing/placement under the free-growth authority;
- when a real presale exists, configure the free Bandsintown presale signup/alert flow; when ticketing is not yet on sale, prefer a truthful supported reminder instead of inventing a presale;
- never import Signal contacts into Bandsintown merely to inflate a provider list. Any third-party import needs its own explicit consent/policy decision;
- provider login/2FA/CAPTCHA steps become precise `manual_steps` rather than brittle browser automation.

This gives VIRYA another discovery/follower graph while preserving the much more valuable direct Signal relationship.

**Beacon discovery**

- find local radio, press, TV, reviewers, creators, photographers, promoters, venues, scene partners, patrons and communities;
- prioritize real scene-trust surfaces: metal media/podcasts, independent music programmes, venue/promoter/support-band networks, record stores, rehearsal studios, music shops, tattoo/alternative-fashion scene businesses, student/culture portals, moderated metal communities and local live creators;
- require a public source URL and a verifiable contact path; never scrape private member lists or personal data;
- ingest discoveries as unverified candidates first;
- only CrowdRelay may qualify/authorize them for contact.

### T-49 to T-35: borrow the local network

**Partner cross-promo**

Use verified venue, line-up, promoter and scene-partner Beacons to request **one concrete action per message**, such as:

- correct event listing and canonical ticket link;
- venue newsletter/calendar placement;
- co-post/repost from the venue or promoter;
- where the partner agrees, Facebook Page event co-host/calendar relay and an Instagram Collab/co-post so the event reaches the partner's existing local audience;
- support-act / bill cross-promotion so every act contributes its local audience;
- local scene/community listing, including relevant moderated metal groups/forums only when the member/community rules allow it;
- ticket giveaway with a verified partner;
- a shared short-form live clip when the partner has a suitable channel.

The executor cannot silently replace the requested offer with another one or promise reciprocal placement outside configured authority. Community posting is manual or moderator-approved: no cold-bot posting, scraping, rule bypass or repetitive group spam. The purpose is to borrow already-relevant local audiences, not to manufacture generic reach.

### T-42 / T-28 / T-14: earned-media Beacon waves

Beacon offers are typed by relationship rather than using one PR blast:

- radio → airplay, interview, ticket giveaway;
- local press → preview, interview, review, patronage;
- TV → local segment/interview;
- reviewer → review/interview;
- creator → co-post, short-form clip, ticket giveaway;
- photographer → photo access/live gallery;
- promoter → support slot, future booking, cross-promo;
- venue → event listing, co-post, newsletter, cross-promo;
- scene partner/community → local listing, cross-promo, giveaway;
- patron → patronage, preview, giveaway.

Gemini may make the authorized message natural and locally relevant. It must not choose the target, invent a connection, invent a review, change an offer or bypass suppression/approval.

### T-35: turn Signal into distribution

**Fan ambassadors**

When there is enough local Signal/referral activity, activate people who have already demonstrated referral intent rather than mailing every subscriber. CrowdRelay now emits a small **relay pack** with the campaign: invite one to three people who genuinely like heavy music, share the one canonical event link on a personal surface, and optionally share it in a local metal community only when the fan is already a member and the rules permit it. Existing referral identity may be used by the delivery template; no scraped contacts, mass DMs, automated group posting or newly invented financial incentive is allowed. The CTA is concrete: bring/share with one relevant person, not “please spam your friends”.

### T-21: factual social-proof relay

Prepare a small channel-ready packet using only verified first-party facts:

- strong current live photo/video;
- actual review/interview/patronage links;
- actual festival/final/award facts;
- real local or audience response where available;
- canonical ticket CTA.

If VIRYA does **not** yet have enough third-party proof, the fallback is a good local story/context packet (why this show is relevant, what is unusual about the live concept, what the audience can expect) — never fake social proof. The executor may resize/reformat/rewrite the caption, but it may not claim “sold out”, “viral”, audience size, stream counts or endorsements unless those facts are supplied.

### T-18: free provider-native fan push

Use free distribution to followers VIRYA has already earned on third-party live-music surfaces:

- send/schedule a **Bandsintown Post** targeted to the event city or existing event RSVPs; provider-native posts can generate follower notifications without paid reach;
- when the currently verified free quota allows it, use **Bandsintown Email Builder** for local/event-relevant followers. Never silently cross into paid Promoted Email/Boost;
- reuse a strong current **live clip / Featured Video** on the provider surface when available; do not manufacture filler content just to occupy a slot;
- create a human step for **Spotify Artist Pick** so the upcoming concert can sit at the top of the artist profile until the show. This is intentionally a guided operator action because it requires Spotify for Artists account access rather than an invented automation API;
- when available, create a human step for a **YouTube artist Post** to the existing subscriber audience and a **Bandcamp Community** message to existing followers. These are earned-audience surfaces, not permission to scrape/import contacts or automate private/community posting;
- use one canonical ticket URL and already-approved event/story facts;
- record provider-confirmed send/schedule/reach metadata, or an explicit skip reason/manual step.

This wave does not replace Signal e-mail. It exploits a second audience graph whose users already self-selected for live-music discovery.

### T-14: merch before load-in

**Merch preorder / buyer offer**

Target consented ticket buyers, not a cold list. Make the offer operationally easy — for example reserve/preorder and collect at the show when the commerce surface supports it. The purpose is to convert existing show intent into merch intent before the table competes with post-show fatigue.

Do not claim event-attributed merch lift until the data model can actually attribute the purchase to that event.

### T-10 to show: high-intent last mile

Target consented people who expressed interest in the event but have not bought. The message should answer the final friction: date/venue, why this show is worth leaving home for, ticket link and any verified urgency. Do not manufacture scarcity.

The sales-pace model makes this more important when a show is behind its configured sold-capacity trajectory.

### Post-show: compound the win

**Post-show merch follow-up** goes only to consented attendees when there is enough attendance to justify the campaign. Keep it short and connected to the actual show. Photos/reviews/Beacon replies become reusable proof and relationship history for the next event.


## Why these free levers are worth automating

The research pattern behind this loop is consistent across professional and independent promotion:

- established promoters keep the event present on several ticket/discovery surfaces and repeatedly create editorial context around shows instead of relying on one announcement;
- supports, venues and scene partners contribute **borrowed audiences** that the headliner/band does not own;
- independent promoters repeatedly emphasize relationship work and local-market saturation — especially where a smaller market has fewer competing events and easier access to local media;
- Bandsintown gives artists free event pages, ticket/RSVP surfaces, email/signup tooling and event widgets; its current distribution network can fan one event out to major discovery surfaces such as Spotify, Google, YouTube, Apple and Shazam, while Spotify can surface those events to local listeners without a separate VIRYA ad spend;
- Bandsintown also provides free follower messaging and a monthly free Email Builder allowance; the executor must verify the current free quota before using it and must never cross into paid Boost/Promoted Campaigns without separate budget authority;
- Spotify Artist Pick is a high-value free manual surface because a concert can be featured at the top of the artist profile until the event/sell-out; CrowdRelay therefore creates a guided operator step instead of pretending this account-level action is safely automatable;
- the valuable first-party loop is therefore: get the event correctly distributed, capture intent/RSVP/Signal identity, follow up at the right moment, and compound the relationship after the show.

CrowdRelay automates the **repeatable mechanics**. It does not pretend that software can replace a great bill, a good venue, strong music, a memorable live show or real human relationships.

## Earned-media seeds for Poland

These are **discovery seeds, not hard-coded recipients**. Every campaign run must re-discover the current public page/contact and pass normal verification/suppression before outreach.

- KVLT — independent rock/metal editorial; explicitly offers event/release patronage and lists news, reminders, social promotion, giveaways, live coverage, reviews and interviews as possible collaboration outputs.
- Polskie Radio Czwórka / `Nie Ma Lekko` — editorial format explicitly featuring young and lesser-known Polish rock/metal bands, with interviews and live material.
- Skoncertowana — concert-oriented independent media/patronage/reviews/interviews.
- RockMetalNews — rock/metal editorial and marketing/interview/video cooperation surface.
- Polska Płyta / Polska Muzyka — Polish-music editorial/patronage/interview surface.

This seed list should grow through verified Beacon discovery and relationship outcomes, not through a manually maintained mass-mail spreadsheet.

## When to ask a human

Human approval remains appropriate when an action creates a meaningful commitment: paid placement, exclusivity, contractual patronage terms, unusual giveaway value, rights/licensing, a sensitive editorial response, or anything outside the configured authority.

The canonical action is assigned skill-first and load-aware, visible in staff web and Signal staff mode, and the owner gets a concise reminder email. Email is only the notification channel; CrowdRelay remains the task/state source of truth.

## Measurement

The current closed-loop measurement is deliberately conservative:

- `show_ticket_revenue_7d` — seven-day event ticket gross after an attendance lever, compared with its baseline/effect evidence;
- action/executor receipts — what actually happened externally and where;
- Beacon relationship outcomes — received/interested/partner/declined/do-not-contact;
- first-party campaign delivery/engagement through the existing communication ledger.

Merch is not falsely attributed to a show when the underlying purchase data cannot prove that attribution. Until event-level merch attribution exists, merch gross remains a broader proxy and the system reports that limitation instead of manufacturing causality.

## Safety / anti-spam invariants

- public-source and verifiable-contact requirement for discovered Beacons;
- verification before contact;
- deduplication and durable idempotency;
- do-not-contact/suppression always wins;
- bounded touch count and minimum contact gap;
- consent required for first-party fan email;
- no fabricated social proof;
- no autonomous paid placement;
- no Gemini/LLM recipient selection or business-policy decisions;
- provider-confirmed receipt is the external completion truth.
### Warm intros and permissioned fan proof

The free partner lane may ask a **verified local Beacon for one warm introduction** to a relevant scene contact. The introduction is consent-based: private contact details are never forwarded without permission, and a newly introduced destination remains unverified until its public identity or direct consent is established. Social proof can also reuse fan-shot live photos/clips, but only with explicit repost permission and credit; private or closed-group media is never harvested as campaign material.

