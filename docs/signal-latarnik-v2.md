# Signal — Latarnik v2 (full lifecycle)

## Cel produktu

Latarnik to profesjonalny, lokalny kanał relacji Virya z istniejącymi **Beaconami**: prasą,
radiem, podcastami, fotografami, twórcami, promotorami, klubami, recenzentami i innymi osobami,
które realnie mogą pomóc przy koncercie lub premierze. Nie jest to poziom fana, staff role ani
„street team”. Obietnica produktu brzmi: **mniej przypadkowych maili, tylko lokalnie istotne
sygnały i natychmiastowy dostęp do poprawnych materiałów prasowych**.

## Granice odpowiedzialności

- `viryaos_beacons` jest jedynym CRM/source of truth dla relacji, weryfikacji, miasta,
  `accepts_outreach`, `do_not_contact`, relevance i relationship score.
- `viryaos_beacon_signal_profiles` przechowuje wyłącznie stan kanału Signal: aktywny/paused/revoked,
  promień, locale, topics i ustawienia lokalnych powiadomień.
- `viryaos_beacon_signal_sessions` przechowuje revocable session capabilities; endpoint push jest
  związany z hashem konkretnej sesji.
- `viryaos_beacon_signal_event_engagements` jest trwałym stanem `Beacon × Event`.
- `viryaos_beacon_press_assets` jest katalogiem materiałów globalnych i event-specific.
- `viryaos_beacon_press_requests` jest kolejką próśb Latarnika do zespołu.
- `viryaos_beacon_signal_coverage` zapisuje realny rezultat współpracy.
- CrowdRelay wybiera odbiorców, sprawdza consent, dystans, idempotencję i stan lifecycle.
- n8n/Gmail może **wykonać** gotową akcję wysyłki, ale nie wybiera odbiorców i nie jest authority.
- Virya web jest pełnym member surface v2. Virya Signal ma entry point; backend jest gotowy na
  późniejsze przejęcie sesji do native Stronghold bez migracji domeny.

## Pełny przepływ A → Z

### 1. Discovery kandydatów

`GET /v1/admin/autopilot/beacon-signal/candidates`

Zwraca wyłącznie istniejące Beacony, które są aktywne, zweryfikowane, mają kontakt e-mail,
`accepts_outreach=true`, `do_not_contact=false` i nie mają już aktywnego profilu Latarnika.
Kandydaci są porządkowani według istniejących relevance/relationship signals. Nie powstaje drugi
CRM.

Operator CLI:

```bash
python3 scripts/latarnik_operator.py candidates
```

### 2. Zaproszenie pojedyncze albo batch ≤ 200

Single:

`POST /v1/admin/autopilot/beacons/{beacon_id}/signal-invites`

Batch:

`POST /v1/admin/autopilot/beacons/signal-invites/batch`

Batch jest twardo ograniczony do 200 rekordów. Każda osoba dostaje inny losowy token. **Plaintext
capability nie jest zapisywany w PostgreSQL ani outboxie**; zapisany jest wyłącznie SHA-256.
Response v2 zawiera jednorazowy URL oraz lokalizowany `delivery.subject` / `delivery.text`, dzięki
czemu executor może od razu wysłać poprawne zaproszenie bez budowania treści po swojej stronie.
Ponowne zaproszenie revokuje stare żywe sesje.

```bash
python3 scripts/latarnik_operator.py invite-batch --top 20 --output /tmp/latarnik-wave.json
```

Plik z invite capabilities jest tworzony jako `0600`. Należy go traktować jak sekret krótkotrwały,
przekazać do executora i usunąć po wysyłce. Nie kopiować tokenów do logów, tasków ani durable outbox.

### 3. One-time exchange

`POST /v1/beacon/invitations/exchange`

Portal natychmiast usuwa invite token z URL. Exchange jest jednorazowy i tworzy sesję z TTL.
Domyślny session TTL to 180 dni. Auth przy każdym requestcie ponownie sprawdza:

- sesję: nieodwołana i niewygasła,
- profil Signal: `active`,
- Beacon: `active`, `verified`, `accepts_outreach`, `!do_not_contact`.

Zmiana consent w CRM działa więc od razu również na starą sesję.

### 4. Preferencje i radar lokalny

`GET /v1/beacon/me`

`POST /v1/beacon/me/preferences`

Latarnik kontroluje promień 10–500 km, locale, topics oraz local-show notifications. CrowdRelay
liczy geodesic distance na współrzędnych miast; nie deleguje geofencingu do klienta.

### 5. Event-specific Press Room

`GET /v1/beacon/me/press-room?event_id=<uuid>`

Response v2 zawiera:

- fakty wydarzenia: tytuł, data, drzwi, venue, miasto, ticket URL, opis, image/listen/trailer,
- globalne materiały Virya,
- event-specific assets, jeśli istnieją.

Katalog zarządza operator:

`GET /v1/admin/autopilot/beacon-press-assets`

`POST /v1/admin/autopilot/beacon-press-assets`

Obsługiwane klasy: EPK, photo, logo, bio, audio, video, rider, social, contact i link. URL jest
ograniczony do bezpiecznych `https://` albo `mailto:` zależnie od typu.

### 6. Lifecycle `Beacon × Event`

Trwały model:

`eligible → notified → opened → interested → helping → completed`

Alternatywnie `declined`.

`POST /v1/beacon/me/events/{event_id}/engagement`

Akcje użytkownika obejmują otwarcie, zainteresowanie, `Mogę pomóc` oraz `Nie tym razem`.
`help_kind` rozróżnia m.in. artykuł, radio, podcast, zdjęcia, share, kontakt i other.

Stan jest monotoniczny: przypadkowe późniejsze `opened` nie cofa `helping/completed`. Jawna,
uwierzytelniona decyzja Latarnika może odwrócić **event-level** wcześniejsze `declined`, natomiast
globalne `do_not_contact`, suppression i closed pozostają twarde.

CrowdRelay projektuje lifecycle do istniejącej kampanii Autopilota; nie utrzymuje równoległej
„prawdy” o stanie relacji.

### 7. Prośba o brakujący materiał

`POST /v1/beacon/me/press-requests`

`GET /v1/beacon/me/press-requests`

Latarnik może poprosić o press photos, WAV, clean version, interview, accreditation albo custom
material. Request trafia do bounded staff queue i emituje outbox event.

Operator:

```bash
python3 scripts/latarnik_operator.py press-requests
python3 scripts/latarnik_operator.py resolve-request <uuid> --status resolved --note "WAV wysłany"
```

Admin resolution jest audytowalna i nie usuwa historii requestu nawet po usunięciu eventu.

### 8. Coverage / realny rezultat

`POST /v1/beacon/me/events/{event_id}/coverage`

Latarnik może oddać link do artykułu, audycji, video, galerii, posta, podcastu albo innego coverage.
URL musi być HTTPS. Coverage kończy engagement i projektuje kampanię do `partner`, o ile globalny
status nie jest suppression/closed. Dzięki temu relationship history jest zasilana rzeczywistym
wynikiem, a nie tylko „mail opened”.

### 9. Bounded nearby-show push wave

`POST /v1/internal/beacon/notifications/emit-due`

Endpoint jest w internal/commerce namespace. Domyślnie bierze 20 par Beacon×Event, maksimum 100,
z lead time domyślnie 60 dni. Ranking uwzględnia:

1. czas wydarzenia,
2. relevance score,
3. relationship score,
4. dystans.

Kandydat musi przejść aktualne consent/profile/topic/radius checks. Już powiadomione, completed,
declined, suppressed i closed są pomijane. Delivery jest idempotentne per endpoint/source. Target
prowadzi bezpośrednio do `/pl/latarnik?event_id=<uuid>` albo `/latarnik?event_id=<uuid>`.

Co ważne, worker **ponownie** rewaliduje live session + profil + Beacon consent przy claim/delivery.
Jeśli zgoda została cofnięta po zakolejkowaniu, push nie jest dostarczany.

```bash
python3 scripts/latarnik_operator.py emit-wave --limit 20 --lead-days 60
```

### 10. Pause, revoke, logout, leave

Admin:

```bash
python3 scripts/latarnik_operator.py state <beacon_uuid> --status paused
python3 scripts/latarnik_operator.py state <beacon_uuid> --status active
python3 scripts/latarnik_operator.py state <beacon_uuid> --status revoked
```

Member `logout` revokuje bieżącą sesję i jej push endpointy.

Member `leave` revokuje kanał Latarnika. Dopiero jawne `doNotContact=true` ustawia globalne
`accepts_outreach=false` + `do_not_contact=true`; zwykłe wyjście z Latarnika nie niszczy całej
relacji CRM.

## Operator dashboard

`GET /v1/admin/autopilot/beacon-signal`

oraz CLI:

```bash
python3 scripts/latarnik_operator.py dashboard
python3 scripts/latarnik_operator.py engagements
python3 scripts/latarnik_operator.py coverage
```

Dashboard ma rozróżniać profile, żywe sesje/push endpoints, otwarte press requests, aktywne
engagements oraz coverage. Lista kontaktów pozostaje w Beacon CRM.

## Reguły bezpieczeństwa i privacy

1. Invite capabilities są response-only i nigdy nie trafiają do persistent outbox.
2. Batch ma twardy limit 200; push wave twardy limit 100.
3. Deploy **nigdy** nie uruchamia invite batch ani nearby push wave automatycznie.
4. Consent jest sprawdzany przy invite, auth, wave selection i faktycznej push delivery.
5. Latarnik bearer jest prywatny i revocable; odpowiedzi auth/session mają `private/no-store`.
6. Portal nie wykonuje `innerHTML` na danych z backendu i przyjmuje tylko kontrolowane URL schemes.
7. Workspace scope jest obecny na każdym durable relation/FK.
8. n8n nie może samodzielnie tworzyć listy 200 adresatów; wykonuje listę zwróconą przez CrowdRelay.
9. Sensitive invite exports należy usuwać po wykorzystaniu i nie wrzucać do repo/Drive/Slack.

## Zalecany rollout pierwszych ~200 kontaktów

Nie wysyłać 200 naraz. Zacząć od 15–20 najlepiej ocenionych kandydatów i sprawdzić exchange/open/help
oraz jakość feedbacku. Następnie rozszerzać falami 20–50. Celem nie jest maksymalny opt-in, tylko
zbudowanie użytecznej sieci lokalnych relacji.

Proponowany rytm:

- fala 1: top 20, ręczny review copy i odbiorców,
- po 3–7 dniach: mierzymy exchange/engagement/request friction,
- poprawiamy copy/Press Room,
- kolejne fale 20–50,
- nearby push dopiero dla osób, które świadomie dołączyły.

## n8n / Gmail integration contract

Najbezpieczniejszy flow:

1. operator/approved workflow wywołuje batch invite,
2. CrowdRelay zwraca `invitations[]` z `contactEmail`, `inviteUrl`, `delivery.subject/text`,
3. executor wysyła mail 1:1,
4. executor nie zapisuje invite URL do trwałej tabeli ani centralnego logu,
5. po sendzie krótkotrwały payload jest niszczony,
6. eligibility i suppression pozostają wyłącznie w CrowdRelay.

Nie wolno rekonstruować invite URL z hasha z DB — to celowy one-way capability design.

## Monitoring / failure modes

- Invite exchange failure nie aktywuje profilu częściowo.
- Re-invite revokuje wcześniejsze sesje.
- Brak push runtime nie blokuje portalu; emitter raportuje `eligible` i `pushQueued` osobno.
- Brak CrowdRelay podczas wizyty portalu nie zmienia consent i nie generuje fikcyjnego engagementu.
- Press request/coverage używają trwałych rekordów + outbox, więc staff workflow może być retry-safe.
- Push delivery po cofnięciu zgody jest fail-closed.

## Native Signal — granica v2

Aktualna pełna powierzchnia Latarnika działa przez Virya web, a Virya Signal daje jawny entry point.
Backend już posiada first-class `beacon` audience i session-bound push endpoints. Native adoption
powinna później przenieść bearer do Stronghold i rejestrację FCM do tego samego audience **bez
zmiany tabel, lifecycle ani API semantyki**. Nie należy mieszać `fan_id` z `beacon_id`.

## Release validation

Na maszynie z przypiętym Rust toolchainem uruchomić:

```bash
./scripts/validate-latarnik-release.sh
```

Gate wykonuje `cargo fmt --check`, pełny workspace `clippy -- -D warnings`, wszystkie Rust testy,
OpenAPI/bootstrap validation, contract suite i runtime/release contracts. Nie zastępować tego samymi
source-testami przed produkcyjnym deployem.

## Release / rollback

Migracja 0057 jest addytywna względem foundation Latarnika. Kod API powinien być wdrażany razem z
migracją i aktualnym OpenAPI markerem webu. Rollback aplikacji nie powinien usuwać tabel 0057;
historia engagement/coverage/requestów jest wartościowa i pozostaje kompatybilna jako dane
nieużywane przez starszy binary.

## Definition of done v2

Latarnik v2 jest funkcjonalnie domknięty, gdy działa cały łańcuch:

`candidate → invite → exchange → preferences → nearby event → Press Room → help/decline → request → resolve → coverage → bounded push → leave/revoke`

bez automatycznego masowego outreachu po deployu i bez obchodzenia istniejącego Beacon consent.
