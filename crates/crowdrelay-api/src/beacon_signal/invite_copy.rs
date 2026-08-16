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
                "Cześć {display_name},\n\nchcemy zaprosić Cię do Latarnika Viryi — naszej małej, zweryfikowanej sieci osób z mediów, sceny i lokalnych społeczności. To nie jest newsletter ani program z obowiązkami. Dostajesz konkretny dostęp do Press Roomu, koncertów w swoim regionie i bezpośredniego kontaktu z zespołem.\n\nKażdy aktywny Latarnik, który ma włączone Premiery, dostaje od nas także każdą nową fizyczną płytę Viryi. Przy każdej takiej premierze wyślemy osobną wiadomość — wystarczy potwierdzić dane odbiorcy i Paczkomat dla tej wysyłki.\n\nJak możesz nam realnie pomóc, jeśli masz przestrzeń:\n• recenzja, artykuł albo wzmianka o premierze,\n• radio, podcast lub wywiad,\n• zdjęcia albo wideo z koncertu,\n• udostępnienie premiery lub koncertu swojej społeczności,\n• połączenie nas z sensownym klubem, promotorem, medium lub organizatorem,\n• zwykły feedback — co działa, a co powinniśmy robić lepiej.\n\nNie musisz robić wszystkiego ani niczego „odrabiać”. Chcemy po prostu dać Ci dobre materiały i ułatwić pomoc wtedy, kiedy ma to sens.\n\nTwój prywatny, jednorazowy link: {invite_url}\n\nPo aktywacji możesz ustawić promień i tematy albo w każdej chwili wyłączyć kanał. Jeśli masz pytania, zadzwoń do Wojtka: 784947481 — chętnie wszystko wyjaśnię.\n\nVirya"
            ),
        }
    } else {
        InviteDeliveryCopy {
            subject: "Virya Signal — Beacon invitation".to_owned(),
            text: format!(
                "Hi {display_name},\n\nwe would like to invite you to Virya Beacon — our small verified network of media, scene and local community contacts. It is not a newsletter and there are no participation quotas. You get a focused Press Room, relevant shows in your region and a direct line to the band.\n\nEvery active Beacon with Releases enabled also receives each new physical Virya record. For every physical release we send one separate confirmation message; you only need to confirm the recipient details and parcel-locker destination for that shipment.\n\nIf and when it makes sense, you can help through a review or article, radio/podcast/interview, live photos or video, sharing a release or show, connecting us with a relevant venue/promoter/media outlet, or simply giving us useful feedback. None of this is an obligation.\n\nYour private one-time link: {invite_url}\n\nAfter activation you can choose your radius and topics or disable the channel at any time. Questions are welcome — call Wojtek on +48 784947481.\n\nVirya"
            ),
        }
    }
}
