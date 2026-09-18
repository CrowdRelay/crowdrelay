//! One person, two roles (P.1).
//!
//! A promoter who books the band, a journalist who reviewed the record and a
//! photographer who shot the show are all `viryaos_beacons` rows. If any of
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

use crowdrelay_domain::latarnik_invite::{ContactStanding, InviteDecision, decide};
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

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
    has_replied: bool,
    fan_status: Option<String>,
    consented: bool,
    days_since_last_contact: Option<i64>,
    already_invited: bool,
    total_count: i64,
}

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
    let rows = sqlx::query_as::<_, Row>(
        r#"
        SELECT
            beacon.id AS beacon_id,
            beacon.display_name,
            beacon.beacon_kind AS role,
            city.name AS city,
            beacon.relationship_score,
            beacon.accepts_outreach,
            beacon.do_not_contact,
            -- A reply outranks a score, so it is read rather than inferred: any
            -- beacon campaign on this person that got an answer counts.
            EXISTS (
                SELECT 1 FROM viryaos_beacon_campaigns AS campaign
                WHERE campaign.workspace_id = beacon.workspace_id
                  AND campaign.beacon_id = beacon.id
                  AND campaign.last_reply_disposition <> 'none'
            ) AS has_replied,
            fan.status AS fan_status,
            -- Consent is the newest marketing record and nothing older. A fan
            -- who opted out is `known_but_not_consented`, never "reachable
            -- again because we have their address".
            COALESCE((
                SELECT consent.granted
                FROM fan_consents AS consent
                WHERE consent.workspace_id = fan.workspace_id
                  AND consent.fan_id = fan.id
                  AND consent.purpose = 'marketing'
                ORDER BY consent.recorded_at DESC, consent.id DESC
                LIMIT 1
            ), false) AS consented,
            -- The governor spans both roles: this is the last time the band
            -- reached this address for any reason at all.
            -- Whole days, floored in SQL: `EXTRACT` returns NUMERIC and the
            -- decode wants an integer answer, not a fraction of a day nobody
            -- reads.
            FLOOR(EXTRACT(EPOCH FROM ($2 - governor.last_outbound_at)) / 86400)::bigint
                AS days_since_last_contact,
            COALESCE(governor.last_context = 'latarnik_invite', false) AS already_invited,
            count(*) OVER ()::bigint AS total_count
        FROM viryaos_beacons AS beacon
        LEFT JOIN cities AS city ON city.id = beacon.city_id
        -- The join that did not exist: the same address wearing the other role.
        LEFT JOIN fans AS fan
          ON fan.workspace_id = beacon.workspace_id
         AND fan.normalized_email = lower(btrim(beacon.contact_email))
        LEFT JOIN viryaos_contact_governor AS governor
          ON governor.workspace_id = beacon.workspace_id
         AND governor.normalized_contact = lower(btrim(beacon.contact_email))
        WHERE beacon.workspace_id = $1
          AND beacon.active
          AND beacon.contact_email IS NOT NULL
        ORDER BY beacon.relationship_score DESC, beacon.display_name, beacon.id
        LIMIT $3
        "#,
    )
    .bind(workspace_id)
    .bind(now)
    .bind(MAX_ROWS)
    .fetch_all(pool)
    .await?;

    let total = rows.first().map_or(0, |row| row.total_count);
    let mut already_hear_the_dates = 0;
    let mut invitable_now = 0;
    let mut contacts = Vec::with_capacity(rows.len());

    for row in rows {
        let hears_the_dates = row.fan_status.as_deref() == Some("active") && row.consented;
        let known_but_not_consented = row.fan_status.is_some() && !hears_the_dates;
        if hears_the_dates {
            already_hear_the_dates += 1;
        }
        let days_since_last_contact = row.days_since_last_contact.map(|days| days.max(0));
        let standing = ContactStanding {
            display_name: row.display_name.clone(),
            role: row.role.clone(),
            city: row.city.clone(),
            relationship_score: row.relationship_score,
            has_replied: row.has_replied,
            do_not_contact: row.do_not_contact,
            accepts_outreach: row.accepts_outreach,
            days_since_last_contact,
            already_invited: row.already_invited,
            already_a_fan: hears_the_dates,
            // The fan row exists and consent is not live: they were on the list
            // and left. The other role is not a way back in.
            previously_opted_out: known_but_not_consented,
        };
        // The reason is the caller's to supply; without one every row holds,
        // which is the honest state for a band with nothing on the calendar.
        let decision = decide(&standing, reason_available.then_some(&PLACEHOLDER_REASON));
        let (invitable, hold_reason) = match decision {
            InviteDecision::Send => {
                invitable_now += 1;
                (true, None)
            }
            InviteDecision::Hold(hold) => (false, Some(hold.message())),
        };
        contacts.push(DualRoleContact {
            beacon_id: row.beacon_id,
            display_name: row.display_name,
            role: row.role,
            city: row.city,
            relationship_score: row.relationship_score,
            hears_the_dates,
            known_but_not_consented,
            days_since_last_contact,
            already_invited: row.already_invited,
            invitable,
            hold_reason,
        });
    }

    Ok(DualRoleReview {
        contacts,
        total,
        already_hear_the_dates,
        invitable_now,
    })
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
