#[test]
fn a_channel_id_is_a_snowflake_and_an_invite_code_is_not() {
    // Production holds an invite code, not a channel snowflake. An invite
    // must never be used as the publishing destination.
    for channel in [
        "1234567890123456789",
        "12345678901234567",
        "12345678901234567890",
        "  1234567890123456789  ",
    ] {
        assert!(is_discord_snowflake(channel));
    }
    for invalid in [
        "BBdDV6gVy",
        "general",
        "",
        "1234567890123456",
        "123456789012345678901",
        "12345678901234567x",
        "#1234567890123456789",
    ] {
        assert!(!is_discord_snowflake(invalid), "invalid channel: {invalid}");
    }
}
