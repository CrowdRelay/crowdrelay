//! Reading the band's outreach workbooks — the `OUTREACH` tabs the band
//! fills in by hand.
//!
//! Three dialects feed the same history: `VIRYA_MASTER.xlsx` (English
//! headers — `Sent_At`, `Reply_At`, `Result`, `Recipient`),
//! `PROMO.xlsx` (Polish headers — `Data_wysłania`, `Data_odpowiedzi`,
//! `Wynik`, `Kontakt`), and the compact human `SCOUT.OUTREACH_LOG`.
//! MASTER/PROMO carry explicit `Outreach_ID` values; SCOUT carries the
//! counterparty + subject + date instead, from which this reader derives a
//! stable id. The header pins keep all three ahead of the contact fallback.
//!
//! # What a row means
//!
//! A row is a *send log entry*, not a contact. `DRAFT` rows in `PROMO`
//! are letters that were planned and never went out — they produce no
//! interaction at all, because an email the band did not send is not
//! history, it is intent. A row becomes:
//!
//! - an **outbound** interaction when the sheet records that it was sent
//!   (`Sent_At`/`Data_wysłania` set, or a status that only a sent letter
//!   can reach — `SENT`, `REPLIED`, `CONTACTED`, `PUBLISHED`, …);
//! - an **inbound reply** interaction when it records that they answered
//!   (`Reply_At`/`Data_odpowiedzi` set, `Reply_Type` of `HUMAN`, or a
//!   replied-family status).
//!
//! One sheet row can produce both, keyed `book:id` and `book:id:reply`
//! — the same convention the original one-off import wrote, so re-running
//! the durable importer against those rows is a refresh, not a second copy.
//!
//! The reply's disposition comes from `reply_verdict_map`: the sheet's own
//! verdict (`Response_Type` for `PROMO`, `Result` for `master`) decided
//! deterministically. Verdicts the map does not settle keep `received`
//! and are left for the triage queue — a guessed disposition is worse than
//! a queue.
//!
//! Parse-only like its sibling readers: no IO, no clock, no writes.

use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Iso8601, macros::format_description};

use crate::outreach::OutreachReplyDisposition;
use crate::reply_verdict_map::{ImportedVerdict, map_sheet_verdict};

/// Canonical column names after normalization. Both dialects fold into
/// this vocabulary — the Polish spellings are listed under their English
/// meaning so one reader serves both books.
pub mod columns {
    pub const OUTREACH_ID: &str = "outreach_id";
    /// `Entity_ID` on master, `Lead_ID` on promo — the registry-side key
    /// the row belongs to.
    pub const ENTITY_REF: &str = "entity_ref";
    /// `Nazwa` on promo; master has no name column (its `Recipient` is the
    /// address itself).
    pub const NAME: &str = "name";
    /// `Recipient` on master, `Kontakt` on promo — the address when the
    /// cell parses as one.
    pub const CONTACT: &str = "contact";
    pub const CHANNEL: &str = "channel";
    /// `Purpose` / `Cel` — what the letter was for.
    pub const PURPOSE: &str = "purpose";
    pub const SUBJECT: &str = "subject";
    pub const SENT_AT: &str = "sent_at";
    pub const STATUS: &str = "status";
    pub const REPLY_AT: &str = "reply_at";
    /// `Reply_Type` on master — `HUMAN` says a person answered, not what
    /// they decided. Kept for provenance; the verdict chain reads it last
    /// because an unknown value there fails to human review anyway.
    pub const REPLY_TYPE: &str = "reply_type";
    /// `Response_Type` on promo — the column the one-off importer read
    /// `POSITIVE` out of. A separate canonical name from `reply_type`:
    /// the two columns answer different questions in their own sheets.
    pub const RESPONSE_TYPE: &str = "response_type";
    /// `Result` on master, `Wynik` on promo — the sheet's verdict cell.
    pub const RESULT: &str = "result";
    pub const FOLLOWUP_DUE: &str = "followup_due";
    pub const NEXT_STEP: &str = "next_step";
    pub const NOTES: &str = "notes";
    pub const GMAIL_MESSAGE_ID: &str = "gmail_message_id";
    pub const GMAIL_THREAD_ID: &str = "gmail_thread_id";
    pub const SEGMENT: &str = "segment";
    pub const SOURCE_SYSTEM: &str = "source_system";
    pub const SOURCE_ID: &str = "source_id";
    /// `Created_At` on master — a sheet-side timestamp, used only as the
    /// last fallback when a sent row carries no `Sent_At` of its own.
    pub const CREATED_AT: &str = "created_at";
    /// `Send_Guard_Key` on master — the sheet's own send dedupe string,
    /// preserved in metadata verbatim.
    pub const SEND_GUARD_KEY: &str = "send_guard_key";
}

/// Statuses that can only be reached by a letter that actually went out.
/// `DRAFT`, `QUEUED`, `PLANNED` and everything else claim nothing — a send
/// the sheet does not prove is a send the importer does not mint.
const SENT_STATUSES: &[&str] = &[
    "sent",
    "replied",
    "answered",
    "contacted",
    "delivered",
    "published",
    "wysłano",
    "wyslano",
    "wysłane",
    "odpowiedziano",
];

/// Statuses that mean an answer came back — belt and braces beside
/// `Reply_At`, for rows where the date cell was left blank.
const REPLIED_STATUSES: &[&str] = &["replied", "answered", "odpowiedziano"];

/// Which workbook dialect a claimed sheet speaks — decided from the
/// header, not the file name, so a renamed copy or a `BACKUP` export
/// still dedupes onto the same `source_key` namespace (`master:ORC-…`,
/// `promo:OUT-…`, or `scout:<derived-id>`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutreachBook {
    /// `VIRYA_MASTER` shape — `Recipient`, `Gmail_Thread_ID`, `Result`.
    Master,
    /// `PROMO` shape — `Nazwa`, `Kontakt`, `Wynik`, `Response_Type`.
    Promo,
    /// Human `SCOUT.OUTREACH_LOG` shape — a compact history table with
    /// recipient, public email, subject/thread, date, action, result/status
    /// and reason/next-step. It has no explicit row id, so the parser derives
    /// a deterministic one from the stable message identity fields.
    Scout,
}

impl OutreachBook {
    /// The `source_key` prefix the stored interactions carry.
    #[must_use]
    pub fn source_key_prefix(self) -> &'static str {
        match self {
            Self::Master => "master",
            Self::Promo => "promo",
            Self::Scout => "scout",
        }
    }
}

/// One parsed log row. Option fields are exactly that — a cell the sheet
/// left blank asserts nothing, and downstream reads `None` as "not
/// recorded", never as zero or empty.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OutreachLogEntry {
    /// The sheet's own row identity — the dedupe key the interactions
    /// carry (`{book}:{outreach_id}` / `{book}:{outreach_id}:reply`).
    /// MASTER/PROMO require the sheet to supply this. SCOUT derives it
    /// deterministically from recipient/email + subject + date, so a rescan
    /// refreshes the same interaction instead of inventing another.
    pub outreach_id: String,
    /// `Entity_ID`/`Lead_ID` — kept for audit and metadata, not a join.
    pub entity_ref: Option<String>,
    /// Display name when the sheet carries one (promo's `Nazwa`).
    pub name: Option<String>,
    /// The counterparty address — `Recipient`/`Kontakt` — when the cell
    /// parses as an email. Non-email values are kept nowhere: a name in
    /// the contact column is not a route.
    pub contact: Option<String>,
    pub channel: Option<String>,
    pub purpose: Option<String>,
    pub subject: Option<String>,
    /// Raw `Sent_At`/`Data_wysłania` text — parsed on demand by
    /// [`OutreachLogEntry::sent_at`], preserved raw for metadata.
    pub sent_at_raw: Option<String>,
    pub status: Option<String>,
    pub reply_at_raw: Option<String>,
    pub reply_type: Option<String>,
    /// The verdict cell — `Result`/`Wynik`.
    pub result: Option<String>,
    /// `Response_Type` — promo's verdict column; the reply disposition
    /// reads it before `result`, matching the order the original import
    /// resolved them (`POSITIVE` lived here, not in `Wynik`).
    pub response_type: Option<String>,
    pub followup_due_raw: Option<String>,
    pub next_step: Option<String>,
    pub notes: Option<String>,
    pub gmail_message_id: Option<String>,
    pub gmail_thread_id: Option<String>,
    pub segment: Option<String>,
    pub source_system: Option<String>,
    pub source_id: Option<String>,
    /// `Created_At` raw text — the timestamp fallback for sent rows whose
    /// `Sent_At` cell was left blank.
    pub created_at_raw: Option<String>,
    /// `Send_Guard_Key` — the band's own dedupe string, kept verbatim.
    pub send_guard_key: Option<String>,
}

impl OutreachLogEntry {
    /// The sheet proves the letter went out: a send timestamp, or a
    /// status only a sent letter can reach.
    #[must_use]
    pub fn was_sent(&self) -> bool {
        if self.sent_at().is_some() {
            return true;
        }
        self.status
            .as_deref()
            .map(|status| SENT_STATUSES.contains(&status.trim().to_lowercase().as_str()))
            .unwrap_or(false)
    }

    /// The sheet says an answer came back.
    #[must_use]
    pub fn was_replied(&self) -> bool {
        if self.replied_at().is_some() {
            return true;
        }
        if self
            .reply_type
            .as_deref()
            .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("human"))
        {
            return true;
        }
        self.status
            .as_deref()
            .map(|status| REPLIED_STATUSES.contains(&status.trim().to_lowercase().as_str()))
            .unwrap_or(false)
    }

    /// `Sent_At`/`Data_wysłania` as a timestamp. The sheets carry wall
    /// times with no zone; they are read as UTC — the convention the
    /// original import wrote, and the only honest reading a zone-less
    /// cell has.
    #[must_use]
    pub fn sent_at(&self) -> Option<OffsetDateTime> {
        sheet_datetime(self.sent_at_raw.as_deref()?)
    }

    /// `Reply_At`/`Data_odpowiedzi` as a timestamp, same convention.
    #[must_use]
    pub fn replied_at(&self) -> Option<OffsetDateTime> {
        sheet_datetime(self.reply_at_raw.as_deref()?)
    }

    /// `Created_At` as a timestamp — the last-ditch clock for a sent row
    /// whose own `Sent_At` is blank.
    #[must_use]
    pub fn created_at(&self) -> Option<OffsetDateTime> {
        sheet_datetime(self.created_at_raw.as_deref()?)
    }

    /// What `occurred_at` an outbound interaction records: the sheet's
    /// send time when it carries one, else the row's own creation stamp.
    /// `None` refuses the write — minting a send with no honest clock
    /// would order it against history at whatever time the import ran.
    #[must_use]
    pub fn send_occurred_at(&self) -> Option<OffsetDateTime> {
        self.sent_at().or_else(|| self.created_at())
    }

    /// What `occurred_at` an inbound reply records: the reply's own time,
    /// then the send's, then the row's creation stamp.
    #[must_use]
    pub fn reply_occurred_at(&self) -> Option<OffsetDateTime> {
        self.replied_at()
            .or_else(|| self.sent_at())
            .or_else(|| self.created_at())
    }

    /// The sheet's verdict for the reply: `Response_Type` first (promo's
    /// classification column), then `Result`/`Wynik`, then `Reply_Type`
    /// last — `HUMAN` there settles nothing and the map sends it to a
    /// human anyway, so reading it cannot hurt.
    #[must_use]
    pub fn verdict(&self) -> Option<&str> {
        self.response_type
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| self.result.as_deref().filter(|v| !v.trim().is_empty()))
            .or_else(|| self.reply_type.as_deref().filter(|v| !v.trim().is_empty()))
    }

    /// The disposition the reply row carries — the verdict decided, or
    /// `Received` when the verdict does not settle it.
    #[must_use]
    pub fn reply_disposition(&self) -> OutreachReplyDisposition {
        match self.verdict().map(map_sheet_verdict) {
            Some(ImportedVerdict::Terminal(disposition)) => disposition,
            _ => OutreachReplyDisposition::Received,
        }
    }

    /// Whether the verdict leaves the reply for a human — the flag the
    /// importer uses to mint a triage row.
    #[must_use]
    pub fn needs_review(&self) -> bool {
        !matches!(
            self.verdict().map(map_sheet_verdict),
            Some(ImportedVerdict::Terminal(_))
        )
    }
}

/// Why a row was not usable. Numbered for the sheet, not the grid —
/// a spreadsheet's own row number is what the operator sees. SCOUT may
/// omit an explicit id only when its stable identity fields can derive one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutreachLogRefusal {
    /// No `Outreach_ID`: the row cannot be told apart from its own
    /// re-import, so it is dropped rather than written once a day forever.
    MissingId,
}

impl OutreachLogRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::MissingId => {
                "a log row with no Outreach_ID cannot be told apart from its own re-import"
                    .to_owned()
            }
        }
    }
}

/// What one grid yielded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutreachLogReport {
    /// Which dialect the header declared — the `source_key` namespace
    /// every row in the report mints under.
    pub book: OutreachBook,
    pub entries: Vec<OutreachLogEntry>,
    /// `(1-based sheet row, refusal)` — the number the operator sees.
    pub refusals: Vec<(usize, OutreachLogRefusal)>,
    /// Non-empty rows examined, refused or not.
    pub rows_read: usize,
}

/// Whether a header row is an outreach log. MASTER/PROMO pin on
/// `Outreach_ID` + send/reply/verdict columns. Human SCOUT pins on its
/// complete seven-column compact history shape; a plain contact list still
/// cannot satisfy either contract.
#[must_use]
pub fn is_outreach_log(header: &[String]) -> bool {
    let has = |name: &str| {
        header
            .iter()
            .any(|cell| canonical_column(cell) == Some(name))
    };
    let durable_book = has(columns::OUTREACH_ID)
        && has(columns::SENT_AT)
        && has(columns::REPLY_AT)
        && (has(columns::RESULT) || has(columns::REPLY_TYPE));
    // SCOUT intentionally keeps a compact operator-facing history table.
    // Its seven fields are specific enough to claim without mistaking an
    // ordinary contact list for sent history.
    let scout_book = !has(columns::OUTREACH_ID)
        && has(columns::NAME)
        && has(columns::CONTACT)
        && has(columns::SUBJECT)
        && has(columns::SENT_AT)
        && has(columns::STATUS)
        && has(columns::RESULT)
        && has(columns::NEXT_STEP);
    durable_book || scout_book
}

/// The column this header cell names, in the shared vocabulary.
/// Normalised like the sibling readers — case, spaces and hyphens fold;
/// the Polish spellings keep their diacritics because the sheets write
/// them and nobody else does.
#[must_use]
pub fn canonical_column(cell: &str) -> Option<&'static str> {
    let cell = cell
        .trim()
        .to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    match cell.as_str() {
        "outreach_id" | "outreach" => Some(columns::OUTREACH_ID),
        "entity_id" | "lead_id" => Some(columns::ENTITY_REF),
        "nazwa" | "display_name" | "recipient_organization" => Some(columns::NAME),
        // `Recipient` on master holds the address — it is this sheet's
        // contact column, not its name column.
        "recipient" | "kontakt" | "contact_email" | "email" | "public_email" => {
            Some(columns::CONTACT)
        },
        "channel" | "kanał" | "kanal" => Some(columns::CHANNEL),
        "purpose" | "cel" => Some(columns::PURPOSE),
        "subject" | "temat" | "subject_thread" => Some(columns::SUBJECT),
        "sent_at" | "data_wysłania" | "data_wyslania" | "wysłano" | "wyslano" | "date" => {
            Some(columns::SENT_AT)
        }
        "status" | "action" => Some(columns::STATUS),
        "reply_at" | "data_odpowiedzi" | "answered_at" => Some(columns::REPLY_AT),
        "reply_type" => Some(columns::REPLY_TYPE),
        "response_type" => Some(columns::RESPONSE_TYPE),
        "result" | "wynik" | "outcome" | "result_status" => Some(columns::RESULT),
        "followup_due" | "follow_up_due" => Some(columns::FOLLOWUP_DUE),
        "next_step" | "następny_krok" | "nastepny_krok" | "reason_next_step" => {
            Some(columns::NEXT_STEP)
        },
        "notes" | "uwagi" | "note" => Some(columns::NOTES),
        "gmail_message_id" | "message_id" => Some(columns::GMAIL_MESSAGE_ID),
        "gmail_thread_id" | "thread_id" => Some(columns::GMAIL_THREAD_ID),
        "segment" => Some(columns::SEGMENT),
        "source_system" => Some(columns::SOURCE_SYSTEM),
        "source_id" => Some(columns::SOURCE_ID),
        "created_at" | "data_utworzenia" => Some(columns::CREATED_AT),
        "send_guard_key" | "guard_key" => Some(columns::SEND_GUARD_KEY),
        _ => None,
    }
}

fn scout_outreach_id(
    index: &std::collections::BTreeMap<&str, usize>,
    row: &[String],
) -> Option<String> {
    let value = |name: &str| {
        index
            .get(name)
            .and_then(|cell| row.get(*cell))
            .map(String::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase()
    };
    let identity = [
        value(columns::NAME),
        value(columns::CONTACT),
        value(columns::SUBJECT),
        value(columns::SENT_AT),
    ]
    .join("\0");
    if identity.chars().all(|character| character == '\0') {
        return None;
    }
    let digest = Sha256::digest(identity.as_bytes());
    Some(
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    )
}

fn scout_status(action: Option<&str>, result: Option<&str>) -> Option<String> {
    let action = action?.trim();
    if action.is_empty() {
        return None;
    }
    let lower = action.to_lowercase();
    let result = result.unwrap_or("").trim().to_lowercase();
    if lower.contains("not sent") {
        return Some("DRAFT".to_owned());
    }
    if lower.contains("positive reply")
        || lower.contains("warm contact")
        || lower.contains("accepted")
        || result.contains("accepted")
        || result.contains("positive reply")
    {
        return Some("REPLIED".to_owned());
    }
    if lower.contains("already sent") {
        return Some("SENT".to_owned());
    }
    Some(action.to_owned())
}

/// Reads one whole grid as an outreach log, or declines it.
///
/// `None` means the header is not this sheet's — the caller tries its
/// other readers. `Some` means the header matched; every non-empty row
/// below it parsed into an entry or refused with a row number.
#[must_use]
pub fn extract_outreach_log(grid: &[Vec<String>]) -> Option<OutreachLogReport> {
    let (header, rows) = grid.split_first()?;
    if !is_outreach_log(header) {
        return None;
    }
    // The dialect is a header fact: promo's `Nazwa`/`Kontakt`/`Wynik`/
    // `Response_Type` spellings are the tell, since master's `Recipient`
    // column claims `contact` too but a promo sheet never writes one.
    let has = |name: &str| {
        header
            .iter()
            .any(|cell| canonical_column(cell) == Some(name))
    };
    let scout_shape = !has(columns::OUTREACH_ID)
        && has(columns::NAME)
        && has(columns::CONTACT)
        && has(columns::SUBJECT)
        && has(columns::SENT_AT)
        && has(columns::STATUS)
        && has(columns::RESULT)
        && has(columns::NEXT_STEP);
    let book = if scout_shape {
        OutreachBook::Scout
    } else if has(columns::RESPONSE_TYPE) || has(columns::NAME) {
        OutreachBook::Promo
    } else {
        OutreachBook::Master
    };
    let mut index: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for (i, cell) in header.iter().enumerate() {
        if let Some(name) = canonical_column(cell) {
            index.entry(name).or_insert(i);
        }
    }
    fn cell_at<'a>(
        index: &std::collections::BTreeMap<&str, usize>,
        row: &'a [String],
        name: &str,
    ) -> Option<&'a str> {
        index
            .get(name)
            .and_then(|i| row.get(*i))
            .map(String::as_str)
    }
    let clean = |value: Option<&str>| -> Option<String> {
        let value = value?.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("n/a") {
            return None;
        }
        Some(value.to_owned())
    };
    let capped = |value: Option<String>, limit: usize| -> Option<String> {
        value.map(|v| v.chars().take(limit).collect())
    };

    let mut report = OutreachLogReport {
        book,
        entries: Vec::new(),
        refusals: Vec::new(),
        rows_read: 0,
    };
    for (offset, row) in rows.iter().enumerate() {
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        report.rows_read += 1;
        let row_number = offset + 2;
        let outreach_id = match clean(cell_at(&index, row, columns::OUTREACH_ID)) {
            Some(id) => id,
            None if book == OutreachBook::Scout => {
                let Some(id) = scout_outreach_id(&index, row) else {
                    report
                        .refusals
                        .push((row_number, OutreachLogRefusal::MissingId));
                    continue;
                };
                id
            }
            None => {
                report
                    .refusals
                    .push((row_number, OutreachLogRefusal::MissingId));
                continue;
            }
        };
        // An address-shaped cell is the contact route; anything else
        // (a name, a note) asserts nothing about where the letter went.
        let contact = clean(cell_at(&index, row, columns::CONTACT))
            .filter(|cell| cell.contains('@') && cell.len() <= 320);
        let result = capped(clean(cell_at(&index, row, columns::RESULT)), 200);
        let status = if book == OutreachBook::Scout {
            scout_status(
                cell_at(&index, row, columns::STATUS),
                result.as_deref(),
            )
        } else {
            capped(clean(cell_at(&index, row, columns::STATUS)), 60)
        };
        report.entries.push(OutreachLogEntry {
            outreach_id,
            entity_ref: capped(clean(cell_at(&index, row, columns::ENTITY_REF)), 96),
            name: capped(clean(cell_at(&index, row, columns::NAME)), 240),
            contact,
            channel: capped(clean(cell_at(&index, row, columns::CHANNEL)), 60),
            purpose: capped(clean(cell_at(&index, row, columns::PURPOSE)), 500),
            subject: capped(clean(cell_at(&index, row, columns::SUBJECT)), 500),
            sent_at_raw: capped(clean(cell_at(&index, row, columns::SENT_AT)), 60),
            status,
            reply_at_raw: capped(clean(cell_at(&index, row, columns::REPLY_AT)), 60),
            reply_type: capped(clean(cell_at(&index, row, columns::REPLY_TYPE)), 60),
            result,
            response_type: capped(clean(cell_at(&index, row, columns::RESPONSE_TYPE)), 200),
            followup_due_raw: capped(clean(cell_at(&index, row, columns::FOLLOWUP_DUE)), 60),
            next_step: capped(clean(cell_at(&index, row, columns::NEXT_STEP)), 500),
            notes: capped(clean(cell_at(&index, row, columns::NOTES)), 2000),
            gmail_message_id: capped(clean(cell_at(&index, row, columns::GMAIL_MESSAGE_ID)), 120),
            gmail_thread_id: capped(clean(cell_at(&index, row, columns::GMAIL_THREAD_ID)), 120),
            segment: capped(clean(cell_at(&index, row, columns::SEGMENT)), 120),
            source_system: if book == OutreachBook::Scout {
                Some("scout".to_owned())
            } else {
                capped(clean(cell_at(&index, row, columns::SOURCE_SYSTEM)), 60)
            },
            source_id: capped(clean(cell_at(&index, row, columns::SOURCE_ID)), 160),
            created_at_raw: capped(clean(cell_at(&index, row, columns::CREATED_AT)), 60),
            send_guard_key: capped(clean(cell_at(&index, row, columns::SEND_GUARD_KEY)), 200),
        });
    }
    Some(report)
}

/// The sheet's wall-time forms: `2026-07-22 12:41`, `2026-07-22 12:41:33`,
/// a bare `2026-07-22` (midnight), or a full ISO instant with a zone.
/// Zone-less values are read as UTC — the sheets do not record one, and
/// the import that seeded these rows already stamped them that way.
fn sheet_datetime(raw: &str) -> Option<OffsetDateTime> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(at) = OffsetDateTime::parse(text, &Iso8601::PARSING) {
        return Some(at);
    }
    for format in [
        format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
        format_description!("[year]-[month]-[day] [hour]:[minute]"),
    ] {
        if let Ok(parsed) = time::PrimitiveDateTime::parse(text, &format) {
            return Some(parsed.assume_utc());
        }
    }
    if let Ok(day) = time::Date::parse(text, &format_description!("[year]-[month]-[day]")) {
        return Some(day.midnight().assume_utc());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn master_header() -> Vec<String> {
        [
            "Outreach_ID",
            "Entity_ID",
            "Contact_ID",
            "Opportunity_ID",
            "Review_ID",
            "Event_ID",
            "Campaign_ID",
            "Purpose",
            "Channel",
            "Recipient",
            "Subject",
            "Sent_At",
            "Gmail_Message_ID",
            "Gmail_Thread_ID",
            "Status",
            "Reply_At",
            "Reply_Type",
            "Result",
            "Followup_Due",
            "Source_System",
            "Source_ID",
            "Send_Guard_Key",
            "Created_At",
            "Notes",
        ]
        .iter()
        .map(|h| h.to_string())
        .collect()
    }

    fn promo_header() -> Vec<String> {
        [
            "Outreach_ID",
            "Lead_ID",
            "Nazwa",
            "Kanał",
            "Kontakt",
            "Cel",
            "Data_wysłania",
            "Status",
            "Data_odpowiedzi",
            "Followup_due",
            "Wynik",
            "Następny_krok",
            "Uwagi",
            "Segment",
            "Angle",
            "Personalization_1_5",
            "Response_Type",
            "Time_to_reply_days",
            "Conversion",
            "Value_1_5",
            "Failure_Reason",
            "Learning_Tag",
            "Cost_PLN",
            "Outcome_Date",
            "Draft_Text",
            "Entity_ID",
            "AUDIT_STATUS",
        ]
        .iter()
        .map(|h| h.to_string())
        .collect()
    }

    fn scout_header() -> Vec<String> {
        [
            "Recipient / Organization",
            "Public Email",
            "Subject / Thread",
            "Date",
            "Action",
            "Result / Status",
            "Reason / Next Step",
        ]
        .iter()
        .map(|header| header.to_string())
        .collect()
    }

    fn row(header: &[String], values: &[(&str, &str)]) -> Vec<String> {
        header
            .iter()
            .map(|cell| {
                let canonical = canonical_column(cell).unwrap_or("");
                values
                    .iter()
                    .find(|(name, _)| *name == canonical)
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn the_master_header_is_claimed() {
        assert!(is_outreach_log(&master_header()));
    }

    #[test]
    fn the_promo_header_is_claimed() {
        assert!(is_outreach_log(&promo_header()));
    }

    #[test]
    fn the_scout_header_is_claimed() {
        assert!(is_outreach_log(&scout_header()));
    }

    #[test]
    fn scout_history_derives_stable_ids_and_send_semantics() {
        let header = scout_header();
        let sent = row(
            &header,
            &[
                ("name", "Metal Forever"),
                ("contact", "redakce@metalforever.info"),
                ("subject", "VIRYA / concert news"),
                ("sent_at", "2026-08-10"),
                ("status", "Already sent before this run"),
                ("result", "Czeka na odpowiedź"),
                ("next_step", "Dedupe from Gmail."),
            ],
        );
        let replied = row(
            &header,
            &[
                ("name", "Metal Gentleman Promotion"),
                ("contact", "promotion@metalgentleman.com"),
                ("subject", "Virya - Metalgentleman submission"),
                ("sent_at", "2026-08-08"),
                ("status", "Existing positive reply"),
                ("result", "Accepted / coverage promised"),
                ("next_step", "Do not cold-pitch duplicate."),
            ],
        );
        let draft = row(
            &header,
            &[
                ("name", "Euroblast Festival"),
                ("contact", "su@euroblast.net"),
                ("subject", "Band application"),
                ("sent_at", "2026-08-11"),
                ("status", "NOT SENT"),
                ("result", "Needs application package"),
            ],
        );
        let report =
            extract_outreach_log(&vec![header.clone(), sent.clone(), replied, draft])
                .expect("SCOUT outreach log is claimed");
        assert_eq!(report.book, OutreachBook::Scout);
        assert_eq!(report.entries.len(), 3);
        assert!(report.entries[0].was_sent());
        assert!(!report.entries[0].was_replied());
        assert!(report.entries[1].was_sent());
        assert!(report.entries[1].was_replied());
        assert!(!report.entries[2].was_sent());
        assert_eq!(report.entries[0].source_system.as_deref(), Some("scout"));

        let first_id = report.entries[0].outreach_id.clone();
        let mut refreshed = sent;
        let result_index = header
            .iter()
            .position(|cell| canonical_column(cell) == Some(columns::RESULT))
            .expect("result column");
        refreshed[result_index] = "Still waiting".to_owned();
        let refreshed_report =
            extract_outreach_log(&vec![header, refreshed]).expect("SCOUT row re-parses");
        assert_eq!(
            first_id, refreshed_report.entries[0].outreach_id,
            "status/result edits must refresh the same interaction identity"
        );
    }

    #[test]
    fn a_contacts_header_is_not_a_log() {
        let header = ["Name", "Email", "City"]
            .iter()
            .map(|h| h.to_string())
            .collect::<Vec<_>>();
        assert!(!is_outreach_log(&header));
    }

    #[test]
    fn a_master_row_parses_send_and_reply() {
        let header = master_header();
        let grid = vec![
            header.clone(),
            row(
                &header,
                &[
                    ("outreach_id", "ORC-000002"),
                    ("entity_ref", "ENT-000436"),
                    ("contact", "radoslaw.szatkowski@radiobemowo.fm"),
                    ("purpose", "REVIEW_REQUEST"),
                    ("channel", "EMAIL"),
                    ("subject", "Virya do Dobrze Rockują"),
                    ("sent_at", "2026-07-22 18:13"),
                    ("gmail_thread_id", "19f8a9a5197a896c"),
                    ("status", "REPLIED"),
                    ("reply_at", "2026-07-22 19:06"),
                    ("reply_type", "GMAIL_REPLY"),
                    ("result", "GMAIL_REPLY"),
                ],
            ),
        ];
        let report = extract_outreach_log(&grid).expect("the sheet is claimed");
        assert_eq!(report.entries.len(), 1);
        let entry = &report.entries[0];
        assert!(entry.was_sent());
        assert!(entry.was_replied());
        assert_eq!(
            entry.reply_disposition(),
            OutreachReplyDisposition::Received
        );
        assert!(entry.needs_review(), "GMAIL_REPLY settles nothing");
        assert_eq!(
            entry.replied_at().map(|t| t.to_string()),
            Some("2026-07-22 19:06:00.0 +00:00:00".to_owned())
        );
        assert_eq!(entry.gmail_thread_id.as_deref(), Some("19f8a9a5197a896c"));
    }

    #[test]
    fn a_positive_promo_verdict_maps_to_positive() {
        let header = promo_header();
        let grid = vec![
            header.clone(),
            row(
                &header,
                &[
                    ("outreach_id", "OUT-20260915-GORZ-04"),
                    ("entity_ref", "PL-0076"),
                    ("name", "Głośniej.pl"),
                    ("contact", "redakcja@glosniej.pl"),
                    ("sent_at", "2026-09-15"),
                    ("status", "REPLIED"),
                    ("reply_at", "2026-09-16"),
                    ("response_type", "POSITIVE"),
                    ("result", "POSITIVE"),
                ],
            ),
        ];
        let report = extract_outreach_log(&grid).expect("claimed");
        let entry = &report.entries[0];
        assert_eq!(
            entry.reply_disposition(),
            OutreachReplyDisposition::Positive
        );
        assert!(!entry.needs_review());
    }

    #[test]
    fn a_draft_row_is_not_history() {
        let header = promo_header();
        let grid = vec![
            header.clone(),
            row(
                &header,
                &[
                    ("outreach_id", "OUT-20260905-01"),
                    ("entity_ref", "PL-0024"),
                    ("name", "Głośniej.pl"),
                    ("contact", "redakcja@glosniej.pl"),
                    ("status", "DRAFT"),
                ],
            ),
        ];
        let report = extract_outreach_log(&grid).expect("claimed");
        let entry = &report.entries[0];
        assert!(!entry.was_sent(), "a draft was never sent");
        assert!(!entry.was_replied());
    }

    #[test]
    fn a_declined_verdict_stores_declined() {
        let header = master_header();
        let grid = vec![
            header.clone(),
            row(
                &header,
                &[
                    ("outreach_id", "ORC-000099"),
                    ("contact", "venue@example.pl"),
                    ("sent_at", "2026-08-01 10:00"),
                    ("status", "REPLIED"),
                    ("reply_at", "2026-08-02 09:00"),
                    ("result", "Odrzucone"),
                ],
            ),
        ];
        let report = extract_outreach_log(&grid).expect("claimed");
        assert_eq!(
            report.entries[0].reply_disposition(),
            OutreachReplyDisposition::Declined
        );
        assert!(!report.entries[0].needs_review());
    }

    #[test]
    fn a_row_without_id_is_refused_not_written() {
        let header = master_header();
        let grid = vec![
            header.clone(),
            row(
                &header,
                &[
                    ("contact", "someone@example.pl"),
                    ("sent_at", "2026-08-01 10:00"),
                    ("status", "SENT"),
                ],
            ),
        ];
        let report = extract_outreach_log(&grid).expect("claimed");
        assert!(report.entries.is_empty());
        assert_eq!(report.refusals.len(), 1);
        assert_eq!(report.refusals[0].1, OutreachLogRefusal::MissingId);
    }
}
