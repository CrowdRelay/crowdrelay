//! The mail a beacon gets when a physical release opens for them.
//!
//! It used to be one text for every tenant: the first tenant's release, its
//! site, its sign-off and a crew member's personal phone number. Another
//! band's beacons would have been told about "a new physical Virya release"
//! and asked to ring a stranger. The first tenant keeps its letter to the
//! byte; every other act gets the same letter in its own name, its own link,
//! and a reply-to-this-mail line where the phone number was.

use time::OffsetDateTime;
use uuid::Uuid;

pub(in crate::beacon_signal) struct ReleaseDeliveryCopy {
    pub subject: String,
    pub text: String,
}

/// Who signs a release mail.
pub(in crate::beacon_signal) struct ReleaseSigner<'a> {
    /// "Virya" for the first tenant — every release mail it has sent was
    /// signed so — unless its operator set a wordmark; otherwise
    /// `crowdrelay_workspace_wordmark`.
    pub signature: &'a str,
    /// The first tenant's letter carries its own crew contact; nobody else's
    /// may.
    pub first_tenant: bool,
}

/// `(signature, first_tenant)` for one workspace's beacon release mails.
/// Shared by the launch mail here and the worker's activation follow-up, so
/// the two letters of one release cannot be signed by different names.
pub async fn beacon_release_signature<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    workspace_id: Uuid,
) -> Result<(String, bool), sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT CASE
                 WHEN workspace.slug = 'virya' AND NOT EXISTS (
                     SELECT 1 FROM tenant_settings AS setting
                     WHERE setting.workspace_id = workspace.id
                       AND setting.key = 'brand_wordmark'
                       AND btrim(setting.value) <> ''
                 )
                 THEN 'Virya'
                 ELSE crowdrelay_workspace_wordmark(workspace.id)
               END,
               workspace.slug = 'virya'
        FROM workspaces AS workspace
        WHERE workspace.id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_one(executor)
    .await
}

pub(in crate::beacon_signal) fn release_delivery_copy(
    locale: &str,
    display_name: &str,
    title: &str,
    deadline: OffsetDateTime,
    member_url: &str,
    signer: &ReleaseSigner<'_>,
) -> ReleaseDeliveryCopy {
    let deadline_str = format!(
        "{:02}-{:02}-{:02}",
        deadline.year(),
        deadline.month() as u8,
        deadline.day()
    );
    let signature = signer.signature;
    let polish = locale.starts_with("pl");
    let subject = if polish {
        format!("Dziękujemy Latarniku, {display_name}! Nowe wydanie: {title}")
    } else {
        format!("Thank you, Beacon, {display_name}! New release: {title}")
    };
    let text = match (polish, signer.first_tenant) {
        (true, true) => format!(
            "Dziękujemy Latarniku, {display_name}!\n\nMamy nowe fizyczne wydanie Viryi: {title}. Twój egzemplarz jest zarezerwowany w puli Latarników. Żebyśmy faktycznie mogli go wysłać, wejdź do swojego panelu i potwierdź dla tej premiery imię i nazwisko odbiorcy, telefon oraz Paczkomat przed {deadline_str}.\n\n{member_url}\n\nJeśli chcesz pomóc przy tej premierze, w Press Roomie masz gotowe materiały. Najbardziej pomagają nam: recenzja lub artykuł, radio/podcast/wywiad, zdjęcia albo wideo, udostępnienie premiery oraz kontakt do sensownego medium, promotora lub klubu. Nic z tego nie jest obowiązkiem — płyta jest naszym podziękowaniem za bycie częścią Latarnika.\n\nMasz pytanie? Wojtek: 784947481.\n\n{signature}",
        ),
        (false, true) => format!(
            "Thank you, Beacon, {display_name}!\n\nWe have a new physical Virya release: {title}. Your copy is reserved in the Beacon pool. To receive it, open your Beacon panel and confirm the recipient name, phone number and parcel-locker destination for this release before {deadline_str}.\n\n{member_url}\n\nThe Press Room contains ready-to-use material if you want to help with the release. Reviews/articles, radio/podcasts/interviews, live photos/video, sharing the release, and relevant media/promoter/venue introductions are especially useful. None of this is an obligation — the record is our thank-you for being part of Beacon.\n\nQuestions? Wojtek: +48 784947481.\n\n{signature}",
        ),
        (true, false) => format!(
            "Dziękujemy Latarniku, {display_name}!\n\nMamy nowe fizyczne wydanie {signature}: {title}. Twój egzemplarz jest zarezerwowany w puli Latarników. Żebyśmy faktycznie mogli go wysłać, wejdź do swojego panelu i potwierdź dla tej premiery imię i nazwisko odbiorcy, telefon oraz Paczkomat przed {deadline_str}.\n\n{member_url}\n\nJeśli chcesz pomóc przy tej premierze, w Press Roomie masz gotowe materiały. Najbardziej pomagają nam: recenzja lub artykuł, radio/podcast/wywiad, zdjęcia albo wideo, udostępnienie premiery oraz kontakt do sensownego medium, promotora lub klubu. Nic z tego nie jest obowiązkiem — płyta jest naszym podziękowaniem za bycie częścią Latarnika.\n\nMasz pytanie? Po prostu odpisz na tę wiadomość.\n\n{signature}",
        ),
        (false, false) => format!(
            "Thank you, Beacon, {display_name}!\n\nWe have a new physical {signature} release: {title}. Your copy is reserved in the Beacon pool. To receive it, open your Beacon panel and confirm the recipient name, phone number and parcel-locker destination for this release before {deadline_str}.\n\n{member_url}\n\nThe Press Room contains ready-to-use material if you want to help with the release. Reviews/articles, radio/podcasts/interviews, live photos/video, sharing the release, and relevant media/promoter/venue introductions are especially useful. None of this is an obligation — the record is our thank-you for being part of Beacon.\n\nQuestions? Just reply to this message.\n\n{signature}",
        ),
    };
    ReleaseDeliveryCopy { subject, text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const FIRST_TENANT_URL: &str = "https://virya.music/pl/latarnik/#wydania";

    fn first_tenant() -> ReleaseSigner<'static> {
        ReleaseSigner {
            signature: "Virya",
            first_tenant: true,
        }
    }

    /// The first tenant's letter, pinned as it was sent before this module
    /// existed.
    #[test]
    fn the_first_tenants_letter_is_unchanged() {
        let deadline = datetime!(2026-10-05 12:00 UTC);
        let pl = release_delivery_copy(
            "pl-PL",
            "Radio Test",
            "Echoes",
            deadline,
            FIRST_TENANT_URL,
            &first_tenant(),
        );
        assert_eq!(
            pl.subject,
            "Dziękujemy Latarniku, Radio Test! Nowe wydanie: Echoes"
        );
        assert!(pl.text.starts_with(
            "Dziękujemy Latarniku, Radio Test!\n\nMamy nowe fizyczne wydanie Viryi: Echoes."
        ));
        assert!(
            pl.text
                .contains("przed 2026-10-05.\n\nhttps://virya.music/pl/latarnik/#wydania\n\n")
        );
        assert!(
            pl.text
                .ends_with("\n\nMasz pytanie? Wojtek: 784947481.\n\nVirya")
        );

        let en = release_delivery_copy(
            "en",
            "Radio Test",
            "Echoes",
            deadline,
            FIRST_TENANT_URL,
            &first_tenant(),
        );
        assert_eq!(
            en.subject,
            "Thank you, Beacon, Radio Test! New release: Echoes"
        );
        assert!(
            en.text
                .contains("We have a new physical Virya release: Echoes.")
        );
        assert!(
            en.text
                .ends_with("\n\nQuestions? Wojtek: +48 784947481.\n\nVirya")
        );
    }

    /// Another act's beacons hear from that act, land on its site, and are
    /// never handed the first tenant's name, site or crew phone.
    #[test]
    fn another_acts_letter_is_its_own() {
        let signer = ReleaseSigner {
            signature: "MGŁA",
            first_tenant: false,
        };
        for locale in ["pl-PL", "en"] {
            let copy = release_delivery_copy(
                locale,
                "Radio Test",
                "Echoes",
                datetime!(2026-10-05 12:00 UTC),
                "https://mgla.example/pl/latarnik/#wydania",
                &signer,
            );
            assert!(copy.text.contains("MGŁA"), "{}", copy.text);
            assert!(
                copy.text
                    .contains("https://mgla.example/pl/latarnik/#wydania")
            );
            assert!(copy.text.ends_with("\n\nMGŁA"), "{}", copy.text);
            for leak in ["Virya", "Viryi", "virya.music", "Wojtek", "784947481"] {
                assert!(!copy.text.contains(leak), "{leak} in: {}", copy.text);
                assert!(!copy.subject.contains(leak), "{leak} in: {}", copy.subject);
            }
        }
    }
}
