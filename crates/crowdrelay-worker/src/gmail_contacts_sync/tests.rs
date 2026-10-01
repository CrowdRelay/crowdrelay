#[test]
fn a_vanished_message_is_not_a_failure() {
    assert!(super::message_vanished("gmail request failed status=404"));
    assert!(!super::message_vanished("gmail request failed status=500"));
    assert!(!super::message_vanished("gmail request failed status=4040"));
    assert!(!super::message_vanished("gmail response parse failed: eof"));
}

use super::*;

#[test]
fn the_tenants_own_outbound_mail_records_nothing() {
    assert_eq!(
        inbound_sighting(
            "Band <band@virya.music>",
            "band@virya.music",
            Some("1727740800000")
        ),
        None
    );
    assert_eq!(
        inbound_sighting("Promoter <promo@venue.pl>", "band@virya.music", None),
        None,
        "no internalDate, no sighting"
    );
    assert_eq!(
        inbound_sighting(
            "Promoter <promo@venue.pl>",
            "band@virya.music",
            Some("not-a-timestamp")
        ),
        None,
        "an unparseable internalDate records nothing"
    );
}
