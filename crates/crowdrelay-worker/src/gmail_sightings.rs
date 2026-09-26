//! Mailbox direction: who wrote to whom, and when.
//!
//! Split out of `gmail_contacts_sync.rs` — the scan file sits under the
//! size ratchet, and direction detection is a self-contained decision a
//! test can drive without a Gmail account. Two sightings per message:
//! an inbound one (`From` = a contact) and an outbound one (`From` = the
//! tenant, `To`/`Cc` = the contacts). Neither reads a body; both run on
//! headers and `internalDate` alone.

use crowdrelay_domain::drive_contacts::extract_header_contacts;
use time::OffsetDateTime;

/// An inbound sighting is one `From` contact that is not the tenant's own
/// mailbox (`extract_header_contacts` already excludes it), on a message
/// whose `internalDate` parses to an instant. The band's own outbound mail,
/// a missing or malformed header, and a missing date all record nothing.
pub(crate) fn inbound_sighting(
    from_header: &str,
    self_email: &str,
    internal_date_ms: Option<&str>,
) -> Option<(String, OffsetDateTime)> {
    let ms = internal_date_ms?.parse::<i64>().ok()?;
    let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()?;
    match extract_header_contacts(&[from_header.to_string()], self_email).as_slice() {
        [contact] => Some((contact.email.clone(), at)),
        _ => None,
    }
}

/// The outbound sighting: `From` parses to the tenant's own mailbox, so
/// every contact named in `To`/`Cc` is an address the band just wrote to.
/// `extract_header_contacts` already drops the tenant's own address from
/// the recipients, so nothing is recorded for mail sent to oneself.
/// Returns the recipient emails and the message instant, or `None` when
/// the mail is not from the tenant or carries no parseable timestamp.
pub(crate) fn outbound_sightings(
    from_header: &str,
    recipient_headers: &[String],
    self_email: &str,
    internal_date_ms: Option<&str>,
) -> Option<(Vec<String>, OffsetDateTime)> {
    let ms = internal_date_ms?.parse::<i64>().ok()?;
    let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()?;
    // `extract_header_contacts` filters the sender out by definition, so
    // it cannot tell "From is the tenant" from "From is malformed". Ask it
    // with a sentinel self address that matches nothing, then compare —
    // the parse rules for "Name <addr>" stay in one place.
    let from_is_self = extract_header_contacts(&[from_header.to_string()], "\u{0}")
        .iter()
        .any(|c| c.email == self_email.trim().to_ascii_lowercase());
    if !from_is_self {
        return None;
    }
    let emails: Vec<String> = extract_header_contacts(recipient_headers, self_email)
        .into_iter()
        .map(|c| c.email)
        .collect();
    if emails.is_empty() {
        return None;
    }
    Some((emails, at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inbound_from_header_records_the_counterpartys_sighting() {
        let ms = "1727740800000";
        let (email, at) =
            inbound_sighting("Promoter <promo@venue.pl>", "band@virya.music", Some(ms))
                .expect("one non-self From contact is a sighting");
        assert_eq!(email, "promo@venue.pl");
        assert_eq!(
            at,
            OffsetDateTime::from_unix_timestamp_nanos(1_727_740_800_000_000_000).expect("in range")
        );
    }

    #[test]
    fn a_sent_message_records_each_recipient_sighting() {
        let ms = "1727740800000";
        let (emails, at) = outbound_sightings(
            "Virya <band@virya.music>",
            &["Promoter <promo@venue.pl>, Other <other@club.pl>".to_string()],
            "band@virya.music",
            Some(ms),
        )
        .expect("a message From the tenant is an outbound sighting");
        let mut emails = emails;
        emails.sort_unstable();
        assert_eq!(emails, vec!["other@club.pl", "promo@venue.pl"]);
        assert_eq!(
            at,
            OffsetDateTime::from_unix_timestamp_nanos(1_727_740_800_000_000_000).expect("in range")
        );
    }

    #[test]
    fn outbound_sightings_refuse_inbound_mail_and_self_copies() {
        // Mail from anyone else is the inbound side's business.
        assert_eq!(
            outbound_sightings(
                "Promoter <promo@venue.pl>",
                &["Virya <band@virya.music>".to_string()],
                "band@virya.music",
                Some("1727740800000")
            ),
            None
        );
        // Mail the tenant sent to itself has no counterparty to record.
        assert_eq!(
            outbound_sightings(
                "Virya <band@virya.music>",
                &["Virya <band@virya.music>".to_string()],
                "band@virya.music",
                Some("1727740800000")
            ),
            None
        );
        // No timestamp, no sighting — the same rule the inbound side uses.
        assert_eq!(
            outbound_sightings(
                "Virya <band@virya.music>",
                &["promo@venue.pl".to_string()],
                "band@virya.music",
                None
            ),
            None
        );
    }
}
