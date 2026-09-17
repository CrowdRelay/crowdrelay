//! What an operator is allowed to change when they fix a draft.
//!
//! Today an operator can approve a draft or reject it. A draft that is ninety
//! per cent right has two fates: ship as written, or die and wait for the loop
//! to try again. For a system whose promise is that outward text sounds like
//! the tenant, the missing verb is the one a person would reach for first.
//!
//! Adding that verb opens a hole, and this module is the hole's shape. A
//! revision arrives as text an operator typed. If the revision may rewrite any
//! field of the payload, then a box for fixing a sentence is also a box for
//! changing who receives it, what it costs, or which target it names — and the
//! approval that gated the original was granted against different facts. That
//! is privilege escalation through a text box, and it would arrive wearing the
//! costume of a usability improvement.
//!
//! So the rule is narrow and stated once, here, rather than implied across the
//! API, the repository and the executor:
//!
//! **A revision may change the words a human reads. It may not change who reads
//! them, what they cost, or what they point at.**
//!
//! Everything else follows from that sentence. `task_title` and `task_detail`
//! are words. `recipient_email` is not. A draft's `body` is words; the
//! `target_id` it was drafted against is not. A field this module does not know
//! about is refused rather than allowed, because the failure of allowing an
//! unknown field is silent and the failure of refusing one is a bug report.

use std::collections::BTreeMap;

/// A field an operator may rewrite, with the reason it is safe.
///
/// The list is deliberately short and explicit. Deriving it from a type — "any
/// `String` field is editable" — would make `recipient_email` editable, which
/// is exactly the case this exists to prevent.
pub const REVISABLE_FIELDS: &[&str] = &[
    // Team handoffs: what the crew member is asked to do, in words.
    "task_title",
    "task_detail",
    // Outward drafts: the message itself.
    "body",
    "subject",
    "draft_text",
    "summary",
];

/// Why a revision was refused. Each variant is a sentence an operator should be
/// able to read without knowing the codebase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RevisionRefusal {
    /// A field nobody may rewrite, named so the operator knows which.
    FieldNotRevisable { field: String },
    /// Empty after trimming. Approving a draft with nothing in it is a reject
    /// with extra steps, and it would send an empty message.
    FieldEmptied { field: String },
    /// Longer than the original by more than the allowance. A revision is a
    /// correction, not a rewrite into a different message — and the executors
    /// downstream have their own length limits.
    FieldTooLong { field: String, limit: usize },
    /// The revision changes nothing. Not an error in spirit, but recording it
    /// as an edit would poison the signal in §4d-3.2: the edit distance is
    /// meant to measure how wrong the machine was.
    NoChange,
}

impl RevisionRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::FieldNotRevisable { field } => format!(
                "`{field}` cannot be revised: a revision may change the words a human reads, \
                 not who reads them, what they cost, or what they point at"
            ),
            Self::FieldEmptied { field } => {
                format!("`{field}` cannot be emptied — reject the draft instead")
            }
            Self::FieldTooLong { field, limit } => {
                format!("`{field}` is longer than the {limit} characters a revision allows")
            }
            Self::NoChange => "the revision is identical to the draft".to_owned(),
        }
    }

    /// The refusal as a `&'static str` for `RepositoryError::ConflictBecause`.
    ///
    /// The full [`Self::message`] names the field; the repository error cannot
    /// carry an owned string, so the wire reason states the rule the field
    /// broke. The HTTP layer pre-validates field names itself, so an operator
    /// who hits this got past that check — usually a stale modal rather than
    /// a real attempt at a locked field.
    #[must_use]
    pub fn conflict_reason(&self) -> &'static str {
        match self {
            Self::FieldNotRevisable { .. } => {
                "draft revision refused: that field cannot be revised — a revision changes                  the words a human reads, not who reads them, what they cost, or what they point at"
            }
            Self::FieldEmptied { .. } => {
                "draft revision refused: a revision may not empty a field — reject the draft instead"
            }
            Self::FieldTooLong { .. } => {
                "draft revision refused: the revision is longer than the allowed multiple                  of the original"
            }
            Self::NoChange => "draft revision refused: the revision makes no change",
        }
    }
}

/// The hard ceiling on any revised field, and the multiple of the original a
/// revision may reach.
///
/// Both bounds exist because "fix the wording" and "write something four times
/// longer" are different acts, and only the first one was approved. The
/// absolute ceiling covers a draft that started out very short.
pub const REVISION_ABSOLUTE_LIMIT: usize = 4_000;
pub const REVISION_GROWTH_MULTIPLE: usize = 3;
/// Three times a two-word draft is not a workable allowance, so a short draft
/// gets a floor instead of a multiple.
pub const REVISION_SHORT_DRAFT_FLOOR: usize = 280;

fn limit_for(original: &str) -> usize {
    (original.chars().count() * REVISION_GROWTH_MULTIPLE)
        .clamp(REVISION_SHORT_DRAFT_FLOOR, REVISION_ABSOLUTE_LIMIT)
}

/// Checks a proposed revision against the draft it revises.
///
/// `draft` is the field set as the machine wrote it; `revision` is what the
/// operator typed. Returns the fields that actually changed, so the caller
/// stores an edit rather than a copy of the whole draft.
///
/// # Errors
///
/// Refuses on the first problem rather than collecting every one. An operator
/// fixing a sentence wants to know what is wrong, and a list of six refusals
/// for one mistake reads as a wall.
pub fn review_revision(
    draft: &BTreeMap<String, String>,
    revision: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, RevisionRefusal> {
    let mut changed = BTreeMap::new();

    for (field, proposed) in revision {
        if !REVISABLE_FIELDS.contains(&field.as_str()) {
            return Err(RevisionRefusal::FieldNotRevisable {
                field: field.clone(),
            });
        }

        // A field the draft does not carry cannot be revised into existence:
        // the payload's shape is the executor's contract, not the operator's.
        let Some(original) = draft.get(field) else {
            return Err(RevisionRefusal::FieldNotRevisable {
                field: field.clone(),
            });
        };

        let trimmed = proposed.trim();
        if trimmed.is_empty() {
            return Err(RevisionRefusal::FieldEmptied {
                field: field.clone(),
            });
        }

        let limit = limit_for(original);
        if trimmed.chars().count() > limit {
            return Err(RevisionRefusal::FieldTooLong {
                field: field.clone(),
                limit,
            });
        }

        if trimmed != original.trim() {
            changed.insert(field.clone(), trimmed.to_owned());
        }
    }

    if changed.is_empty() {
        return Err(RevisionRefusal::NoChange);
    }

    Ok(changed)
}

/// The fields a payload lets an operator revise, with their current text.
///
/// One definition feeds both ends of the flow: the briefing renders this map
/// as the editable surface, and the approve path feeds it to
/// [`review_revision`] before writing anything. The two must never disagree —
/// a field the operator could see but the gate refused is a trap, and a field
/// the gate accepted but the operator never saw is a hole.
///
/// Sources: top-level string fields on the allowlist, plus the same fields
/// nested under `draft` — an agent-content payload carries its words there.
/// Non-string fields are skipped: a revisable field that is not text is the
/// payload's business, not the operator's.
#[must_use]
pub fn revisable_fields(payload: &serde_json::Value) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut collect = |object: Option<&serde_json::Map<String, serde_json::Value>>| {
        let Some(object) = object else { return };
        for field in REVISABLE_FIELDS {
            if let Some(serde_json::Value::String(text)) = object.get(*field) {
                fields.insert((*field).to_owned(), text.clone());
            }
        }
    };
    collect(payload.as_object());
    collect(payload.get("draft").and_then(serde_json::Value::as_object));
    fields
}

/// Writes an accepted revision back into the payload it was reviewed against.
///
/// A field lands everywhere it appears as a string — top level and inside
/// `draft` — so a payload that carries the same words in two places stays
/// consistent rather than sending one and recording the other.
pub fn apply_revision(payload: &mut serde_json::Value, changed: &BTreeMap<String, String>) {
    let apply_to = |object: &mut serde_json::Map<String, serde_json::Value>| {
        for (field, revised) in changed {
            if matches!(object.get(field), Some(serde_json::Value::String(_))) {
                object.insert(field.clone(), serde_json::Value::String(revised.clone()));
            }
        }
    };
    if let Some(object) = payload.as_object_mut() {
        apply_to(object);
        if let Some(serde_json::Value::Object(draft)) = object.get_mut("draft") {
            apply_to(draft);
        }
    }
}

/// How far the operator moved the machine's words, in changed characters.
///
/// This is the §4d-3.2 signal: the edit is what the machine got wrong, and the
/// distance should fall as the voice rule improves. It is a crude measure on
/// purpose — a real edit distance would imply a precision that a handful of
/// drafts per week cannot support, and §4c's rules about n apply before anyone
/// reads a trend into it.
#[must_use]
pub fn revision_distance(
    draft: &BTreeMap<String, String>,
    changed: &BTreeMap<String, String>,
) -> usize {
    changed
        .iter()
        .map(|(field, revised)| {
            let original = draft.get(field).map_or("", |value| value.as_str()).trim();
            let length_delta = original.chars().count().abs_diff(revised.chars().count());
            // A same-length rewrite — "Film the show" for "Shoot the gig" — is
            // still the machine getting it wrong, and a delta of zero would
            // read as it getting it right. `changed` only holds fields that
            // differ, so the floor of one is never applied to an untouched one.
            length_delta.max(usize::from(original != revised.as_str()))
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("task_title".to_owned(), "Film three shots".to_owned()),
            (
                "task_detail".to_owned(),
                "Wide, crowd, merch table.".to_owned(),
            ),
            (
                "recipient_email".to_owned(),
                "tomek@example.test".to_owned(),
            ),
        ])
    }

    fn revision(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn words_a_human_reads_may_be_rewritten() {
        let changed = review_revision(
            &draft(),
            &revision(&[("task_title", "Film three shots at the merch table")]),
        )
        .expect("a title is words");
        assert_eq!(changed.len(), 1);
        assert_eq!(
            changed.get("task_title").map(String::as_str),
            Some("Film three shots at the merch table")
        );
    }

    /// The whole reason this module exists. A box for fixing a sentence must
    /// not be a box for changing who receives it.
    #[test]
    fn the_recipient_is_not_words() {
        let refusal = review_revision(
            &draft(),
            &revision(&[("recipient_email", "someone-else@example.test")]),
        )
        .expect_err("recipient must be refused");
        assert_eq!(
            refusal,
            RevisionRefusal::FieldNotRevisable {
                field: "recipient_email".to_owned()
            }
        );
    }

    /// Refusing by default matters more than the allow-list being complete: an
    /// unknown field allowed through fails silently, refused fails loudly.
    #[test]
    fn an_unknown_field_is_refused_not_ignored() {
        let refusal = review_revision(&draft(), &revision(&[("fee_minor", "100000")]))
            .expect_err("unknown field must be refused");
        assert!(matches!(refusal, RevisionRefusal::FieldNotRevisable { .. }));
    }

    /// A revisable field the draft does not carry cannot be added: the payload
    /// shape is the executor's contract.
    #[test]
    fn a_field_absent_from_the_draft_cannot_be_added() {
        let refusal = review_revision(&draft(), &revision(&[("subject", "Hello")]))
            .expect_err("absent field must be refused");
        assert!(matches!(refusal, RevisionRefusal::FieldNotRevisable { .. }));
    }

    #[test]
    fn emptying_a_field_is_a_reject_with_extra_steps() {
        let refusal = review_revision(&draft(), &revision(&[("task_title", "   ")]))
            .expect_err("empty must be refused");
        assert_eq!(
            refusal,
            RevisionRefusal::FieldEmptied {
                field: "task_title".to_owned()
            }
        );
    }

    #[test]
    fn a_revision_may_not_become_a_different_message() {
        let long = "x".repeat(5_000);
        let refusal = review_revision(&draft(), &revision(&[("task_detail", &long)]))
            .expect_err("over-long must be refused");
        assert!(matches!(refusal, RevisionRefusal::FieldTooLong { .. }));
    }

    /// A short draft still gets room to be fixed — three times "ok" is not a
    /// workable allowance, so the floor is what matters here.
    #[test]
    fn a_short_draft_still_has_room_to_grow() {
        let short = BTreeMap::from([("task_title".to_owned(), "Post".to_owned())]);
        let changed = review_revision(
            &short,
            &revision(&[("task_title", "Post the video to r/PolishMetal tonight")]),
        )
        .expect("a short draft has a floor, not three characters");
        assert_eq!(changed.len(), 1);
    }

    /// Recording a no-op as an edit would poison the §4d-3.2 signal: the
    /// distance is meant to measure how wrong the machine was.
    #[test]
    fn an_identical_revision_is_not_an_edit() {
        let refusal = review_revision(&draft(), &revision(&[("task_title", "Film three shots")]))
            .expect_err("no-op must be refused");
        assert_eq!(refusal, RevisionRefusal::NoChange);
    }

    #[test]
    fn whitespace_only_changes_are_not_edits() {
        let refusal = review_revision(
            &draft(),
            &revision(&[("task_title", "  Film three shots  ")]),
        )
        .expect_err("trim-equal must be refused");
        assert_eq!(refusal, RevisionRefusal::NoChange);
    }

    #[test]
    fn only_the_changed_fields_come_back() {
        let changed = review_revision(
            &draft(),
            &revision(&[
                ("task_title", "Film three shots"),
                ("task_detail", "Wide, crowd, merch table, and the scan."),
            ]),
        )
        .expect("one field moved");
        assert_eq!(changed.keys().collect::<Vec<_>>(), vec!["task_detail"]);
    }

    #[test]
    fn distance_counts_the_characters_the_operator_moved() {
        let draft = draft();
        let changed = review_revision(
            &draft,
            &revision(&[("task_title", "Film three shots at the merch table")]),
        )
        .expect("revision accepted");
        assert_eq!(revision_distance(&draft, &changed), 19);
    }

    /// A same-length rewrite is still an edit, and a distance of zero would
    /// read as "the machine was right".
    #[test]
    fn a_same_length_rewrite_is_not_zero_distance() {
        let draft = BTreeMap::from([("task_title".to_owned(), "Film the show".to_owned())]);
        let changed = review_revision(&draft, &revision(&[("task_title", "Shoot the gig")]))
            .expect("revision accepted");
        assert_eq!(revision_distance(&draft, &changed), 1);
    }

    /// The map the operator edits and the map the gate reviews are the same
    /// extraction — a field missing from one but present in the other is the
    /// trap this exists to prevent.
    #[test]
    fn revisable_fields_covers_top_level_and_draft() {
        let payload = serde_json::json!({
            "task_title": "Film three shots",
            "recipient_email": "tomek@example.test",
            "fee_minor": 4200,
            "draft": {
                "subject": "Virya at Progresja",
                "body": "hello",
                "platform": "instagram"
            }
        });
        let fields = revisable_fields(&payload);
        assert_eq!(
            fields.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["body", "subject", "task_title"]
        );
        // Recipients and fees exist in the payload but not in the map — a
        // revision against them must be refused, never silently applied.
        assert!(!fields.contains_key("recipient_email"));
        assert!(!fields.contains_key("fee_minor"));
    }

    #[test]
    fn apply_revision_lands_where_the_field_lives() {
        let mut payload = serde_json::json!({
            "task_title": "Film three shots",
            "draft": {"subject": "old", "platform": "instagram"}
        });
        apply_revision(
            &mut payload,
            &BTreeMap::from([
                ("task_title".to_owned(), "Film the encore".to_owned()),
                ("subject".to_owned(), "new subject".to_owned()),
            ]),
        );
        assert_eq!(payload["task_title"], "Film the encore");
        assert_eq!(payload["draft"]["subject"], "new subject");
        // Untouched and non-revisable fields stay put.
        assert_eq!(payload["draft"]["platform"], "instagram");
    }

    #[test]
    fn apply_revision_never_adds_a_field_the_payload_lacks() {
        let mut payload = serde_json::json!({"task_title": "Film three shots"});
        apply_revision(
            &mut payload,
            &BTreeMap::from([("body".to_owned(), "surprise".to_owned())]),
        );
        assert!(payload.get("body").is_none());
    }

    #[test]
    fn every_refusal_reads_as_a_sentence() {
        for refusal in [
            RevisionRefusal::FieldNotRevisable {
                field: "recipient_email".to_owned(),
            },
            RevisionRefusal::FieldEmptied {
                field: "body".to_owned(),
            },
            RevisionRefusal::FieldTooLong {
                field: "body".to_owned(),
                limit: 280,
            },
            RevisionRefusal::NoChange,
        ] {
            let message = refusal.message();
            assert!(message.len() > 20, "refusal too terse: {message}");
            assert!(
                !message.contains("Err(") && !message.contains("None"),
                "refusal leaks Rust at the operator: {message}"
            );
        }
    }
}
