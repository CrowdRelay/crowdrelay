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
                "Cześć {display_name},\n\nchcemy zaprosić Cię do Latarnika VIRYA — prywatnego kanału dla mediów, fotografów, radia, twórców, promotorów i ludzi sceny, z którymi chcemy utrzymywać sensowny, lokalny kontakt.\n\nW jednym miejscu dostajesz:\n• koncerty VIRYA istotne dla Twojego regionu,\n• aktualny Press Room: EPK, zdjęcia, bio, audio, wideo i rider,\n• szybkie prośby o dodatkowy materiał, wywiad lub akredytację,\n• wcześniejszy dostęp do wybranych materiałów i pul promocyjnych, gdy je uruchamiamy.\n\nLatarnik nie jest newsletterem, programem ambasadorskim ani wymianą „publikacja za wejściówkę”. Nie ma obowiązku publikowania ani wykonywania zadań. Chcemy po prostu ograniczyć przypadkowe maile i dać Ci poprawne materiały wtedy, kiedy są naprawdę przydatne.\n\nTwój prywatny, jednorazowy link:\n{invite_url}\n\nNa telefonie link może otworzyć Virya Signal. Bez aplikacji działa normalnie w przeglądarce. Po aktywacji możesz ustawić promień, tematy i powiadomienia albo w każdej chwili wyłączyć Latarnika.\n\nJeśli coś jest niejasne, po prostu odpisz na tę wiadomość.\n\nVIRYA"
            ),
        }
    } else {
        InviteDeliveryCopy {
            subject: "Virya Signal — Beacon invitation".to_owned(),
            text: format!(
                "Hi {display_name},\n\nwe would like to invite you to VIRYA Beacon — a private channel for media, photographers, radio, creators, promoters and scene contacts with whom we want to keep a useful local relationship.\n\nIn one place you get:\n• VIRYA shows relevant to your area,\n• the current Press Room: EPK, photos, bio, audio, video and rider,\n• a quick way to request extra assets, an interview or accreditation,\n• early access to selected materials and promotional allocations when we open them.\n\nBeacon is not a newsletter, an ambassador programme or a “coverage for entry” exchange. There is no publishing quota and no task obligation. The point is simply fewer random emails and the right materials when they are actually useful.\n\nYour private one-time link:\n{invite_url}\n\nOn a phone the link can open Virya Signal. Without the app it works normally in the browser. After activation you can choose your radius, topics and notifications or disable Beacon at any time.\n\nIf anything is unclear, just reply to this message.\n\nVIRYA"
            ),
        }
    }
}
