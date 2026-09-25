//! Keeping the outreach ledger current from the connected Gmail.
//!
//! Two paths feed `crowdrelay_infra::outreach_mail::record_mail_touch`:
//!
//! - **every scanned message**: the contacts sync already reads each new
//!   message's headers; a message between the mailbox and an outreach
//!   contact is now also a ledger touch, so a reply the act sends from its
//!   own Gmail moves the conversation out of "your turn" by itself, and an
//!   answer that arrives moves it in;
//! - **reconciliation**: the incremental scan only sees mail newer than its
//!   cursor, so each cycle also searches the mailbox for a rotating batch of
//!   outreach contacts (a year back) and records any message the ledger does
//!   not hold yet. That backfills the history the sheets never logged and
//!   catches anything the cursor missed. It reads headers only, like the scan.

use std::collections::HashMap;
use std::time::Instant;

use crowdrelay_domain::drive_contacts::extract_header_contacts;
use crowdrelay_infra::outreach_mail::{
    MailDirection, MailTouch, message_recorded, outreach_addresses, record_mail_touch,
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::gmail_contacts_sync::GmailContactsSyncWorker;

/// Contacts searched per cycle. The sync runs hourly, so a few hundred
/// contacts are all reconciled within a working day, at one list call each.
const RECONCILE_BATCH: usize = 40;
/// Messages read per contact per search — the newest first.
const MESSAGES_PER_CONTACT: u32 = 20;

/// The touch one message makes, if it makes one: outbound to every
/// recipient when the mailbox sent it, inbound from the sender otherwise.
/// `None` when the date does not parse, the sender cannot be read, or the
/// message names nobody on the other side.
#[must_use]
pub fn mail_touch(
    message_id: &str,
    from_header: &str,
    recipient_headers: &[String],
    self_email: &str,
    internal_date_ms: Option<&str>,
) -> Option<MailTouch> {
    let ms = internal_date_ms?.trim().parse::<i64>().ok()?;
    let at = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()?;
    let self_norm = self_email.trim().to_ascii_lowercase();
    // Read the sender without excluding anyone, then compare: the header
    // parser drops the mailbox's own address, which would make "sent by us"
    // indistinguishable from "unreadable".
    let sender = extract_header_contacts(&[from_header.to_owned()], "")
        .into_iter()
        .next()?
        .email;
    let (direction, counterparts) = if sender == self_norm {
        let recipients: Vec<String> = extract_header_contacts(recipient_headers, &self_norm)
            .into_iter()
            .map(|contact| contact.email)
            .collect();
        (MailDirection::Outbound, recipients)
    } else {
        (MailDirection::Inbound, vec![sender])
    };
    if counterparts.is_empty() {
        return None;
    }
    let mut counterparts = counterparts;
    counterparts.sort();
    Some(MailTouch {
        message_id: message_id.to_owned(),
        direction,
        counterparts,
        at,
    })
}

#[derive(Debug, serde::Deserialize)]
struct MessageList {
    messages: Option<Vec<MessageRef>>,
}

#[derive(Debug, serde::Deserialize)]
struct MessageRef {
    id: String,
}

#[derive(Debug, serde::Deserialize)]
struct MessageHeaders {
    #[serde(rename = "internalDate")]
    internal_date: Option<String>,
    payload: Option<HeaderPayload>,
}

#[derive(Debug, serde::Deserialize)]
struct HeaderPayload {
    headers: Option<Vec<Header>>,
}

#[derive(Debug, serde::Deserialize)]
struct Header {
    name: String,
    value: String,
}

/// What one reconciliation pass did.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    pub contacts_searched: usize,
    pub messages_read: usize,
    pub touches_recorded: u64,
}

/// Picks the next batch: contacts never reconciled in this process first,
/// then the longest since. `last` is the worker's in-memory record; a
/// restart forgets it, which only means one extra pass of idempotent reads.
fn next_batch(
    addresses: Vec<(Uuid, String)>,
    last: &HashMap<Uuid, Instant>,
    size: usize,
) -> Vec<(Uuid, String)> {
    let mut ordered = addresses;
    ordered.sort_by_key(|(id, _)| last.get(id).copied());
    ordered.truncate(size);
    ordered
}

impl GmailContactsSyncWorker {
    /// Records the touch a scanned message makes. Called by the scan for
    /// every message it opens.
    pub(crate) async fn record_scanned_touch(
        &self,
        message_id: &str,
        from_header: &str,
        recipient_headers: &[String],
        self_email: &str,
        internal_date_ms: Option<&str>,
    ) -> Result<u64, String> {
        let Some(touch) = mail_touch(
            message_id,
            from_header,
            recipient_headers,
            self_email,
            internal_date_ms,
        ) else {
            return Ok(0);
        };
        record_mail_touch(&self.pool, self.workspace_id, &touch)
            .await
            .map_err(|error| format!("outreach ledger write failed: {error}"))
    }

    /// One reconciliation pass over the next batch of outreach contacts.
    pub(crate) async fn reconcile_outreach_ledger(
        &self,
        connection_id: Uuid,
        self_email: &str,
    ) -> Result<ReconcileReport, String> {
        let addresses = outreach_addresses(&self.pool, self.workspace_id)
            .await
            .map_err(|error| format!("outreach addresses read failed: {error}"))?;
        let batch = {
            let last = self
                .reconciled
                .lock()
                .map_err(|_| "reconcile state poisoned".to_owned())?;
            next_batch(addresses, &last, RECONCILE_BATCH)
        };
        let mut report = ReconcileReport::default();
        for (target_id, address) in batch {
            let query = format!("from:{address} OR to:{address} OR cc:{address} newer_than:365d");
            let listed: MessageList = self
                .get(
                    connection_id,
                    "https://gmail.googleapis.com/gmail/v1/users/me/messages",
                    &[
                        ("q", query),
                        ("maxResults", MESSAGES_PER_CONTACT.to_string()),
                        ("includeSpamTrash", "false".to_owned()),
                    ],
                )
                .await?;
            report.contacts_searched += 1;
            for message in listed.messages.unwrap_or_default() {
                if message_recorded(&self.pool, self.workspace_id, &message.id)
                    .await
                    .map_err(|error| format!("outreach ledger read failed: {error}"))?
                {
                    continue;
                }
                let read: MessageHeaders = match self
                    .get(
                        connection_id,
                        &format!(
                            "https://gmail.googleapis.com/gmail/v1/users/me/messages/{}",
                            message.id
                        ),
                        &[
                            ("format", "metadata".to_owned()),
                            ("metadataHeaders", "From".to_owned()),
                            ("metadataHeaders", "To".to_owned()),
                            ("metadataHeaders", "Cc".to_owned()),
                            ("fields", "internalDate,payload.headers".to_owned()),
                        ],
                    )
                    .await
                {
                    Ok(read) => read,
                    // A message deleted between the search and the read.
                    Err(error) if error.ends_with("status=404") => continue,
                    Err(error) => return Err(error),
                };
                report.messages_read += 1;
                let headers = read.payload.and_then(|p| p.headers).unwrap_or_default();
                let values = |name: &str| -> Vec<String> {
                    headers
                        .iter()
                        .filter(|h| h.name.eq_ignore_ascii_case(name))
                        .map(|h| h.value.clone())
                        .collect()
                };
                let from = values("From").into_iter().next().unwrap_or_default();
                let recipients: Vec<String> =
                    values("To").into_iter().chain(values("Cc")).collect();
                report.touches_recorded += self
                    .record_scanned_touch(
                        &message.id,
                        &from,
                        &recipients,
                        self_email,
                        read.internal_date.as_deref(),
                    )
                    .await?;
            }
            if let Ok(mut last) = self.reconciled.lock() {
                last.insert(target_id, Instant::now());
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "band@virya.example";

    #[test]
    fn a_message_the_mailbox_sent_touches_every_recipient() {
        let touch = mail_touch(
            "m1",
            "Virya <Band@Virya.example>",
            &[
                "Zine <editor@zine.pl>, radio@station.fm".to_owned(),
                "band@virya.example".to_owned(),
            ],
            ME,
            Some("1758800000000"),
        )
        .expect("an outbound touch");
        assert_eq!(touch.direction, MailDirection::Outbound);
        assert_eq!(touch.counterparts, ["editor@zine.pl", "radio@station.fm"]);
        assert_eq!(touch.at.unix_timestamp(), 1_758_800_000);
    }

    #[test]
    fn a_message_a_contact_sent_is_their_answer() {
        let touch = mail_touch(
            "m2",
            "Editor <editor@zine.pl>",
            &["band@virya.example".to_owned()],
            ME,
            Some("1758800000000"),
        )
        .expect("an inbound touch");
        assert_eq!(touch.direction, MailDirection::Inbound);
        assert_eq!(touch.counterparts, ["editor@zine.pl"]);
    }

    #[test]
    fn nothing_readable_makes_no_touch() {
        assert!(mail_touch("m3", "", &[], ME, Some("1758800000000")).is_none());
        assert!(
            mail_touch("m4", "editor@zine.pl", &[], ME, None).is_none(),
            "no date, no touch"
        );
        assert!(
            mail_touch(
                "m5",
                "band@virya.example",
                &["band@virya.example".to_owned()],
                ME,
                Some("1")
            )
            .is_none(),
            "a note to self names nobody"
        );
    }

    #[test]
    fn the_batch_starts_with_contacts_never_reconciled() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let c = Uuid::now_v7();
        let mut last = HashMap::new();
        last.insert(a, Instant::now());
        let batch = next_batch(
            vec![(a, "a@x".into()), (b, "b@x".into()), (c, "c@x".into())],
            &last,
            2,
        );
        let ids: Vec<Uuid> = batch.into_iter().map(|(id, _)| id).collect();
        assert!(!ids.contains(&a), "the reconciled contact waits: {ids:?}");
        assert_eq!(ids.len(), 2);
    }
}
