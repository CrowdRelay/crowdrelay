//! The shared night — §12-9's rendezvous.
//!
//! One evening at one room is a `place_events` row every tenant's event can
//! point at. Four readers stand over the same night — the band that owns an
//! event on it, another band on its bill, the organiser holding a link, and
//! a roster with two or more acts playing — and the lens each one gets is
//! derived from who is asking, never from a parameter they chose.
//!
//! The boundary this module exists to hold: nothing crosses a workspace
//! boundary that the other side did not put on the bill, and the organiser
//! sees the sum, never the parts. What crosses is what a workspace
//! *contributes* — explicit, capped, revocable, audited. `validate_contribution`
//! is the gate on the write side: a contribution that cannot say what it is
//! never reaches the shared view.

use serde::Serialize;
use serde_json::Value;

/// Who is reading the night — resolved server-side from the caller's
/// relationship to it, so a caller with no relationship learns nothing,
/// including that the night exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NightLens {
    /// The caller owns an `events` row on the night — the band's own page.
    OwnBand,
    /// Another workspace's bill names one of the caller's acts.
    CoBilled,
    /// A link-holder — the promoter or the room. The link is the admission.
    Organiser,
    /// The caller owns two or more billed acts — a label or roster reading
    /// the night across its own acts.
    Roster,
}

/// The lens the caller's relationship derives, if any.
///
/// `billed_slots` is the count of the night's `event_acts` rows whose
/// `act_workspace_id` is the caller — how many of *its* acts are on the bill
/// (confirmed or not: the claim alone earns the least lens). Two or more is
/// the roster rule — a workspace that size sees the split between its acts'
/// contribution and everyone else's, because nobody else may.
///
/// `Organiser` is never returned here: it is forced by a link token, not
/// derived from a workspace.
#[must_use]
pub fn derive_lens(owns_event: bool, billed_slots: u64) -> Option<NightLens> {
    if billed_slots >= 2 {
        Some(NightLens::Roster)
    } else if owns_event {
        Some(NightLens::OwnBand)
    } else if billed_slots == 1 {
        Some(NightLens::CoBilled)
    } else {
        None
    }
}

/// What a workspace may publish into the shared view — the four kinds the
/// schema's CHECK names, one row per kind per workspace per night.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContributionKind {
    /// `{"reachable_fans": int, "expected_draw": int|null}` — how big an
    /// audience this act could actually tell about the night.
    DrawEstimate,
    /// `{"state": "planned"|"announced"|"done"}` — has the act's
    /// announcement gone out. Contributed = public.
    AnnounceStatus,
    /// `{"items": [text, ..]}` — the co-promotion asks this act is making,
    /// public-by-choice to the other acts on the bill.
    Asks,
    /// `{"amount_minor": int, "currency": "PLN"}` — the money line between
    /// this act and the payer. The organiser reads the sum; the parts never
    /// leave the workspace that wrote them.
    Terms,
}

impl ContributionKind {
    /// The schema's spelling of the kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DrawEstimate => "draw_estimate",
            Self::AnnounceStatus => "announce_status",
            Self::Asks => "asks",
            Self::Terms => "terms",
        }
    }

    /// The kind a request names, or nothing — an unknown kind is a 400, not
    /// a guess.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "draw_estimate" => Some(Self::DrawEstimate),
            "announce_status" => Some(Self::AnnounceStatus),
            "asks" => Some(Self::Asks),
            "terms" => Some(Self::Terms),
            _ => None,
        }
    }
}

/// Why a contribution may not enter the shared view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContributionRefusal {
    /// The value is not a JSON object at all — it cannot even name fields.
    NotAnObject,
    /// A field the kind does not define — a reader elsewhere decodes these
    /// strictly, so drift starts at the write.
    UnexpectedField,
    /// A required field is absent, or present with the wrong type.
    Field(&'static str),
    /// A number the schema names but the meaning rejects — a draw below
    /// zero, a guarantee of nothing.
    Value(&'static str),
    /// A currency that is not an ISO-4217 uppercase triple.
    Currency,
    /// More asks than the shared view carries, or an ask that is not a
    /// non-empty bounded string.
    Asks,
}

impl ContributionRefusal {
    /// The refusal as a band reads it — a sentence, not a field path.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NotAnObject => "a contribution is a JSON object naming its fields".to_owned(),
            Self::UnexpectedField => {
                "the contribution carries a field its kind does not define".to_owned()
            }
            Self::Field(field) => {
                format!("the contribution is missing `{field}` or gives it the wrong type")
            }
            Self::Value(field) => {
                format!("`{field}` must be a count that is not negative")
            }
            Self::Currency => {
                "terms need an ISO-4217 currency — three uppercase letters, like PLN".to_owned()
            }
            Self::Asks => {
                "asks are up to 8 items, each a non-empty line of at most 500 characters".to_owned()
            }
        }
    }
}

/// The field set each kind permits — anything outside it is
/// [`ContributionRefusal::UnexpectedField`].
fn allowed_fields(kind: ContributionKind) -> &'static [&'static str] {
    match kind {
        ContributionKind::DrawEstimate => &["reachable_fans", "expected_draw"],
        ContributionKind::AnnounceStatus => &["state"],
        ContributionKind::Asks => &["items"],
        ContributionKind::Terms => &["amount_minor", "currency"],
    }
}

/// A JSON integer field: present, integral, and at least `minimum`.
fn int_field(
    object: &serde_json::Map<String, Value>,
    field: &'static str,
    minimum: i64,
) -> Result<i64, ContributionRefusal> {
    let Some(value) = object.get(field) else {
        return Err(ContributionRefusal::Field(field));
    };
    let Some(number) = value.as_i64() else {
        return Err(ContributionRefusal::Field(field));
    };
    if number < minimum {
        return Err(ContributionRefusal::Value(field));
    }
    Ok(number)
}

/// Checks a contribution's value against its kind's shape.
///
/// # Errors
///
/// Refuses anything that is not the kind's exact shape: terms a non-positive
/// amount or a malformed currency, draw figures below zero, an announce state
/// outside `planned`/`announced`/`done`, more than eight asks or one empty.
/// Unknown fields are refused rather than shrugged off — the readers decode
/// the same shapes, so what cannot be named must not be stored.
pub fn validate_contribution(
    kind: ContributionKind,
    value: &Value,
) -> Result<(), ContributionRefusal> {
    let Some(object) = value.as_object() else {
        return Err(ContributionRefusal::NotAnObject);
    };
    let allowed = allowed_fields(kind);
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(ContributionRefusal::UnexpectedField);
    }
    match kind {
        ContributionKind::Terms => {
            int_field(object, "amount_minor", 1)?;
            match object.get("currency").and_then(Value::as_str) {
                Some(currency)
                    if currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_uppercase()) =>
                {
                    Ok(())
                }
                Some(_) => Err(ContributionRefusal::Currency),
                None => Err(ContributionRefusal::Field("currency")),
            }
        }
        ContributionKind::DrawEstimate => {
            int_field(object, "reachable_fans", 0)?;
            match object.get("expected_draw") {
                // The act may not know the door yet — null is a stated
                // absence, not a refusal.
                None | Some(Value::Null) => Ok(()),
                Some(value) => value
                    .as_i64()
                    .filter(|number| *number >= 0)
                    .map(|_| ())
                    .ok_or(ContributionRefusal::Value("expected_draw")),
            }
        }
        ContributionKind::AnnounceStatus => match object.get("state").and_then(Value::as_str) {
            Some("planned" | "announced" | "done") => Ok(()),
            Some(_) => Err(ContributionRefusal::Value("state")),
            None => Err(ContributionRefusal::Field("state")),
        },
        ContributionKind::Asks => match object.get("items").and_then(Value::as_array) {
            Some(items)
                if !items.is_empty()
                    && items.len() <= 8
                    && items.iter().all(|item| {
                        item.as_str().is_some_and(|text| {
                            !text.trim().is_empty() && text.chars().count() <= 500
                        })
                    }) =>
            {
                Ok(())
            }
            Some(_) => Err(ContributionRefusal::Asks),
            None => Err(ContributionRefusal::Field("items")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Lens derivation ─────────────────────────────────────────────

    #[test]
    fn the_event_owner_reads_the_band_lens() {
        assert_eq!(derive_lens(true, 0), Some(NightLens::OwnBand));
        assert_eq!(derive_lens(true, 1), Some(NightLens::OwnBand));
    }

    #[test]
    fn a_billed_act_with_no_event_is_co_billed() {
        assert_eq!(derive_lens(false, 1), Some(NightLens::CoBilled));
    }

    #[test]
    fn two_billed_acts_make_a_roster() {
        assert_eq!(derive_lens(false, 2), Some(NightLens::Roster));
        assert_eq!(derive_lens(true, 3), Some(NightLens::Roster));
    }

    #[test]
    fn no_relationship_means_no_lens() {
        assert_eq!(derive_lens(false, 0), None);
    }

    #[test]
    fn the_organiser_lens_is_never_derived() {
        // derive_lens has no parameter that could name it — the organiser
        // enters by token, and this is the test that keeps it that way.
        for owns in [false, true] {
            for billed in 0..5 {
                assert_ne!(derive_lens(owns, billed), Some(NightLens::Organiser));
            }
        }
    }

    // ── terms ───────────────────────────────────────────────────────

    #[test]
    fn terms_accepts_a_positive_amount_and_iso_currency() {
        let value = json!({"amount_minor": 125_000, "currency": "PLN"});
        assert_eq!(
            validate_contribution(ContributionKind::Terms, &value),
            Ok(())
        );
    }

    #[test]
    fn terms_refuses_zero_and_negative_amounts() {
        for amount in [0, -1, -500_000] {
            let value = json!({"amount_minor": amount, "currency": "PLN"});
            assert_eq!(
                validate_contribution(ContributionKind::Terms, &value),
                Err(ContributionRefusal::Value("amount_minor"))
            );
        }
    }

    #[test]
    fn terms_refuses_bad_currencies() {
        for currency in ["pln", "PL", "PLNX", "P1N", "", "eur"] {
            let value = json!({"amount_minor": 100, "currency": currency});
            assert_eq!(
                validate_contribution(ContributionKind::Terms, &value),
                Err(ContributionRefusal::Currency),
                "{currency}"
            );
        }
        let missing = json!({"amount_minor": 100});
        assert_eq!(
            validate_contribution(ContributionKind::Terms, &missing),
            Err(ContributionRefusal::Field("currency"))
        );
        let wrong_type = json!({"amount_minor": 100, "currency": 985});
        assert_eq!(
            validate_contribution(ContributionKind::Terms, &wrong_type),
            Err(ContributionRefusal::Field("currency"))
        );
    }

    #[test]
    fn terms_refuses_a_float_amount() {
        let value = json!({"amount_minor": 12.5, "currency": "PLN"});
        assert_eq!(
            validate_contribution(ContributionKind::Terms, &value),
            Err(ContributionRefusal::Field("amount_minor"))
        );
    }

    // ── draw_estimate ───────────────────────────────────────────────

    #[test]
    fn draw_estimate_accepts_reachable_alone_or_with_expected() {
        assert_eq!(
            validate_contribution(
                ContributionKind::DrawEstimate,
                &json!({"reachable_fans": 640})
            ),
            Ok(())
        );
        assert_eq!(
            validate_contribution(
                ContributionKind::DrawEstimate,
                &json!({"reachable_fans": 640, "expected_draw": 90})
            ),
            Ok(())
        );
        // `expected_draw: null` is a stated unknown, not a refusal.
        assert_eq!(
            validate_contribution(
                ContributionKind::DrawEstimate,
                &json!({"reachable_fans": 640, "expected_draw": null})
            ),
            Ok(())
        );
        assert_eq!(
            validate_contribution(
                ContributionKind::DrawEstimate,
                &json!({"reachable_fans": 0})
            ),
            Ok(())
        );
    }

    #[test]
    fn draw_estimate_refuses_negative_and_garbage() {
        assert_eq!(
            validate_contribution(
                ContributionKind::DrawEstimate,
                &json!({"reachable_fans": -1})
            ),
            Err(ContributionRefusal::Value("reachable_fans"))
        );
        assert_eq!(
            validate_contribution(
                ContributionKind::DrawEstimate,
                &json!({"expected_draw": 90})
            ),
            Err(ContributionRefusal::Field("reachable_fans"))
        );
        assert_eq!(
            validate_contribution(
                ContributionKind::DrawEstimate,
                &json!({"reachable_fans": "lots"})
            ),
            Err(ContributionRefusal::Field("reachable_fans"))
        );
        assert_eq!(
            validate_contribution(
                ContributionKind::DrawEstimate,
                &json!({"reachable_fans": 10, "expected_draw": -5})
            ),
            Err(ContributionRefusal::Value("expected_draw"))
        );
    }

    // ── announce_status ─────────────────────────────────────────────

    #[test]
    fn announce_status_accepts_the_three_states() {
        for state in ["planned", "announced", "done"] {
            assert_eq!(
                validate_contribution(ContributionKind::AnnounceStatus, &json!({"state": state})),
                Ok(()),
                "{state}"
            );
        }
    }

    #[test]
    fn announce_status_refuses_other_states_and_missing() {
        assert_eq!(
            validate_contribution(
                ContributionKind::AnnounceStatus,
                &json!({"state": "teased"})
            ),
            Err(ContributionRefusal::Value("state"))
        );
        assert_eq!(
            validate_contribution(ContributionKind::AnnounceStatus, &json!({})),
            Err(ContributionRefusal::Field("state"))
        );
        assert_eq!(
            validate_contribution(ContributionKind::AnnounceStatus, &json!({"state": 7})),
            Err(ContributionRefusal::Field("state"))
        );
    }

    // ── asks ────────────────────────────────────────────────────────

    #[test]
    fn asks_accepts_up_to_eight_non_empty_lines() {
        let items: Vec<String> = (1..=8).map(|n| format!("share the poster {n}")).collect();
        assert_eq!(
            validate_contribution(ContributionKind::Asks, &json!({"items": items})),
            Ok(())
        );
    }

    #[test]
    fn asks_refuses_nine_items_empty_items_and_long_ones() {
        let nine: Vec<String> = (1..=9).map(|n| format!("ask {n}")).collect();
        assert_eq!(
            validate_contribution(ContributionKind::Asks, &json!({"items": nine})),
            Err(ContributionRefusal::Asks)
        );
        assert_eq!(
            validate_contribution(ContributionKind::Asks, &json!({"items": []})),
            Err(ContributionRefusal::Asks)
        );
        assert_eq!(
            validate_contribution(ContributionKind::Asks, &json!({"items": ["  "]})),
            Err(ContributionRefusal::Asks)
        );
        assert_eq!(
            validate_contribution(ContributionKind::Asks, &json!({"items": ["x".repeat(501)]})),
            Err(ContributionRefusal::Asks)
        );
        assert_eq!(
            validate_contribution(ContributionKind::Asks, &json!({"items": [42]})),
            Err(ContributionRefusal::Asks)
        );
        assert_eq!(
            validate_contribution(ContributionKind::Asks, &json!({})),
            Err(ContributionRefusal::Field("items"))
        );
    }

    // ── the envelope ────────────────────────────────────────────────

    #[test]
    fn non_objects_and_stray_fields_are_refused() {
        assert_eq!(
            validate_contribution(ContributionKind::Terms, &json!("PLN 1250")),
            Err(ContributionRefusal::NotAnObject)
        );
        assert_eq!(
            validate_contribution(ContributionKind::Terms, &json!([])),
            Err(ContributionRefusal::NotAnObject)
        );
        assert_eq!(
            validate_contribution(
                ContributionKind::Terms,
                &json!({"amount_minor": 100, "currency": "PLN", "note": "net of tax"})
            ),
            Err(ContributionRefusal::UnexpectedField)
        );
    }

    #[test]
    fn kinds_round_trip_through_parse() {
        for kind in [
            ContributionKind::DrawEstimate,
            ContributionKind::AnnounceStatus,
            ContributionKind::Asks,
            ContributionKind::Terms,
        ] {
            assert_eq!(ContributionKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ContributionKind::parse("everything"), None);
    }

    #[test]
    fn refusals_read_as_sentences() {
        for refusal in [
            ContributionRefusal::NotAnObject,
            ContributionRefusal::UnexpectedField,
            ContributionRefusal::Field("amount_minor"),
            ContributionRefusal::Value("reachable_fans"),
            ContributionRefusal::Currency,
            ContributionRefusal::Asks,
        ] {
            let message = refusal.message();
            assert!(message.len() > 20, "too terse: {message}");
            assert!(
                !message.contains("Err(") && !message.contains("None"),
                "leaks Rust at the band: {message}"
            );
        }
    }
}
