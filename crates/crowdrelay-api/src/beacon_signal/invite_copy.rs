use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InviteDeliveryCopy {
    pub(super) subject: String,
    pub(super) text: String,
}

pub(super) fn invite_delivery_copy(
    locale: &str,
    display_name: &str,
    invite_url: &str,
) -> InviteDeliveryCopy {
    if locale.starts_with("pl") {
        InviteDeliveryCopy {
            subject: "Virya Signal — zaproszenie do Latarnika".to_owned(),
            text: format!(
                "Cześć {display_name},\n\nchcemy dać Ci prostszy dostęp do rzeczy, które mogą być przydatne przy Viryi — bez kolejnego newslettera i bez zasypywania mailami. Latarnik pokazuje tylko istotne koncerty w Twoim regionie, gotowy Press Room oraz bezpośrednią ścieżkę do zdjęć, WAV-ów, akredytacji i kontaktu z zespołem.\n\nTwój prywatny, jednorazowy link: {invite_url}\n\nPo aktywacji możesz ustawić promień i rodzaje informacji albo w każdej chwili wyłączyć kanał.\n\nVirya"
            ),
        }
    } else {
        InviteDeliveryCopy {
            subject: "Virya Signal — Beacon invitation".to_owned(),
            text: format!(
                "Hi {display_name},\n\nwe would like to give you a lower-friction way to access useful Virya material — without another newsletter or irrelevant email stream. Beacon shows only relevant Virya shows in your region, a ready-to-use Press Room, and a direct path to photos, WAVs, accreditation and the band.\n\nYour private one-time link: {invite_url}\n\nAfter activation you can set the radius and topics, or disable the channel at any time.\n\nVirya"
            ),
        }
    }
}
