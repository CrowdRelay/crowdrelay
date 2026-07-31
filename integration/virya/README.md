# Integracja CrowdRelay z virya.music

Ten katalog zawiera gotowe komponenty Astro/Preact oraz przeglądarkowy klient API. CrowdRelay nie uczestniczy w pierwszym renderze strony: dynamiczne elementy są ładowane jako wyspy, a awaria API nie blokuje strony, EPK ani sklepu.

## Pliki do skopiowania

- `src/lib/crowdrelay-client.ts` i `src/lib/crowdrelay.ts`;
- wybrane komponenty z `src/components/`;
- strony z `src/pages/`;
- reguły z `public/_redirects.snippet` albo `netlify.toml.snippet`.

Klient w `src/lib/crowdrelay-client.ts` jest kopią pakietu `packages/crowdrelay-js` i nie wymaga publikowania paczki npm.

## Zależności

```bash
npm install qrcode @zxing/browser
npm install --save-dev @types/qrcode
```

## Konfiguracja Netlify

```env
PUBLIC_CROWDRELAY_API_URL=https://signal-api.virya.music/v1/
```

To jedyna publiczna zmienna CrowdRelay. Kluczy administratora, obsługi bramki, commerce, webhooków i bazy nie wolno umieszczać w zmiennych `PUBLIC_*`.

## Podstrony

- `/join` — `JoinSignalForm client:visible`;
- `/cities` — `CitySignals client:visible`;
- `/concerts` — `UpcomingEvents client:visible`;
- `/live/[slug]` — treść statyczna oraz `EventActions client:visible`;
- `/my-signal` — `MySignal client:only="preact"`;
- `/signal/confirm` — `ConfirmSignal client:only="preact"`;
- `/signal/unsubscribe` — `UnsubscribeSignal client:only="preact"`;
- `/win` — `WinnerPass client:only="preact"`;
- `/staff/scan` — `GateScanner client:only="preact"`;
- `/staff/admission` — `AdmissionIssuer client:only="preact"`.

Nie pobieraj danych CrowdRelay w globalnym layoucie. Publiczne komponenty powinny mieć czytelny stan awaryjny albo znikać bez wpływu na pozostałą treść.

## Bezpieczeństwo stron obsługi

Strony `/staff/*` zabezpiecz dodatkowo przez Netlify Identity albo Basic Auth. Klucze są wprowadzane przez operatora i zapisywane wyłącznie w `sessionStorage` na czas karty przeglądarki.

Skaner aparatem wymaga nagłówka odpowiedzi strony Viryi:

```text
Permissions-Policy: camera=(self)
```

Nie ustawiaj `camera=()` globalnie dla `/staff/scan`.

## Linki markowe

Reguły `/r/*` i `/go/*` utrzymują adresy `virya.music`, ale przekazują obsługę do szybkiego redirectu CrowdRelay. API ustawia cookie atrybucji na `signal-api.virya.music`, po czym odsyła użytkownika na stronę Viryi.

## Linki z tajnymi tokenami

Wiadomości potwierdzające, wypisanie i wygrane używają fragmentu URL:

```text
/signal/confirm#token=...
/signal/unsubscribe#token=...
/win#token=...
```

Fragment nie jest wysyłany do serwera strony ani logów reverse proxy. Komponent
przenosi token wraz ze stałym kluczem idempotency do `sessionStorage`, usuwa
fragment z paska adresu i czyści dane tymczasowe dopiero po udanej wymianie na
sesję HttpOnly. Dzięki temu utrata odpowiedzi albo odświeżenie karty nie blokuje
bezpowrotnie wejściówki.

## Eventy i pule wejściówek

Koncerty oraz ograniczone pule są konfigurowane w `deploy/bootstrap.production.json` i aktualizowane przez idempotentne polecenie `setup`. Wdrożenie wykonuje `./crowdrelayctl deploy`, a stan API potwierdza `./crowdrelayctl verify`.
