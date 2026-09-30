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
//!
//! Privacy boundary: the two passes above read headers only. A message's
//! *body* is fetched in exactly one case — an inbound reply from a contact
//! this workspace pitched in the last 60 days — because what a pitched
//! curator answered is the substance triage needs. The fetch is a separate
//! `format=full` call with a fields whitelist, made only after the pitched
//! gate returns a target; a sender nobody pitched never triggers it. The
//! body lands on the interaction's `metadata.reply_text` and queues a
//! `reply_classifications` row for the first-party classifier, the same as
//! an operator filing the reply by hand.

use std::collections::HashMap;
use std::time::Instant;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use crowdrelay_domain::drive_contacts::extract_header_contacts;
use crowdrelay_infra::outreach_mail::{
    MailDirection, MailTouch, message_recorded, outreach_addresses, pitched_reply_targets,
    record_mail_touch, record_reply_text, unbodied_replies,
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::gmail_contacts_sync::GmailContactsSyncWorker;
use crate::release_source_sync::strip_tags;

/// Contacts searched per cycle. The sync runs hourly, so a few hundred
/// contacts are all reconciled within a working day, at one list call each.
const RECONCILE_BATCH: usize = 40;
/// Messages read per contact per search — the newest first.
const MESSAGES_PER_CONTACT: u32 = 20;
/// An inbound touch counts as a reply only when the sender was pitched
/// inside this window — older outbound history makes it mail, not an answer.
const PITCHED_WINDOW_DAYS: i32 = 60;
/// Recorded replies still missing `reply_text` fetched per cycle — the
/// backfill for messages the ledger wrote before bodies were captured,
/// bounded so a fresh deployment does not stampede Gmail.
const REPLY_BODY_BATCH: i64 = 10;
/// Replies newer than this get their bodies read — the date reply capture
/// shipped; earlier mail stays headers-only history.
const REPLY_BODY_SINCE: OffsetDateTime = time::macros::datetime!(2026-09-28 00:00:00 UTC);

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

/// A `format=full` message read for its body — only the fields the reply
/// capture asks for, so the whitelist the request sends is the whole truth.
#[derive(Debug, serde::Deserialize)]
struct FullMessage {
    payload: Option<BodyPart>,
}

#[derive(Debug, serde::Deserialize)]
struct BodyPart {
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    body: Option<BodyData>,
    parts: Option<Vec<BodyPart>>,
}

#[derive(Debug, serde::Deserialize)]
struct BodyData {
    data: Option<String>,
}

/// The reply's own words: the first `text/plain` part, or a stripped
/// `text/html` one when the sender's client never wrote plain text. The
/// quoted tail — `>` lines and the "On … wrote:" header every client
/// prepends — is cut, and the result capped: the ledger stores what they
/// said, not the thread's whole history.
const REPLY_TEXT_LIMIT: usize = 4000;

fn part_text(part: &BodyPart, wanted: &str) -> Option<String> {
    if part.mime_type.as_deref() == Some(wanted)
        && let Some(data) = part.body.as_ref().and_then(|b| b.data.as_deref())
        && let Ok(bytes) = URL_SAFE_NO_PAD.decode(data.trim_end_matches('='))
    {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    part.parts
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find_map(|p| part_text(p, wanted))
}

/// The quote-attribution line, in the locales the mailbox actually sees:
/// "On 29 Sep … wrote:", "W dniu … pisze:", "Am … schrieb:",
/// "El … escribió:". Matching head and tail covers every date and sender
/// rendering without listing them.
fn is_quote_header(content_lower: &str) -> bool {
    const HEADERS: [(&str, &str); 4] = [
        ("on ", "wrote:"),
        ("w dniu ", "pisze:"),
        ("am ", "schrieb:"),
        ("el ", "escribió:"),
    ];
    HEADERS
        .iter()
        .any(|(head, tail)| content_lower.starts_with(head) && content_lower.ends_with(tail))
}

/// Where a quote header starts inside a stripped line, or `None`. A header
/// matches when a head token sits at a word boundary and its tail follows
/// anywhere after it — an html reply can pack the attribution and the quoted
/// text on one source line. The scan runs on the original casing so the
/// returned byte index is safe to slice `content` at — lowercasing first
/// would shift offsets wherever a char expands (ß, İ).
fn quote_header_start(content: &str) -> Option<usize> {
    const HEADERS: [(&str, &str); 4] = [
        ("on ", "wrote:"),
        ("w dniu ", "pisze:"),
        ("am ", "schrieb:"),
        ("el ", "escribió:"),
    ];
    let mut best: Option<usize> = None;
    for (head, tail) in HEADERS {
        for (i, _) in content.char_indices() {
            let at_boundary = i == 0
                || content
                    .get(..i)
                    .and_then(|before| before.chars().next_back())
                    .is_some_and(|c| !c.is_alphanumeric());
            let head_here = content
                .get(i..)
                .and_then(|rest| rest.get(..head.len()))
                .is_some_and(|h| h.eq_ignore_ascii_case(head));
            let tail_after = content
                .get(i + head.len()..)
                .is_some_and(|rest| rest.to_lowercase().contains(tail));
            if at_boundary && head_here && tail_after {
                best = Some(best.map_or(i, |b: usize| b.min(i)));
                break;
            }
        }
    }
    best
}

fn cap_reply(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() > REPLY_TEXT_LIMIT {
        trimmed.chars().take(REPLY_TEXT_LIMIT).collect()
    } else {
        trimmed.to_owned()
    }
}

/// Plain text: everything from the quote header down is the sender's client
/// quoting us, not them answering; `>`-prefixed lines go too — some clients
/// interleave quotes mid-reply.
fn cut_quoted_history(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if is_quote_header(&line.trim_end().to_lowercase()) {
            break;
        }
        if !line.trim_start().starts_with('>') {
            out.push_str(line);
            out.push('\n');
        }
    }
    cap_reply(&out)
}

/// HTML: the quote header sits inside a `<div>`, so each line is checked on
/// its stripped text content — and cut mid-line when the attribution shares
/// a line with the quoted text. `&gt;` is how a `>` quote marker survives
/// html encoding.
fn cut_html_reply(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    for line in html.lines() {
        let content = strip_tags(line);
        if content.is_empty() {
            continue;
        }
        match quote_header_start(&content) {
            Some(start) => {
                if let Some(head) = content.get(..start) {
                    out.push_str(head.trim_end());
                    out.push(' ');
                }
                break;
            }
            None => {
                let lead = content.trim_start();
                if lead.starts_with('>') || lead.starts_with("&gt;") {
                    continue;
                }
                out.push_str(&content);
                out.push(' ');
            }
        }
    }
    cap_reply(&out)
}

/// The reply text of a full message, or `None` when no part carries a
/// readable body.
fn extract_reply_text(message: &FullMessage) -> Option<String> {
    let payload = message.payload.as_ref()?;
    let text = part_text(payload, "text/plain")
        .map(|raw| cut_quoted_history(&raw))
        .or_else(|| part_text(payload, "text/html").map(|html| cut_html_reply(&html)))?;
    (!text.is_empty()).then_some(text)
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
    /// every message it opens. When the touch is an inbound reply from a
    /// contact this workspace pitched recently, the message's body is read
    /// once and stored on the interaction — see the module doc for why this
    /// is the only body fetch.
    pub(crate) async fn record_scanned_touch(
        &self,
        connection_id: Uuid,
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
        // The pitched gate runs before the write so the disposition it reads
        // is still the one the target held before this reply — what the
        // classification records as `previous_disposition`.
        let pitched = if touch.direction == MailDirection::Inbound {
            pitched_reply_targets(
                &self.pool,
                self.workspace_id,
                &touch.counterparts,
                PITCHED_WINDOW_DAYS,
            )
            .await
            .map_err(|error| format!("pitched-reply read failed: {error}"))?
        } else {
            Vec::new()
        };
        let touched = record_mail_touch(&self.pool, self.workspace_id, &touch)
            .await
            .map_err(|error| format!("outreach ledger write failed: {error}"))?;
        // A touch that wrote nothing was already recorded — its body was
        // fetched (or deliberately skipped) the first time.
        if touched > 0 && !pitched.is_empty() {
            self.capture_reply_body(connection_id, message_id, &pitched, touch.at)
                .await?;
        }
        Ok(touched)
    }

    /// Fetches one replied message's body and stores it on each pitched
    /// target's inbound interaction. Errors propagate to the caller's retry;
    /// the `reply_text` metadata guard makes the retry safe.
    async fn capture_reply_body(
        &self,
        connection_id: Uuid,
        message_id: &str,
        pitched: &[crowdrelay_infra::outreach_mail::PitchedReplyTarget],
        occurred_at: OffsetDateTime,
    ) -> Result<(), String> {
        let read: FullMessage = self
            .get(
                connection_id,
                &format!("https://gmail.googleapis.com/gmail/v1/users/me/messages/{message_id}"),
                &[
                    ("format", "full".to_owned()),
                    (
                        "fields",
                        "payload(mimeType,body/data,\
                         parts(mimeType,body/data,parts(mimeType,body/data)))"
                            .to_owned(),
                    ),
                ],
            )
            .await?;
        let Some(text) = extract_reply_text(&read) else {
            return Ok(());
        };
        for target in pitched {
            record_reply_text(
                &self.pool,
                self.workspace_id,
                message_id,
                target,
                &text,
                occurred_at,
            )
            .await
            .map_err(|error| format!("reply_text write failed: {error}"))?;
        }
        Ok(())
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
        // One unreadable message must not abort the contacts behind it — a
        // persistent failure would otherwise starve every later target in the
        // batch. First error is kept and reported once everything else ran.
        let mut first_error: Option<String> = None;
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
                    Err(error) => {
                        tracing::warn!(
                            %error, message_id = %message.id,
                            "ledger reconcile: message read failed; continuing"
                        );
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        continue;
                    }
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
                        connection_id,
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
        // Backfill: replies the ledger recorded before bodies were captured
        // carry no `reply_text`. A few per cycle reads them through the same
        // path fresh replies take; once drained this list is empty and the
        // pass is a no-op.
        let unbodied = unbodied_replies(
            &self.pool,
            self.workspace_id,
            REPLY_BODY_SINCE,
            PITCHED_WINDOW_DAYS,
            REPLY_BODY_BATCH,
        )
        .await
        .map_err(|error| format!("unbodied replies read failed: {error}"))?;
        // One unreadable reply must not starve the ones behind it — the list
        // is oldest-first, so a failing message would head-of-line block the
        // whole backfill forever.
        for (message_id, target, occurred_at) in unbodied {
            if let Err(error) = self
                .capture_reply_body(connection_id, &message_id, &[target], occurred_at)
                .await
            {
                tracing::warn!(%error, %message_id, "reply-body backfill: message failed; continuing");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
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

    fn b64(text: &str) -> String {
        URL_SAFE_NO_PAD.encode(text.as_bytes())
    }

    fn message_with(parts: serde_json::Value) -> FullMessage {
        serde_json::from_value::<FullMessage>(serde_json::json!({ "payload": parts }))
            .expect("a full message parses")
    }

    #[test]
    fn the_reply_text_is_the_plain_part_without_its_quoted_tail() {
        let message = message_with(serde_json::json!({
            "mimeType": "multipart/mixed",
            "parts": [
                { "mimeType": "text/plain",
                  "body": { "data": b64("Dzięki, fajny numer.\n\nOn 29 Sep 2026, at 18:22, Virya <band@virya.example> wrote:\n> nowy klip jest super") } },
                { "mimeType": "text/html",
                  "body": { "data": b64("<div>Dzięki</div>") } }
            ]
        }));
        assert_eq!(
            extract_reply_text(&message).as_deref(),
            Some("Dzięki, fajny numer.")
        );
    }

    #[test]
    fn an_html_only_reply_falls_back_to_stripped_text() {
        let message = message_with(serde_json::json!({
            "mimeType": "text/html",
            "body": { "data": b64("<div dir=\"ltr\">Tak, podeślijcie materiały.</div>\n<div class=\"gmail_quote\"><div>On Mon, 29 Sep 2026 Virya wrote:</div><blockquote>&gt; pitch</blockquote></div>") }
        }));
        assert_eq!(
            extract_reply_text(&message).as_deref(),
            Some("Tak, podeślijcie materiały.")
        );
    }

    #[test]
    fn a_polish_quote_header_ends_the_reply() {
        let text = cut_quoted_history(
            "Jasne, wrzucimy.\nW dniu 29 września 2026 Virya pisze:\nreszta cytatu",
        );
        assert_eq!(text, "Jasne, wrzucimy.");
    }

    #[test]
    fn a_body_over_the_cap_is_trimmed() {
        let text = cut_quoted_history(&"a".repeat(5000));
        assert_eq!(text.chars().count(), REPLY_TEXT_LIMIT);
    }

    #[test]
    fn a_message_with_no_body_yields_nothing() {
        let message = message_with(serde_json::json!({
            "mimeType": "multipart/mixed",
            "parts": [{ "mimeType": "application/pdf", "body": { "size": 10 } }]
        }));
        assert!(extract_reply_text(&message).is_none());
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
