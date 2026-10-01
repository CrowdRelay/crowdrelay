//! One person, two roles (P.1).
//!
//! A promoter who books the band, a journalist who reviewed the record and a
//! photographer who shot the show are all `beacons` rows. If any of
//! them also wants the dates, they are a `fans` row as well — and nothing in
//! the system knew those were the same person.
//!
//! They were kept apart for a reason: a fan is somebody who consented to be
//! written to, and a beacon is somebody the band works with. Merging the two
//! concepts would make the consent boundary mushy, which is the one boundary
//! that must not be. So this does not merge them. It joins them **by the
//! address, for reading only**, and answers three questions the operator could
//! not ask before:
//!
//! * which of the people we already work with also hear the dates;
//! * which of them could be asked, with a reason to hand;
//! * which of them must not be asked right now, and why.
//!
//! The contact governor already spans both roles — it is keyed on
//! `(workspace_id, normalized_contact)`, so a promoter written to on Monday is
//! shielded whichever role Friday's letter would have used. That spine is why
//! this is a read and a rule rather than a new sending path: the protection
//! already exists, and nothing here may weaken it.
//!
//! The eligibility rule itself is `crowdrelay_domain::latarnik_invite`, so
//! "never cold, once ever, never on top of business, never empty-handed" is
//! testable without a database.

use crowdrelay_application::IdempotencyKey;
use crowdrelay_application::autopilot::AutopilotActionPayload;
use crowdrelay_domain::{BeaconId, WorkspaceId};
use crowdrelay_domain::latarnik_invite::{ContactStanding, InviteDecision, InviteHold, decide};
use crowdrelay_domain::trace::TraceContext;
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

include!("latarnik/approve.rs");

/// Rows returned in one read. An operator reviewing who to invite is reading,
/// not exporting; the count comes back separately so a truncated list still
/// says how much it is hiding.
const MAX_ROWS: i64 = 200;

/// One person the band already works with, and what the two roles say together.
#[derive(Debug, Serialize)]
pub struct DualRoleContact {
    pub beacon_id: Uuid,
    pub display_name: String,
    /// `promoter`, `local_press`, `photographer`, …
    pub role: String,
    pub city: Option<String>,
    pub relationship_score: i32,
    /// True when this address is also an active fan with marketing consent —
    /// the person already hears the dates.
    pub hears_the_dates: bool,
    /// True when the address exists as a fan but without live consent: they
    /// signed up once and are pending or unsubscribed. Never treated as
    /// reachable, and never silently re-subscribed.
    pub known_but_not_consented: bool,
    /// Days since the band last contacted this address about anything, in any
    /// role. `None` means never — which reads as "cold", not "long ago".
    pub days_since_last_contact: Option<i64>,
    pub already_invited: bool,
    /// `true` when the invitation may go out now.
    pub invitable: bool,
    /// Why not, when not. A sentence, not a flag.
    pub hold_reason: Option<String>,
    /// The band ever had an answer from them — a reply outranks a score.
    pub has_replied: bool,
    /// Marked do-not-contact, on the beacon or on the contact governor —
    /// an org-wide opt-out counts the same.
    pub do_not_contact: bool,
    /// The contact takes outreach at all.
    pub accepts_outreach: bool,
    /// They were on the list and unsubscribed — or the newest consent
    /// record is a withdrawal. The one hold that never expires.
    pub previously_opted_out: bool,
    /// Their double opt-in is already in their inbox, unanswered.
    pub opt_in_pending: bool,
    /// The band has looked at what they did lately: a recent, sourced fact is
    /// on file. Without one they are held as "not read yet" and are the work
    /// queue for the research step.
    pub has_research: bool,
}

/// What the read found, with the honest denominator.
#[derive(Debug, Serialize)]
pub struct DualRoleReview {
    pub contacts: Vec<DualRoleContact>,
    /// Everybody the band works with who is contactable at all.
    pub total: i64,
    /// Of those, how many already hear the dates. The number that says whether
    /// this is worth doing at all.
    pub already_hear_the_dates: i64,
    /// How many could be asked right now.
    pub invitable_now: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct Row {
    beacon_id: Uuid,
    display_name: String,
    role: String,
    city: Option<String>,
    relationship_score: i32,
    accepts_outreach: bool,
    do_not_contact: bool,
    governor_do_not_contact: bool,
    has_replied: bool,
    fan_status: Option<String>,
    /// The newest marketing consent record's verdict — `None` when the fan
    /// has never been asked, which is not the same as having said no.
    latest_consent: Option<bool>,
    days_since_last_contact: Option<i64>,
    already_invited: bool,
    has_recent_research: bool,
    /// The person said no in either ledger: a declined or do-not-contact
    /// reply on a beacon campaign, or on the outreach target behind the same
    /// address.
    declined: bool,
}

/// The dual-role read, shared by the review page and the approve path's
/// by-id re-check. One column list and one join shape so the two can never
/// decode differently.
///
/// The fan join is LATERAL because `fans.normalized_email` is not unique: a
/// merged identity leaves a stale row next to the live one, and a plain join
/// would read both and double the contact. The pick prefers the live row —
/// active, then pending, then anything left — so a dead merge remnant can
/// never outrank the identity that actually holds the consent.
///
/// `latest_consent` is deliberately nullable: "never asked" and "said no"
/// are different standings, and COALESCE would file the first under the
/// second.
const DUAL_ROLE_CORE: &str = r#"
    SELECT
        beacon.id AS beacon_id,
        beacon.display_name,
        beacon.beacon_kind AS role,
        city.name AS city,
        beacon.relationship_score,
        beacon.accepts_outreach,
        beacon.do_not_contact,
        -- The governor's opt-out binds org-wide: a reply that said stop
        -- anywhere is a stop here too, not a fresh start under another role.
        COALESCE(governor.do_not_contact, false) AS governor_do_not_contact,
        -- A reply outranks a score, so it is read rather than inferred: an
        -- answer on a beacon campaign, or on the outreach target behind the
        -- same address, counts. Only an answer that leaves the door open does:
        -- `declined` and `do_not_contact` used to count as a relationship
        -- through `<> 'none'`, which would have invited the people who had
        -- just said no.
        (
            EXISTS (
                SELECT 1 FROM beacon_campaigns AS campaign
                WHERE campaign.workspace_id = beacon.workspace_id
                  AND campaign.beacon_id = beacon.id
                  AND campaign.last_reply_disposition IN ('received', 'interested', 'partner')
            )
            OR COALESCE(outreach.replied, false)
        ) AS has_replied,
        (
            EXISTS (
                SELECT 1 FROM beacon_campaigns AS campaign
                WHERE campaign.workspace_id = beacon.workspace_id
                  AND campaign.beacon_id = beacon.id
                  AND campaign.last_reply_disposition IN ('declined', 'do_not_contact')
            )
            OR COALESCE(outreach.refused, false)
        ) AS declined,
        fan.status AS fan_status,
        -- Consent is the newest marketing record and nothing older. A fan
        -- who opted out is `known_but_not_consented`, never "reachable
        -- again because we have their address".
        (
            SELECT consent.granted
            FROM fan_consents AS consent
            WHERE consent.workspace_id = beacon.workspace_id
              AND consent.fan_id = fan.id
              AND consent.purpose = 'marketing'
            ORDER BY consent.recorded_at DESC, consent.id DESC
            LIMIT 1
        ) AS latest_consent,
        -- The last time the band reached this address for any reason at all:
        -- the later of the governor's record and the outreach engine's.
        --
        -- The governor alone is blind to the outreach engine. Production held
        -- 56 governor rows against 485 outbound outreach mails in a month, so
        -- ~120 people the band had mailed repeatedly read as "never contacted"
        -- and were refused as cold: the invitation lane found nobody to ask.
        -- `GREATEST` ignores a NULL side, so either ledger alone is enough.
        -- Whole days, floored in SQL: `EXTRACT` returns NUMERIC and the
        -- decode wants an integer answer, not a fraction of a day nobody
        -- reads.
        FLOOR(EXTRACT(EPOCH FROM (
            $2 - GREATEST(governor.last_outbound_at, outreach.last_at)
        )) / 86400)::bigint
            AS days_since_last_contact,
        COALESCE(governor.last_context = 'latarnik_invite', false) AS already_invited,
        -- Nobody is written to as a stranger: a dated, sourced fact about what
        -- this address did lately (`contact_research`) is the price of a letter.
        -- The window is `crowdrelay_domain::contact_research::HOOK_MAX_AGE_DAYS`,
        -- pinned by a test because a SQL literal cannot import a constant.
        EXISTS (
            SELECT 1 FROM contact_research AS research
            WHERE research.workspace_id = beacon.workspace_id
              AND research.normalized_email = lower(btrim(beacon.contact_email))
              AND research.observed_on <= ($2::timestamptz AT TIME ZONE 'UTC')::date
              AND research.observed_on >= ($2::timestamptz AT TIME ZONE 'UTC')::date - 120
        ) AS has_recent_research
    FROM beacons AS beacon
    LEFT JOIN cities AS city ON city.id = beacon.city_id
    -- The join that did not exist: the same address wearing the other role.
    LEFT JOIN LATERAL (
        SELECT f.id, f.status
        FROM fans AS f
        WHERE f.workspace_id = beacon.workspace_id
          AND f.normalized_email = lower(btrim(beacon.contact_email))
        ORDER BY (f.status = 'active') DESC, (f.status = 'pending') DESC, f.id
        LIMIT 1
    ) AS fan ON true
    LEFT JOIN contact_governor AS governor
      ON governor.workspace_id = beacon.workspace_id
     AND governor.normalized_contact = lower(btrim(beacon.contact_email))
    -- The outreach engine's own record of the same address. Aggregated because
    -- `(workspace_id, contact_email)` is unique only on the raw spelling.
    LEFT JOIN LATERAL (
        SELECT max(target.last_outreach_at) AS last_at,
               bool_or(target.last_reply_disposition IN ('positive', 'received')) AS replied,
               bool_or(target.last_reply_disposition IN ('declined', 'do_not_contact')
                       OR target.do_not_contact) AS refused
        FROM outreach_targets AS target
        WHERE target.workspace_id = beacon.workspace_id
          AND lower(btrim(target.contact_email)) = lower(btrim(beacon.contact_email))
    ) AS outreach ON true
    WHERE beacon.workspace_id = $1
      AND beacon.active
      AND beacon.contact_email IS NOT NULL
"#;

/// Everybody the band works with, with both roles resolved.
///
/// `reason_available` is passed in rather than computed here: whether there is
/// something concrete to tell this person — a date in their city, a shared
/// night, a new record — is a question about the band's calendar, and the
/// caller that has the calendar answers it. Passing `false` makes every row
/// hold with "nothing to offer", which is the correct answer for a band with
/// nothing on.
///
/// # Errors
///
/// Propagates the database error.
/// Everybody the band works with, with both roles resolved.
///
/// `reason_available` is passed in rather than computed here: whether there is
/// something concrete to tell this person — a date in their city, a shared
/// night, a new record — is a question about the band's calendar, and the
/// caller that has the calendar answers it. Passing `false` makes every row
/// hold with "nothing to offer", which is the correct answer for a band with
/// nothing on.
///
/// # Errors
///
/// Propagates the database error.
pub async fn dual_role_review(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
    reason_available: bool,
) -> Result<DualRoleReview, sqlx::Error> {
    // Every row is read, not just the page: the KPI counts must be the true
    // denominator, so `invitable_now` cannot understate what a capped list
    // hides. The returned contacts are still bounded to MAX_ROWS.
    let sql = format!(
        "{DUAL_ROLE_CORE} ORDER BY beacon.relationship_score DESC, beacon.display_name, beacon.id"
    );
    let rows = sqlx::query_as::<_, Row>(&sql)
        .bind(workspace_id)
        .bind(now)
        .fetch_all(pool)
        .await?;

    let total = rows.len() as i64;
    let mut already_hear_the_dates = 0;
    let mut invitable_now = 0;
    let mut contacts = Vec::with_capacity(rows.len().min(MAX_ROWS as usize));

    for row in rows {
        let (contact, hears, invitable) = row_to_contact(&row, reason_available);
        if hears {
            already_hear_the_dates += 1;
        }
        if invitable {
            invitable_now += 1;
        }
        if contacts.len() < MAX_ROWS as usize {
            contacts.push(contact);
        }
    }

    Ok(DualRoleReview {
        contacts,
        total,
        already_hear_the_dates,
        invitable_now,
    })
}

/// One row of the same read, by beacon id — the approve path's lookup. The
/// review page is capped; an id-addressed click must not 404 because the
/// person ranked below the cap.
pub async fn dual_role_contact(
    pool: &PgPool,
    workspace_id: Uuid,
    beacon_id: Uuid,
    now: OffsetDateTime,
    reason_available: bool,
) -> Result<Option<DualRoleContact>, sqlx::Error> {
    let sql = format!("{DUAL_ROLE_CORE} AND beacon.id = $3");
    let row = sqlx::query_as::<_, Row>(&sql)
        .bind(workspace_id)
        .bind(now)
        .bind(beacon_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|row| row_to_contact(&row, reason_available).0))
}


/// The small queue of warm relationships worth learning about now.
///
/// This is not an invitation queue. It asks a narrower question: which people
/// have already earned a relationship, are rested, have not opted out, are not
/// already fans/pending, and have something concrete the act could eventually
/// tell them — but are missing the recent sourced context required for a
/// thoughtful message.
///
/// The domain rule remains authoritative. We reconstruct the standing and ask
/// `decide` for `NeedsResearch`; then we additionally require a real
/// database-backed invite reason rather than the review screen's placeholder.
/// At most eight contacts enter one cycle: research attention is scarce and
/// quality falls before throughput becomes useful.
pub async fn relationship_research_queue(
    pool: &PgPool,
    workspace_id: Uuid,
    now: OffsetDateTime,
) -> Result<Vec<crowdrelay_application::autopilot::RelationshipResearchSnapshot>, sqlx::Error> {
    const MAX_RESEARCH_PER_CYCLE: usize = 8;
    let review = dual_role_review(pool, workspace_id, now, true).await?;
    let mut queue = Vec::new();

    for contact in review.contacts {
        if queue.len() >= MAX_RESEARCH_PER_CYCLE {
            break;
        }
        let standing = ContactStanding {
            display_name: contact.display_name.clone(),
            role: contact.role.clone(),
            city: contact.city.clone(),
            relationship_score: contact.relationship_score,
            has_replied: contact.has_replied,
            do_not_contact: contact.do_not_contact,
            accepts_outreach: contact.accepts_outreach,
            days_since_last_contact: contact.days_since_last_contact,
            already_invited: contact.already_invited,
            already_a_fan: contact.hears_the_dates,
            previously_opted_out: contact.previously_opted_out,
            opt_in_pending: contact.opt_in_pending,
            has_recent_research: contact.has_research,
        };
        if decide(&standing, Some(&PLACEHOLDER_REASON))
            != InviteDecision::Hold(InviteHold::NeedsResearch)
        {
            continue;
        }
        let language = contact_language(pool, workspace_id, contact.beacon_id).await?;
        if invite_reason(pool, workspace_id, contact.beacon_id, language, now)
            .await?
            .is_none()
        {
            continue;
        }
        queue.push(crowdrelay_application::autopilot::RelationshipResearchSnapshot {
            beacon_id: BeaconId::from_uuid(contact.beacon_id),
            display_name: contact.display_name,
            role: contact.role,
            city: contact.city,
            relationship_score: contact.relationship_score,
            has_replied: contact.has_replied,
            days_since_last_contact: contact.days_since_last_contact,
        });
    }
    Ok(queue)
}

/// One decoded row → the standing, the decision and the contact the console
/// renders. `hears`/`invitable` come back separately so the review can count
/// them without re-reading the contact.
fn row_to_contact(row: &Row, reason_available: bool) -> (DualRoleContact, bool, bool) {
    let hears_the_dates =
        row.fan_status.as_deref() == Some("active") && row.latest_consent == Some(true);
    let known_but_not_consented = row.fan_status.is_some() && !hears_the_dates;
    let days_since_last_contact = row.days_since_last_contact.map(|days| days.max(0));
    // An opt-out is a withdrawal somebody made or a bounce the system took —
    // `unsubscribed`/`suppressed`, or a consent record whose newest word was
    // no. `pending` is not leaving: their confirmation is still open, and it
    // gets its own hold rather than borrowing the strongest one.
    let previously_opted_out = matches!(
        row.fan_status.as_deref(),
        Some("unsubscribed") | Some("suppressed")
    ) || row.latest_consent == Some(false);
    let opt_in_pending = row.fan_status.as_deref() == Some("pending");
    let do_not_contact = row.do_not_contact || row.governor_do_not_contact || row.declined;
    let standing = ContactStanding {
        display_name: row.display_name.clone(),
        role: row.role.clone(),
        city: row.city.clone(),
        relationship_score: row.relationship_score,
        has_replied: row.has_replied,
        do_not_contact,
        accepts_outreach: row.accepts_outreach,
        days_since_last_contact,
        already_invited: row.already_invited,
        already_a_fan: hears_the_dates,
        previously_opted_out,
        opt_in_pending,
        has_recent_research: row.has_recent_research,
    };
    // The reason is the caller's to supply; without one every row holds,
    // which is the honest state for a band with nothing on the calendar.
    let decision = decide(&standing, reason_available.then_some(&PLACEHOLDER_REASON));
    let (invitable, hold_reason) = match decision {
        InviteDecision::Send => (true, None),
        InviteDecision::Hold(hold) => (false, Some(hold.message())),
    };
    (
        DualRoleContact {
            beacon_id: row.beacon_id,
            display_name: row.display_name.clone(),
            role: row.role.clone(),
            city: row.city.clone(),
            relationship_score: row.relationship_score,
            hears_the_dates,
            known_but_not_consented,
            days_since_last_contact,
            already_invited: row.already_invited,
            invitable,
            hold_reason,
            has_replied: row.has_replied,
            do_not_contact,
            accepts_outreach: row.accepts_outreach,
            previously_opted_out,
            opt_in_pending,
            has_research: row.has_recent_research,
        },
        hears_the_dates,
        invitable,
    )
}

/// A stand-in for "the caller says there is something to tell them".
///
/// The eligibility rule only asks whether a reason exists, never what it says —
/// the words come from the composer at send time, against the real calendar.
/// Keeping a placeholder here rather than widening the signature means the read
/// cannot accidentally become the thing that decides what a letter claims.
static PLACEHOLDER_REASON: crowdrelay_domain::latarnik_invite::InviteReason =
    crowdrelay_domain::latarnik_invite::InviteReason::RecentRelease {
        title: String::new(),
    };

#[cfg(test)]
mod core_tests {
    use super::DUAL_ROLE_CORE;
    use crowdrelay_domain::contact_research::HOOK_MAX_AGE_DAYS;

    /// The read's recency window is a SQL literal; the rule's is a constant.
    /// They must be the same number.
    #[test]
    fn the_sql_window_is_the_domains_window() {
        assert!(
            DUAL_ROLE_CORE.contains(&format!("::date - {HOOK_MAX_AGE_DAYS}")),
            "the research window in DUAL_ROLE_CORE drifted from HOOK_MAX_AGE_DAYS"
        );
    }
}
