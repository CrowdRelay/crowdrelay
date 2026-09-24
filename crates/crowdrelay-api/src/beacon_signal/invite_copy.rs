use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InviteDeliveryCopy {
    pub(super) subject: String,
    pub(super) text: String,
}

/// Who the invitation is from — the act's own name and its own app.
///
/// Both were the first tenant's, hardcoded, so a press contact invited by any
/// other band received a letter signed "VIRYA" offering them "Virya Signal".
#[derive(Clone, Copy)]
pub(super) struct InviteBrand<'a> {
    /// `crowdrelay_workspace_wordmark`: `VIRYA` for the first tenant.
    pub(super) wordmark: &'a str,
    /// The fan app's name: `Virya Signal` for the first tenant, which is what
    /// its store listing is called; `{wordmark} Signal` for anyone else.
    pub(super) app_name: &'a str,
}

pub(super) fn invite_delivery_copy(
    locale: &str,
    display_name: &str,
    invite_url: &str,
    brand: &InviteBrand<'_>,
) -> InviteDeliveryCopy {
    let InviteBrand { wordmark, app_name } = *brand;
    if locale.starts_with("pl") {
        InviteDeliveryCopy {
            subject: format!("{app_name} — zaproszenie do Latarnika"),
            text: format!(
                "Cześć {display_name},\n\nchcemy zaprosić Cię do Latarnika {wordmark} — prywatnego kanału dla mediów, fotografów, radia, twórców, promotorów i ludzi sceny, z którymi chcemy utrzymywać sensowny, lokalny kontakt.\n\nW jednym miejscu dostajesz:\n• koncerty {wordmark} istotne dla Twojego regionu,\n• aktualny Press Room: EPK, zdjęcia, bio, audio, wideo i rider,\n• szybkie prośby o dodatkowy materiał, wywiad lub akredytację,\n• wcześniejszy dostęp do wybranych materiałów i pul promocyjnych, gdy je uruchamiamy.\n\nLatarnik nie jest newsletterem, programem ambasadorskim ani wymianą „publikacja za wejściówkę”. Nie ma obowiązku publikowania ani wykonywania zadań. Chcemy po prostu ograniczyć przypadkowe maile i dać Ci poprawne materiały wtedy, kiedy są naprawdę przydatne.\n\nTwój prywatny, jednorazowy link:\n{invite_url}\n\nNa telefonie link może otworzyć {app_name}. Bez aplikacji działa normalnie w przeglądarce. Po aktywacji możesz ustawić promień, tematy i powiadomienia albo w każdej chwili wyłączyć Latarnika.\n\nJeśli coś jest niejasne, po prostu odpisz na tę wiadomość.\n\n{wordmark}"
            ),
        }
    } else {
        InviteDeliveryCopy {
            subject: format!("{app_name} — Beacon invitation"),
            text: format!(
                "Hi {display_name},\n\nwe would like to invite you to {wordmark} Beacon — a private channel for media, photographers, radio, creators, promoters and scene contacts with whom we want to keep a useful local relationship.\n\nIn one place you get:\n• {wordmark} shows relevant to your area,\n• the current Press Room: EPK, photos, bio, audio, video and rider,\n• a quick way to request extra assets, an interview or accreditation,\n• early access to selected materials and promotional allocations when we open them.\n\nBeacon is not a newsletter, an ambassador programme or a “coverage for entry” exchange. There is no publishing quota and no task obligation. The point is simply fewer random emails and the right materials when they are actually useful.\n\nYour private one-time link:\n{invite_url}\n\nOn a phone the link can open {app_name}. Without the app it works normally in the browser. After activation you can choose your radius, topics and notifications or disable Beacon at any time.\n\nIf anything is unclear, just reply to this message.\n\n{wordmark}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST_TENANT: InviteBrand<'static> = InviteBrand {
        wordmark: "VIRYA",
        app_name: "Virya Signal",
    };

    /// The first tenant's invitation is unchanged: these are fragments of the
    /// text as it was sent when the names were literals.
    #[test]
    fn the_first_tenants_invitation_is_unchanged() {
        let pl = invite_delivery_copy(
            "pl-PL",
            "Ola",
            "https://virya.music/pl/latarnik?invite=x",
            &FIRST_TENANT,
        );
        assert_eq!(pl.subject, "Virya Signal — zaproszenie do Latarnika");
        assert!(pl.text.contains("do Latarnika VIRYA — prywatnego kanału"));
        assert!(
            pl.text
                .contains("• koncerty VIRYA istotne dla Twojego regionu,")
        );
        assert!(
            pl.text
                .contains("Na telefonie link może otworzyć Virya Signal.")
        );
        assert!(pl.text.ends_with("\n\nVIRYA"));

        let en = invite_delivery_copy(
            "en",
            "Ola",
            "https://virya.music/latarnik?invite=x",
            &FIRST_TENANT,
        );
        assert_eq!(en.subject, "Virya Signal — Beacon invitation");
        assert!(en.text.contains("to VIRYA Beacon — a private channel"));
        assert!(en.text.contains("• VIRYA shows relevant to your area,"));
        assert!(
            en.text
                .contains("On a phone the link can open Virya Signal.")
        );
        assert!(en.text.ends_with("\n\nVIRYA"));
    }

    #[test]
    fn another_act_invites_as_itself() {
        let brand = InviteBrand {
            wordmark: "Mgła",
            app_name: "Mgła Signal",
        };
        for locale in ["pl-PL", "en"] {
            let copy = invite_delivery_copy(
                locale,
                "Ola",
                "https://mgla.example/latarnik?invite=x",
                &brand,
            );
            assert!(
                copy.subject.starts_with("Mgła Signal — "),
                "{}",
                copy.subject
            );
            assert!(copy.text.ends_with("\n\nMgła"), "{}", copy.text);
            for leaked in ["VIRYA", "Virya"] {
                assert!(!copy.subject.contains(leaked), "{}", copy.subject);
                assert!(!copy.text.contains(leaked), "{}", copy.text);
            }
        }
    }
}
