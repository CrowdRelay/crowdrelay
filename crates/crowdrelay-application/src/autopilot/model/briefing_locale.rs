// Briefings in the language the crew reads.
//
// `briefing()` authors every action in English, exhaustively — the compiler
// refuses a new payload variant until somebody writes its briefing, and that
// guarantee is worth keeping exactly as it is. This module is the layer that
// puts the result in front of a person.
//
// # Why this is not a translation of the file next door
//
// Virya is Polish, and before this module the briefings were English prose
// inside a Polish email frame: a crew member read *"Dlaczego to ważne:
// Changing the ticket price affects both revenue and attendance."* A handful
// of labels had drifted into Polish too, so neither language was coherent.
//
// The obvious fix — write the briefings in Polish — would bake one tenant's
// language into a crate every tenant shares. So the briefing stays English at
// source and is localised here, per tenant, from the locale the control plane
// already collects in the regional profile.
//
// # Partial by design
//
// A locale covers what has been written for it and nothing else. Anything
// absent falls back to the English source rather than failing to compile or
// rendering an empty string — the same rule the console applies to north-star
// wording, where an unknown value keeps the server's own label. A briefing
// that is half translated is worse than one that is honestly English, so the
// tables below are organised by action so a whole briefing moves at once.

use crate::autopilot::control::ActionBriefing;

/// The languages a briefing can be read in. `En` is the source language and is
/// always complete; every other locale is an overlay that may be partial.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BriefingLocale {
    #[default]
    En,
    Pl,
}

impl BriefingLocale {
    /// Resolves a BCP-47 locale tag from the tenant's regional profile.
    ///
    /// Matches on the language subtag only: `pl`, `pl-PL` and `pl_PL` are one
    /// language as far as this copy is concerned, and a region we have no
    /// wording for is the source language rather than an error.
    #[must_use]
    pub fn from_tag(tag: &str) -> Self {
        let language = tag
            .split(['-', '_'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match language.as_str() {
            "pl" => Self::Pl,
            _ => Self::En,
        }
    }

    /// The BCP-47 tag downstream executors read — the n8n workflow picks its
    /// greeting and subject wrapper on this value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Pl => "pl",
        }
    }
}

impl ActionBriefing {
    /// Returns this briefing in `locale`, leaving anything untranslated in the
    /// source language.
    #[must_use]
    pub fn localized(mut self, locale: BriefingLocale) -> Self {
        let BriefingLocale::Pl = locale else {
            return self;
        };
        self.summary = translate_pl(&self.summary);
        self.why_it_matters = translate_pl(&self.why_it_matters);
        for step in &mut self.steps {
            step.what_to_do = translate_pl(&step.what_to_do);
            step.why_it_matters = translate_pl(&step.why_it_matters);
        }
        for field in &mut self.content {
            // Values translate only where the label says the value is system
            // vocabulary — a phase, a task kind, a tier. A free-text field
            // (a title, a fan's name) that happens to spell a vocabulary word
            // must never be rewritten.
            if PL_VALUE_LABELS.contains(&field.label.as_str()) {
                field.value = translate_value_pl(&field.value);
            }
            field.label = translate_pl(&field.label);
        }
        self
    }
}

/// Looks a phrase up, falling back to the source string.
///
/// Matching is exact on the whole string. Substring or fuzzy matching over
/// prose produces sentences that are half one language and read worse than
/// either — the failure this module exists to remove.
///
/// Two structured fallbacks extend the table where a whole-string match
/// cannot reach. "Show task: staff assigned" is a fixed head plus an enum
/// word, and "Find 3 local Beacons" is a fixed wrapper around a count — both
/// translate their fixed parts and pass the variable through.
fn translate_pl(source: &str) -> String {
    if let Some((_, pl)) = PL.iter().find(|(en, _)| *en == source) {
        return (*pl).to_owned();
    }
    if let Some((head, tail)) = source.split_once(": ")
        && let Some((_, head_pl)) = PL.iter().find(|(en, _)| *en == head)
    {
        return format!("{head_pl}: {}", translate_value_pl(tail));
    }
    for (en, pl) in PL_FIND_TARGETS {
        if let Some(count) = source
            .strip_prefix("Find ")
            .and_then(|rest| rest.strip_suffix(en))
        {
            return format!("Znajdź {count}{pl}");
        }
    }
    // "Join-ask post on {platform}" — the platform name is the tenant's own
    // channel label and stays untranslated either way.
    if let Some(platform) = source.strip_prefix("Join-ask post on ") {
        return format!("Post „dołącz do nas” na {platform}");
    }
    if let Some(count) = source
        .strip_prefix("Ask a Beacon for ")
        .and_then(|rest| rest.strip_suffix(" invite codes"))
    {
        return format!("Poproś Beacona o {count} kodów zaproszeń");
    }
    source.to_owned()
}

/// Translates a content-field value — the enum vocabulary the briefing
/// renders as words, and nothing else. Free text returns unchanged.
fn translate_value_pl(source: &str) -> String {
    // The enum words arrive capitalized at the start of a summary tail and
    // lowercased inside composite values; the vocabulary is closed enough
    // that an ASCII-case-insensitive match stays unambiguous.
    if let Some((_, pl)) = PL_VALUES
        .iter()
        .find(|(en, _)| en.eq_ignore_ascii_case(source))
    {
        return (*pl).to_owned();
    }
    // Composite values carry the word after a counter ("2: announce ask") or
    // before a qualifier ("track us ask (step 2)").
    if let Some((head, tail)) = source.split_once(": ")
        && let Some((_, pl)) = PL_VALUES
            .iter()
            .find(|(en, _)| en.eq_ignore_ascii_case(tail))
    {
        return format!("{head}: {pl}");
    }
    if let Some((word, qualifier)) = source.split_once(" (")
        && let Some((_, pl)) = PL_VALUES
            .iter()
            .find(|(en, _)| en.eq_ignore_ascii_case(word))
    {
        let qualifier = qualifier.replacen("step ", "krok ", 1);
        return format!("{pl} ({qualifier}");
    }
    if let Some(count) = source.strip_suffix(" units") {
        return format!("{count} szt.");
    }
    source.to_owned()
}

/// The labels whose values are enum vocabulary, not free text. Value
/// translation is gated on this list so a title or a name that happens to
/// spell a vocabulary word — a release literally called "Countdown" — is
/// never rewritten.
const PL_VALUE_LABELS: &[&str] = &[
    "Phase",
    "Task",
    "Milestone",
    "Type",
    "Signal",
    "Debt type",
    "Play type",
    "Step",
    "Tier",
    "Finish",
    "Reaches fans",
];

/// The count-bearing summary wrappers: "Find {n} local Beacons".
const PL_FIND_TARGETS: &[(&str, &str)] = &[
    (" local Beacons", " lokalnych Beaconów"),
    (" outreach candidates", " kandydatów outreach"),
    (" booking targets", " celów bookingowych"),
];

/// The enum words the briefing renders into fields and summary tails. Keys
/// are the CamelCase-split words the briefing produces — the same strings an
/// English crew reads — not the Rust variant names.
const PL_VALUES: &[(&str, &str)] = &[
    // ContentArtifactKind labels
    ("signal push", "push Signal"),
    ("newsletter block", "blok newslettera"),
    ("social feed", "post w social media"),
    ("social story", "story w social media"),
    ("live listing", "listing koncertu"),
    ("press hook", "haczyk prasowy"),
    ("post-show recap", "podsumowanie po koncercie"),
    // ShowTaskKind
    ("announcement published", "ogłoszenie opublikowane"),
    ("ticketing verified", "bilety zweryfikowane"),
    ("staff assigned", "obsada przydzielona"),
    ("offline snapshot ready", "snapshot offline gotowy"),
    ("gate device charged", "urządzenie wejściowe naładowane"),
    ("backup device ready", "urządzenie zapasowe gotowe"),
    ("network tested", "internet przetestowany"),
    ("guestlist checked", "guestlista sprawdzona"),
    ("capture plan", "plan ujęć"),
    ("post show reconciliation", "rozliczenie po koncercie"),
    ("post show report", "raport po koncercie"),
    // ReleaseMilestone
    ("seed calendar", "kalendarz seedów"),
    ("editorial pitch", "pitch editorial"),
    ("start press", "start prasy"),
    ("fan warmup", "rozgrzewka fanów"),
    ("countdown", "odliczanie"),
    ("release day", "dzień premiery"),
    ("sustain", "podtrzymanie"),
    ("wrap", "zamknięcie"),
    // ReleaseTier
    ("single", "singiel"),
    ("track", "utwór"),
    ("filler", "utwór wypełniający"),
    // LiveOpportunityKind
    ("festival", "festiwal"),
    ("showcase", "showcase"),
    ("review contest", "konkurs recenzencki"),
    ("support slot", "slot supportowy"),
    // GrowthSignal
    ("decline", "spadek"),
    ("stall", "stagnacja"),
    ("surge", "skok"),
    ("stale feed", "martwy feed"),
    // GrowthDebtKind
    ("relationship quiet", "cicha relacja"),
    ("event levers skipped", "pominięte dźwignie wydarzenia"),
    (
        "release milestones missed",
        "przegapione kamienie milowe wydania",
    ),
    ("release assets missing", "brakujące materiały wydania"),
    ("stale contact data", "nieaktualne dane kontaktowe"),
    (
        "calendar routing conflict",
        "konflikt trasowania kalendarza",
    ),
    ("ticket sales behind pace", "sprzedaż biletów poniżej tempa"),
    // OutreachPhase / BookingOutreachPhase
    ("initial", "pierwszy kontakt"),
    ("follow up", "follow-up"),
    // BeaconOutreachPhase
    ("collaboration follow up", "follow-up o współpracę"),
    ("local push", "lokalny push"),
    ("post show thanks", "podziękowanie po koncercie"),
    // EventCampaignPhase
    ("announcement", "ogłoszenie"),
    ("interest reminder", "przypomnienie o zainteresowaniu"),
    ("last call", "ostatnie wezwanie"),
    ("day of", "dzień koncertu"),
    ("thank you", "podziękowanie"),
    // ReleasePhase
    ("inactive", "nieaktywna"),
    ("preparing", "przygotowanie"),
    ("release week", "tydzień premiery"),
    ("sustaining", "podtrzymywanie"),
    ("complete", "zakończona"),
    // ShowLifecyclePhase
    ("done", "zrobione"),
    ("parked", "zaparkowane"),
    ("due", "wymagane"),
    ("planning", "planowanie"),
    ("amplify", "wzmacnianie"),
    ("convert", "konwersja"),
    ("ready", "gotowe"),
    ("live", "na żywo"),
    ("afterglow", "poblask"),
    ("review", "przegląd"),
    // AgentTier
    ("basic", "podstawowy"),
    ("premium", "premium"),
    // PlayKind
    ("track us ask", "prośba o obserwowanie"),
    (
        "listing completeness sweep",
        "przegląd kompletności listingów",
    ),
    ("follow ask ladder", "drabinka próśb o obserwację"),
    ("dormant revival", "reaktywacja uśpionych"),
    ("release runway", "rozbieg wydania"),
    // PlayStepKind
    ("announce ask", "prośba przy ogłoszeniu"),
    ("post show ask", "prośba po koncercie"),
    ("listing sweep", "przegląd listingów"),
    ("follow ask first", "pierwsza prośba o obserwację"),
    ("follow ask second", "druga prośba o obserwację"),
    ("follow ask final", "ostatnia prośba o obserwację"),
    ("dormant revival first", "pierwsza reaktywacja"),
    ("dormant revival final", "ostatnia reaktywacja"),
    ("release presave live", "pre-save na żywo"),
    (
        "release audience announce",
        "ogłoszenie wydania publiczności",
    ),
    ("release curator wave", "fala kuratorska"),
    ("release day push", "push w dzień premiery"),
    ("release sustain ask", "prośba podtrzymująca"),
    // Yes/no and scheduling words
    ("yes", "tak"),
    ("no", "nie"),
    ("on approval", "po zatwierdzeniu"),
];

/// English source phrase, then the Polish a band member reads.
///
/// Field labels first — they are a small closed set, they repeat across almost
/// every action, and they are what the panel renders in its content block.
/// Step text second: the steps repeat far more than the summaries do, because
/// most actions end in the same two instructions.
const PL: &[(&str, &str)] = &[
    // ── Field labels ────────────────────────────────────────────────────
    ("Event", "Wydarzenie"),
    ("Opportunity", "Okazja"),
    ("Task", "Zadanie"),
    ("Task title", "Nazwa zadania"),
    ("Ticket type", "Typ biletu"),
    ("Target", "Cel"),
    ("Targets", "Cele"),
    ("Target type", "Typ celu"),
    ("Play type", "Typ działania"),
    ("Recommended action", "Zalecane działanie"),
    ("Outcome", "Wynik"),
    ("Platform", "Platforma"),
    ("Campaign", "Kampania"),
    ("Experiment", "Eksperyment"),
    ("Reason", "Powód"),
    ("Body", "Treść"),
    ("Subject", "Temat"),
    ("Title", "Tytuł"),
    ("Name", "Nazwa"),
    ("Description", "Opis"),
    ("Recipient", "Adresat"),
    ("City", "Miasto"),
    ("Amount", "Kwota"),
    ("Currency", "Waluta"),
    ("Deadline", "Termin"),
    ("Previous price", "Poprzednia cena"),
    ("New price", "Nowa cena"),
    ("Variant", "Wariant"),
    ("Winning variant", "Zwycięski wariant"),
    ("Product", "Produkt"),
    ("Source", "Źródło"),
    ("Artifact", "Artefakt"),
    ("Template", "Szablon"),
    ("Phase", "Etap"),
    ("Tier", "Poziom"),
    ("Type", "Typ"),
    ("Track ID", "Numer utworu"),
    ("Smart link", "Link"),
    ("Fan", "Fan"),
    ("Beacon", "Beacon"),
    ("Signal", "Signal"),
    ("Subreddit", "Subreddit"),
    ("Address", "Adres"),
    ("Affinity", "Dopasowanie"),
    ("Arc", "Łuk"),
    ("Beats", "Odcinki"),
    ("Bundle price", "Cena zestawu"),
    ("Candidates", "Kandydaci"),
    ("Channel", "Kanał"),
    ("Checkpoint", "Punkt kontrolny"),
    ("Codes", "Kody"),
    ("Concept", "Koncepcja"),
    ("Contact email", "Email kontaktowy"),
    ("Debt type", "Rodzaj zaległości"),
    ("Details", "Szczegóły"),
    ("Deviation", "Odchylenie"),
    ("Domain", "Domena"),
    ("Draft", "Szkic"),
    ("Evidence", "Dowody"),
    ("Fee", "Honorarium"),
    ("Finish", "Zakończenie"),
    ("Format", "Format"),
    ("Horizon", "Horyzont"),
    ("Lever", "Dźwignia"),
    ("Link", "Link"),
    ("Metric", "Metryka"),
    ("Milestone", "Kamień milowy"),
    ("New budget", "Nowy budżet"),
    ("New capacity", "Nowy limit"),
    ("Overdue", "Po terminie"),
    ("Overdue items", "Zaległe pozycje"),
    ("Plan", "Plan"),
    ("Play", "Play"),
    ("Playlist ID", "Numer playlisty"),
    ("Previous budget", "Poprzedni budżet"),
    ("Previous capacity", "Poprzedni limit"),
    ("Priority", "Priorytet"),
    ("Product A", "Produkt A"),
    ("Product B", "Produkt B"),
    ("Prompt", "Prompt"),
    ("Quantity", "Ilość"),
    ("ROAS", "ROAS"),
    ("Reaches fans", "Dociera do fanów"),
    ("Release date", "Data wydania"),
    ("Reminder", "Przypomnienie"),
    ("Round", "Runda"),
    ("Segment", "Segment"),
    ("Step", "Krok"),
    ("Why it fits", "Dlaczego pasuje"),
    ("Why now", "Dlaczego teraz"),
    ("Who sees it", "Kto to zobaczy"),
    // ── Step instructions ───────────────────────────────────────────────
    (
        "Click APPROVE to apply it",
        "Kliknij ZATWIERDŹ, aby to wykonać",
    ),
    (
        "Check the new price and the ticket type",
        "Sprawdź nową cenę i typ biletu",
    ),
    (
        "Once approved the price is live immediately",
        "Po zatwierdzeniu cena obowiązuje od razu",
    ),
    (
        "Make sure the change is deliberate",
        "Upewnij się, że zmiana jest zamierzona",
    ),
    (
        "Make sure the venue can hold it",
        "Upewnij się, że miejsce pomieści tyle osób",
    ),
    (
        "Once approved the capacity is live",
        "Po zatwierdzeniu limit obowiązuje od razu",
    ),
    (
        "Make sure the tone fits",
        "Sprawdź, czy ton pasuje do zespołu",
    ),
    (
        "The message is delivered to the fan",
        "Wiadomość trafia do fana",
    ),
    (
        "Confirm the stock is genuinely short",
        "Potwierdź, że zapas naprawdę się kończy",
    ),
    (
        "Once approved the order is placed",
        "Po zatwierdzeniu zamówienie zostaje złożone",
    ),
    // ── Why-it-matters, for the actions that reach the crew most ────────
    (
        "Changing the ticket price affects both revenue and attendance. Someone may already have paid the old price, and this cannot be undone.",
        "Zmiana ceny biletu wpływa na przychód i na frekwencję. Ktoś mógł już zapłacić starą cenę, a tego nie da się cofnąć.",
    ),
    (
        "Capacity decides how many tickets can be sold. Lowering it can invalidate tickets already sold.",
        "Limit decyduje, ile biletów można sprzedać. Obniżenie go może unieważnić bilety już sprzedane.",
    ),
    (
        "This goes to a fan who consented to be contacted. A sent message cannot be unsent.",
        "To trafia do fana, który zgodził się na kontakt. Wysłanej wiadomości nie da się cofnąć.",
    ),
    (
        "A merch order costs money and takes time to arrive. Confirm the stock is genuinely short before ordering.",
        "Zamówienie merchu kosztuje i trwa. Potwierdź, że zapas naprawdę się kończy, zanim zamówisz.",
    ),
    // ── Summaries that carry no variable ──────────────────────────────
    (
        "Prepare the funding package",
        "Przygotuj pakiet finansowania",
    ),
    (
        "Issue a referral code to a fan",
        "Wydaj kod referencyjny fanowi",
    ),
    (
        "Send the funding application",
        "Wyślij wniosek o finansowanie",
    ),
    // ── Interpolated-summary heads (the fixed text before ": {value}") ──
    ("Change ticket price", "Zmiana ceny biletu"),
    ("Change ticket capacity", "Zmiana limitu biletów"),
    ("Send a message to a fan", "Wiadomość do fana"),
    ("Reorder merch", "Domówienie merchu"),
    ("Change merch price", "Zmiana ceny merchu"),
    ("Booking contact", "Kontakt bookingowy"),
    ("Audience campaign", "Kampania do fanów"),
    ("Create a merch bundle", "Utworzenie zestawu merchu"),
    ("Outreach contact", "Kontakt outreach"),
    ("Beacon contact", "Kontakt z Beaconem"),
    ("Boost attendance", "Pchnięcie frekwencji"),
    ("Content artifact", "Artefakt treści"),
    ("Show task", "Zadanie koncertowe"),
    ("Escalate the show task", "Eskalacja zadania koncertowego"),
    ("Change promotion budget", "Zmiana budżetu promocji"),
    ("Release milestone", "Kamień milowy wydania"),
    ("Escalate the editorial pitch", "Eskalacja pitchu editorial"),
    (
        "Send a show application",
        "Wysłanie zgłoszenia koncertowego",
    ),
    ("Counter the terms", "Kontra warunków"),
    ("Accept the terms", "Akceptacja warunków"),
    ("Growth opportunity", "Szansa wzrostu"),
    ("Growth debt", "Zaległość wzrostu"),
    ("Make this", "Do zrobienia"),
    ("Commit to the season", "Zobowiązanie na sezon"),
    ("Play step", "Krok play"),
    ("Approve an outreach target", "Zatwierdzenie celu outreach"),
    ("Run the agent", "Uruchomienie agenta"),
    ("Social post", "Post w social media"),
    ("Push notification", "Powiadomienie push"),
    // ── Step instructions, continued ──────────────────────────────────
    (
        "Check the Beacon and the number of codes",
        "Sprawdź Beacona i liczbę kodów",
    ),
    (
        "Check the Beacon, the phase and the template",
        "Sprawdź Beacona, etap i szablon",
    ),
    (
        "Check the amount and the currency",
        "Sprawdź kwotę i walutę",
    ),
    (
        "Check the application is complete",
        "Sprawdź, czy wniosek jest kompletny",
    ),
    (
        "Check the application type and the result",
        "Sprawdź typ zgłoszenia i wynik",
    ),
    (
        "Check the contact details and the rationale",
        "Sprawdź dane kontaktowe i uzasadnienie",
    ),
    (
        "Check the lever and the template",
        "Sprawdź dźwignię i szablon",
    ),
    (
        "Check the new budget against ROAS",
        "Sprawdź nowy budżet na tle ROAS",
    ),
    ("Check the new capacity", "Sprawdź nowy limit"),
    ("Check the new price", "Sprawdź nową cenę"),
    (
        "Check the products and the bundle price",
        "Sprawdź produkty i cenę zestawu",
    ),
    (
        "Check the quantity and the product variant",
        "Sprawdź ilość i wariant produktu",
    ),
    (
        "Check the step type and the template",
        "Sprawdź typ kroku i szablon",
    ),
    (
        "Check the target name and the contact phase",
        "Sprawdź nazwę celu i fazę kontaktu",
    ),
    (
        "Check the target, the phase and the template",
        "Sprawdź cel, etap i szablon",
    ),
    (
        "Check the template and the campaign phase",
        "Sprawdź szablon i fazę kampanii",
    ),
    (
        "Check the template and the priority",
        "Sprawdź szablon i priorytet",
    ),
    ("Check the title and the deadline", "Sprawdź tytuł i termin"),
    (
        "Check the title, the date and the milestone type",
        "Sprawdź tytuł, datę i typ kamienia milowego",
    ),
    (
        "Check the winning variant and the allocations",
        "Sprawdź zwycięski wariant i podział",
    ),
    (
        "Check tone, facts, and that it meets the platform's rules",
        "Sprawdź ton, fakty i zgodność z zasadami platformy",
    ),
    (
        "Check tone, facts, and that it sounds like the brand",
        "Sprawdź ton, fakty i czy brzmi jak zespół",
    ),
    (
        "Click APPROVE if the draft is good, REJECT if it needs work",
        "Kliknij ZATWIERDŹ, jeśli szkic jest dobry, ODRZUĆ, jeśli wymaga pracy",
    ),
    (
        "Click APPROVE to accept the terms",
        "Kliknij ZATWIERDŹ, aby zaakceptować warunki",
    ),
    (
        "Click APPROVE to assemble the package",
        "Kliknij ZATWIERDŹ, aby złożyć pakiet",
    ),
    (
        "Click APPROVE to carry it out",
        "Kliknij ZATWIERDŹ, aby to wykonać",
    ),
    (
        "Click APPROVE to close it out",
        "Kliknij ZATWIERDŹ, aby to zamknąć",
    ),
    (
        "Click APPROVE to commit to the arc",
        "Kliknij ZATWIERDŹ, aby zatwierdzić łuk",
    ),
    (
        "Click APPROVE to commit to the beat",
        "Kliknij ZATWIERDŹ, aby zobowiązać się do odcinka",
    ),
    (
        "Click APPROVE to create it",
        "Kliknij ZATWIERDŹ, aby go utworzyć",
    ),
    (
        "Click APPROVE to dispatch the agent",
        "Kliknij ZATWIERDŹ, aby wysłać agenta",
    ),
    (
        "Click APPROVE to escalate it",
        "Kliknij ZATWIERDŹ, aby to eskalować",
    ),
    (
        "Click APPROVE to generate it",
        "Kliknij ZATWIERDŹ, aby go wygenerować",
    ),
    (
        "Click APPROVE to issue the code",
        "Kliknij ZATWIERDŹ, aby wydać kod",
    ),
    (
        "Click APPROVE to place the order",
        "Kliknij ZATWIERDŹ, aby złożyć zamówienie",
    ),
    (
        "Click APPROVE to promote the target, REJECT if it does not fit",
        "Kliknij ZATWIERDŹ, aby promować cel, ODRZUĆ, jeśli nie pasuje",
    ),
    (
        "Click APPROVE to publish it",
        "Kliknij ZATWIERDŹ, aby opublikować",
    ),
    (
        "The followers already on the page are the cheapest fans to win. This is the weekly ask — the tenant's own words, rotated, never rewritten.",
        "Obserwujący na stronie to najtańsi fani do zdobycia. To cotygodniowe wezwanie — słowa zespołu, rotowane, nigdy nie przepisywane.",
    ),
    ("Read the post text", "Przeczytaj treść posta"),
    (
        "It is one of the variants the tenant wrote — if it no longer sounds right, the variant list is what to edit",
        "To jeden z wariantów napisanych przez zespół — jeśli już nie brzmi dobrze, edytuj listę wariantów",
    ),
    (
        "Once approved the post goes to the band's own page with its tracked link",
        "Po zatwierdzeniu post trafia na stronę zespołu ze śledzonym linkiem",
    ),
    ("Post", "Treść posta"),
    (
        "Click APPROVE to run the check",
        "Kliknij ZATWIERDŹ, aby uruchomić sprawdzenie",
    ),
    (
        "Click APPROVE to run the search",
        "Kliknij ZATWIERDŹ, aby uruchomić wyszukiwanie",
    ),
    (
        "Click APPROVE to schedule the action",
        "Kliknij ZATWIERDŹ, aby zaplanować działanie",
    ),
    (
        "Click APPROVE to schedule the catch-up",
        "Kliknij ZATWIERDŹ, aby zaplanować nadrabianie",
    ),
    (
        "Click APPROVE to send it",
        "Kliknij ZATWIERDŹ, aby to wysłać",
    ),
    (
        "Click APPROVE to send it to the segment",
        "Kliknij ZATWIERDŹ, aby wysłać do segmentu",
    ),
    (
        "Click APPROVE to send the application",
        "Kliknij ZATWIERDŹ, aby wysłać zgłoszenie",
    ),
    (
        "Click APPROVE to send the counter-offer",
        "Kliknij ZATWIERDŹ, aby wysłać kontrofertę",
    ),
    (
        "Click APPROVE to send the email",
        "Kliknij ZATWIERDŹ, aby wysłać email",
    ),
    (
        "Click APPROVE to send the request",
        "Kliknij ZATWIERDŹ, aby wysłać prośbę",
    ),
    (
        "Click APPROVE to start it",
        "Kliknij ZATWIERDŹ, aby to uruchomić",
    ),
    (
        "Confirm the task is actually done",
        "Potwierdź, że zadanie jest naprawdę zrobione",
    ),
    (
        "Make sure every document is ready",
        "Upewnij się, że każdy dokument jest gotowy",
    ),
    (
        "Make sure everything is ready",
        "Upewnij się, że wszystko jest gotowe",
    ),
    (
        "Make sure the action suits the event",
        "Upewnij się, że działanie pasuje do wydarzenia",
    ),
    (
        "Make sure the amount is acceptable",
        "Upewnij się, że kwota jest akceptowalna",
    ),
    (
        "Make sure the change is justified",
        "Upewnij się, że zmiana jest uzasadniona",
    ),
    (
        "Make sure the content suits this phase",
        "Upewnij się, że treść pasuje do tej fazy",
    ),
    (
        "Make sure the content suits this step",
        "Upewnij się, że treść pasuje do tego kroku",
    ),
    (
        "Make sure the decision follows the data",
        "Upewnij się, że decyzja wynika z danych",
    ),
    (
        "Make sure the margin still works",
        "Upewnij się, że marża nadal się spina",
    ),
    (
        "Make sure the message fits",
        "Upewnij się, że wiadomość pasuje",
    ),
    (
        "Make sure the message is personal to them",
        "Upewnij się, że wiadomość jest osobista",
    ),
    (
        "Make sure the target is real and fits the brand",
        "Upewnij się, że cel jest realny i pasuje do marki",
    ),
    (
        "Make sure the task is unambiguous",
        "Upewnij się, że zadanie jest jednoznaczne",
    ),
    (
        "Make sure the task makes sense",
        "Upewnij się, że zadanie ma sens",
    ),
    (
        "Make sure this is the right opportunity",
        "Upewnij się, że to właściwa okazja",
    ),
    (
        "Make sure this is the right partner",
        "Upewnij się, że to właściwy partner",
    ),
    (
        "Make sure this is the right promoter",
        "Upewnij się, że to właściwy promoter",
    ),
    (
        "Once approved the action is carried out",
        "Po zatwierdzeniu działanie zostaje wykonane",
    ),
    (
        "Once approved the action joins the queue",
        "Po zatwierdzeniu działanie trafia do kolejki",
    ),
    (
        "Once approved the agent runs and gathers intelligence",
        "Po zatwierdzeniu agent działa i zbiera informacje",
    ),
    (
        "Once approved the allocations change",
        "Po zatwierdzeniu podział się zmienia",
    ),
    (
        "Once approved the application is sent",
        "Po zatwierdzeniu zgłoszenie zostaje wysłane",
    ),
    (
        "Once approved the budget changes",
        "Po zatwierdzeniu budżet się zmienia",
    ),
    (
        "Once approved the bundle goes on sale",
        "Po zatwierdzeniu zestaw trafia do sprzedaży",
    ),
    (
        "Once approved the campaign is sent",
        "Po zatwierdzeniu kampania zostaje wysłana",
    ),
    (
        "Once approved the content is published, and cannot be unpublished",
        "Po zatwierdzeniu treść zostaje opublikowana — nie da się tego cofnąć",
    ),
    (
        "Once approved the counter-offer is sent",
        "Po zatwierdzeniu kontroferta zostaje wysłana",
    ),
    (
        "Once approved the email is sent",
        "Po zatwierdzeniu email zostaje wysłany",
    ),
    (
        "Once approved the fan receives their referral code",
        "Po zatwierdzeniu fan otrzymuje swój kod referencyjny",
    ),
    (
        "Once approved the growth loop may use this target in campaigns",
        "Po zatwierdzeniu pętla wzrostu może użyć tego celu w kampaniach",
    ),
    (
        "Once approved the message is sent",
        "Po zatwierdzeniu wiadomość zostaje wysłana",
    ),
    (
        "Once approved the milestone is carried out",
        "Po zatwierdzeniu kamień milowy zostaje zrealizowany",
    ),
    (
        "Once approved the post goes live on the platform",
        "Po zatwierdzeniu post publikuje się na platformie",
    ),
    (
        "Once approved the priority is raised",
        "Po zatwierdzeniu priorytet rośnie",
    ),
    (
        "Once approved the push goes to the chosen fan segment",
        "Po zatwierdzeniu push trafia do wybranego segmentu fanów",
    ),
    (
        "Once approved the reminder is sent",
        "Po zatwierdzeniu przypomnienie zostaje wysłane",
    ),
    (
        "Once approved the request goes to the Beacon",
        "Po zatwierdzeniu prośba trafia do Beacona",
    ),
    (
        "Once approved the step is carried out",
        "Po zatwierdzeniu krok zostaje wykonany",
    ),
    (
        "Once approved the task is marked complete",
        "Po zatwierdzeniu zadanie zostaje odhaczone",
    ),
    (
        "Once approved the terms are binding",
        "Po zatwierdzeniu warunki są wiążące",
    ),
    ("Read the draft below", "Przeczytaj szkic poniżej"),
    (
        "Read the message body and its template",
        "Przeczytaj treść wiadomości i jej szablon",
    ),
    (
        "Read the notification title and body",
        "Przeczytaj tytuł i treść powiadomienia",
    ),
    (
        "Read the post title and body",
        "Przeczytaj tytuł i treść posta",
    ),
    (
        "Read the reason and who it reaches",
        "Przeczytaj powód i do kogo to dotrze",
    ),
    (
        "Read the recommended action",
        "Przeczytaj zalecane działanie",
    ),
    (
        "Read the spine and the anchor it is built around",
        "Przeczytaj szkielet i kotwicę, wokół której jest zbudowany",
    ),
    ("Read the task body", "Przeczytaj treść zadania"),
    (
        "Read why the task needs escalating",
        "Przeczytaj, dlaczego zadanie wymaga eskalacji",
    ),
    (
        "Tick this only if the work was actually done",
        "Odhacz tylko, jeśli praca została naprawdę wykonana",
    ),
    (
        "Understand the problem before acting",
        "Zrozum problem, zanim zaczniesz działać",
    ),
    ("Understand what is overdue", "Zrozum, co jest po terminie"),
    (
        "Understand what is overdue, and why",
        "Zrozum, co jest zaległe i dlaczego",
    ),
    (
        "A sent push cannot be recalled — read it closely",
        "Wysłanego pusha nie da się cofnąć — przeczytaj dokładnie",
    ),
    (
        "Approved stays open until the band reports done, declined, or done differently — or the beat's day passes unreported",
        "Zatwierdzone zostaje otwarte, dopóki zespół nie zgłosi: zrobione, odrzucone albo zrobione inaczej — albo dopóki nie minie dzień odcinka bez zgłoszenia",
    ),
    (
        "Beats in the spine then surface as ordinary suggestions under the same policy",
        "Odcinki w szkielecie pojawiają się potem jako zwykłe sugestie na tych samych zasadach",
    ),
    (
        "The band approves the plan, not each step — this is the one creative decision",
        "Zespół zatwierdza plan, nie każdy krok — to ta jedna kreatywna decyzja",
    ),
    // ── Why-it-matters, continued ─────────────────────────────────────
    (
        "A bundle sells two products under one price. Confirm the affinity between them is strong enough.",
        "Zestaw sprzedaje dwa produkty pod jedną ceną. Potwierdź, że dopasowanie między nimi jest wystarczająco mocne.",
    ),
    (
        "A merch order costs money and takes time to arrive. Confirm the stock is actually needed.",
        "Zamówienie merchu kosztuje i trwa. Potwierdź, że zapas jest naprawdę potrzebny.",
    ),
    (
        "A referral code is growth that scales with the audience. The fan must have consented.",
        "Kod referencyjny to wzrost, który skaluje się z publicznością. Fan musi wyrazić zgodę.",
    ),
    (
        "Accepting a fee commits both the calendar and the money. It cannot be undone.",
        "Akceptacja honorarium wiąże i kalendarz, i pieniądze. Nie da się tego cofnąć.",
    ),
    (
        "Allocation decides which variant fans see. Ending the experiment fixes the winner for good.",
        "Podział decyduje, który wariant widzą fani. Zakończenie eksperymentu na stałe ustala zwycięzcę.",
    ),
    (
        "An external metric moved. That is a signal something is happening and may deserve a response.",
        "Zewnętrzna metryka się ruszyła. To sygnał, że coś się dzieje i może wymagać reakcji.",
    ),
    (
        "Escalating marks the task as needing urgent attention and raises its priority in the queue.",
        "Eskalacja oznacza zadanie jako wymagające pilnej uwagi i podnosi jego priorytet w kolejce.",
    ),
    (
        "Merch price affects both margin and volume. Someone may already have paid the old price.",
        "Cena merchu wpływa na marżę i na wolumen. Ktoś mógł już zapłacić starą cenę.",
    ),
    (
        "Submitting the application is a formal commitment and cannot be withdrawn.",
        "Wysłanie wniosku to formalne zobowiązanie — nie da się go wycofać.",
    ),
    (
        "The agent wrote this draft from intelligence the system gathered. Approving publishes it to the channel it was written for.",
        "Agent napisał ten szkic z informacji, które zebrał system. Zatwierdzenie publikuje go w kanale, dla którego powstał.",
    ),
    (
        "The arc is the shape every beat for the next weeks serves. Approving it once replaces dozens of smaller asks — beats inside it still surface, but under a plan the band already chose.",
        "Łuk to kształt, któremu służy każdy odcinek na najbliższe tygodnie. Jedno zatwierdzenie zastępuje dziesiątki mniejszych próśb — odcinki nadal się pojawiają, ale w planie, który zespół już wybrał.",
    ),
    (
        "The budget decides ad spend. ROAS is the return that spend has produced so far.",
        "Budżet decyduje o wydatkach na reklamy. ROAS to zwrot, który te wydatki przyniosły do tej pory.",
    ),
    (
        "The campaign reaches fans tied to this event. A sent campaign cannot be recalled.",
        "Kampania dociera do fanów powiązanych z tym wydarzeniem. Wysłanej kampanii nie da się cofnąć.",
    ),
    (
        "The content engine ranked this beat against everything else the band could make — it is the suggestion, not one of thirty.",
        "Silnik treści ocenił ten odcinek na tle wszystkiego, co zespół mógłby zrobić — to jest ta sugestia, nie jedna z trzydziestu.",
    ),
    (
        "The deterministic brain dispatches an LLM worker to gather intelligence or draft content. The agent decides nothing — it only supplies material.",
        "Deterministyczny mózg wysyła pracownika LLM po informacje albo szkic treści. Agent nic nie decyduje — tylko dostarcza materiał.",
    ),
    (
        "The push reaches fans who consented to notifications. A sent push cannot be recalled.",
        "Push trafia do fanów, którzy zgodzili się na powiadomienia. Wysłanego pusha nie da się cofnąć.",
    ),
    (
        "The system assembles the required documents",
        "System zbierze wymagane dokumenty",
    ),
    (
        "The system builds an artefact from the named source",
        "System zbuduje artefakt ze wskazanego źródła",
    ),
    (
        "The system finds candidate Beacons for the event",
        "System znajdzie potencjalne Beacony dla wydarzenia",
    ),
    (
        "The system finds candidate booking targets",
        "System znajdzie potencjalne cele bookingowe",
    ),
    (
        "The system finds candidate outreach targets",
        "System znajdzie potencjalne cele outreach",
    ),
    (
        "The system generates a content artefact — an image, a piece of copy — from a source. Internal only.",
        "System generuje artefakt treści — obraz, tekst — ze źródła. Tylko wewnętrzne.",
    ),
    (
        "The system reads the public playlist and checks for the track",
        "System odczyta publiczną playlistę i sprawdzi utwór",
    ),
    (
        "The system searches local Beacons near the event. It reads data and contacts nobody.",
        "System przeszuka lokalne Beacony w okolicy wydarzenia. To odczyt danych — nie kontaktuje nikogo.",
    ),
    (
        "The system searches published sources for submission routes. It reads public data and contacts nobody.",
        "System przeszuka opublikowane źródła tras zgłoszeniowych. To odczyt danych publicznych — nie kontaktuje nikogo.",
    ),
    (
        "The system searches published venue and promoter routes. It reads public data and contacts nobody.",
        "System przeszuka opublikowane trasy venue i promoterów. To odczyt danych publicznych — nie kontaktuje nikogo.",
    ),
    (
        "This applies to a show or festival. Applying commits the calendar.",
        "To zgłoszenie na koncert lub festiwal. Zgłoszenie wiąże kalendarz.",
    ),
    (
        "This approaches a Beacon about an event. You get one chance at contact.",
        "To podejście do Beacona w sprawie wydarzenia. Masz jedną szansę kontaktu.",
    ),
    (
        "This approaches an outside target. You get one chance at contact.",
        "To podejście do zewnętrznego celu. Masz jedną szansę kontaktu.",
    ),
    (
        "This asks a partner Beacon to hand out invite codes in their community. The codes are ours, so every signup stays attributed and consented.",
        "To prośba do partnerskiego Beacona o rozdawanie kodów zaproszeń w jego społeczności. Kody są nasze, więc każdy zapis zostaje przypisany i zgodny.",
    ),
    (
        "This assembles the funding application documents. Internal only; it contacts nobody outside.",
        "To składa dokumenty wniosku o finansowanie. Tylko wewnętrzne — nie kontaktuje nikogo na zewnątrz.",
    ),
    (
        "This checks whether the track is on the playlist. It reads public data and contacts nobody.",
        "To sprawdza, czy utwór jest na playliście. Odczytuje publiczne dane i nie kontaktuje nikogo.",
    ),
    (
        "This counters a promoter's fee. Sending it changes the terms under negotiation.",
        "To kontruje honorarium promotera. Wysłanie zmienia negocjowane warunki.",
    ),
    (
        "This emails a task assignment to a crew member. Reminders keep going until the task is closed.",
        "To wysyła ekipie zadanie mailem. Przypomnienia lecą, dopóki zadanie nie zostanie zamknięte.",
    ),
    (
        "This is a commitment — make sure the terms are good",
        "To zobowiązanie — upewnij się, że warunki są dobre",
    ),
    (
        "This is a milestone in the release plan. Completing it triggers the promotional actions scheduled behind it.",
        "To kamień milowy planu wydania. Jego domknięcie odpala zaplanowane za nim działania promocyjne.",
    ),
    (
        "This is a reminder about an unsent Spotify Editorial pitch. A nudge inside the workspace; it contacts nobody outside.",
        "To przypomnienie o niewysłanym pitcu Spotify Editorial. Sztych wewnątrz workspace'u — nie kontaktuje nikogo na zewnątrz.",
    ),
    (
        "This is an attendance push for a show. It may contact outside parties or message fans directly.",
        "To pchnięcie frekwencji na koncert. Może kontaktować strony zewnętrzne albo pisać do fanów bezpośrednio.",
    ),
    (
        "This is an operational task for a show. Marking it done closes that item on the checklist.",
        "To zadanie operacyjne koncertu. Oznaczenie go jako zrobione zamyka punkt na liście.",
    ),
    (
        "This is one step of a play campaign, for one fan. A sent message cannot be recalled.",
        "To jeden krok kampanii play dla jednego fana. Wysłanej wiadomości nie da się cofnąć.",
    ),
    (
        "This is the first approach to a promoter. You get one chance at contact, so the message has to be right.",
        "To pierwsze podejście do promotera. Masz jedną szansę kontaktu, więc wiadomość musi być trafna.",
    ),
    (
        "This is work that was committed to and never done. The longer it waits, the harder it is to catch up.",
        "To praca, która została zadeklarowana i nigdy niewykonana. Im dłużej czeka, tym trudniej nadrobić.",
    ),
    (
        "This posts to an outside platform such as Reddit. It cannot be unposted, and it lands in someone else's community.",
        "To publikacja na zewnętrznej platformie jak Reddit. Nie da się cofnąć, a trafia w czyjąś społeczność.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::BriefingField;

    #[test]
    fn language_subtag_decides_the_locale() {
        assert_eq!(BriefingLocale::from_tag("pl-PL"), BriefingLocale::Pl);
        assert_eq!(BriefingLocale::from_tag("pl_PL"), BriefingLocale::Pl);
        assert_eq!(BriefingLocale::from_tag("PL"), BriefingLocale::Pl);
        assert_eq!(BriefingLocale::from_tag("en-IE"), BriefingLocale::En);
    }

    /// A locale we have no wording for must read as the source language, not
    /// as an empty briefing.
    #[test]
    fn an_unknown_locale_is_the_source_language() {
        assert_eq!(BriefingLocale::from_tag("de-DE"), BriefingLocale::En);
        assert_eq!(BriefingLocale::from_tag(""), BriefingLocale::En);
    }

    #[test]
    fn an_untranslated_phrase_keeps_its_source_text() {
        let source = "A sentence nobody has written Polish for yet";
        assert_eq!(translate_pl(source), source);
    }

    #[test]
    fn a_translated_phrase_is_replaced_whole() {
        assert_eq!(translate_pl("Event"), "Wydarzenie");
    }

    /// Prose must match end to end. A substring rule would translate "Event"
    /// inside "Event sold out" and leave the rest English, which is the mixed
    /// sentence this module removes.
    #[test]
    fn matching_is_whole_string_not_substring() {
        assert_eq!(translate_pl("Event sold out"), "Event sold out");
    }

    #[test]
    fn the_source_locale_changes_nothing() {
        let briefing = ActionBriefing {
            summary: "Event".into(),
            why_it_matters: "Event".into(),
            steps: vec![],
            content: vec![],
            deadline_note: String::new(),
        };
        let unchanged = briefing.clone().localized(BriefingLocale::En);
        assert_eq!(unchanged.summary, "Event");
        let polish = briefing.localized(BriefingLocale::Pl);
        assert_eq!(polish.summary, "Wydarzenie");
    }

    /// Two English phrases must never map to one Polish string by accident:
    /// the panel shows labels side by side and duplicates read as a bug.
    #[test]
    fn the_table_has_no_duplicate_source_phrases() {
        let mut seen = std::collections::HashSet::new();
        for (en, _) in PL {
            assert!(seen.insert(*en), "duplicate source phrase: {en}");
        }
    }

    /// "Show task: staff assigned" is a fixed head plus an enum word — the
    /// whole-string table cannot see it, the head/tail split can.
    #[test]
    fn an_interpolated_summary_translates_head_and_enum_tail() {
        assert_eq!(
            translate_pl("Show task: Staff assigned"),
            "Zadanie koncertowe: obsada przydzielona"
        );
        assert_eq!(
            translate_pl("Growth debt: Stale feed"),
            "Zaległość wzrostu: martwy feed"
        );
    }

    /// A free-text tail — a title, a template label, a name — must pass
    /// through unchanged even when the head translates.
    #[test]
    fn an_interpolated_summary_keeps_a_free_text_tail() {
        assert_eq!(
            translate_pl("Release milestone: Midnight Run"),
            "Kamień milowy wydania: Midnight Run"
        );
    }

    #[test]
    fn a_count_bearing_summary_wraps_the_number() {
        assert_eq!(
            translate_pl("Find 3 local Beacons"),
            "Znajdź 3 lokalnych Beaconów"
        );
        assert_eq!(
            translate_pl("Ask a Beacon for 5 invite codes"),
            "Poproś Beacona o 5 kodów zaproszeń"
        );
    }

    /// Field values translate only behind a vocabulary label — a release
    /// literally titled "Countdown" must not become "odliczanie".
    #[test]
    fn a_free_text_value_is_never_rewritten() {
        let briefing = ActionBriefing {
            summary: String::new(),
            why_it_matters: String::new(),
            steps: vec![],
            content: vec![
                BriefingField {
                    label: "Milestone".into(),
                    value: "Countdown".into(),
                },
                BriefingField {
                    label: "Title".into(),
                    value: "Countdown".into(),
                },
            ],
            deadline_note: String::new(),
        };
        let polish = briefing.localized(BriefingLocale::Pl);
        assert_eq!(polish.content[0].value, "odliczanie");
        assert_eq!(polish.content[1].value, "Countdown");
    }

    /// "2: announce ask" and "track us ask (step 2)" carry the vocabulary
    /// word inside a composite — both shapes must still translate.
    #[test]
    fn composite_values_translate_the_word_inside() {
        assert_eq!(
            translate_value_pl("2: announce ask"),
            "2: prośba przy ogłoszeniu"
        );
        assert_eq!(
            translate_value_pl("Track us ask (step 2)"),
            "prośba o obserwowanie (krok 2)"
        );
        assert_eq!(translate_value_pl("12 units"), "12 szt.");
    }
}
