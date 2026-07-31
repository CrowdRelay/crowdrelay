import { useEffect, useState } from "preact/hooks";
import type { CitySignal } from "../lib/crowdrelay-client";
import { campaignIdFromLocation, crowdrelay, referralCodeFromLocation } from "../lib/crowdrelay";

type State = "loading" | "ready" | "saving" | "pending" | "saved" | "error";

export function JoinSignalForm() {
  const [cities, setCities] = useState<CitySignal[]>([]);
  const [state, setState] = useState<State>("loading");
  const [message, setMessage] = useState("");
  const [referralUrl, setReferralUrl] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    crowdrelay
      .listCities(100)
      .then((items) => {
        if (cancelled) return;
        setCities(items);
        setState("ready");
      })
      .catch(() => {
        if (cancelled) return;
        setMessage("Nie udało się pobrać listy miast. Spróbuj ponownie za chwilę.");
        setState("error");
      });
    return () => {
      cancelled = true;
    };
  }, []);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    const form = event.currentTarget as HTMLFormElement;
    const data = new FormData(form);
    const email = String(data.get("email") ?? "").trim();
    const citySlug = String(data.get("city") ?? "").trim();
    const displayName = String(data.get("display_name") ?? "").trim();
    const consent = data.get("consent") === "on";

    if (!email || !citySlug || !consent) {
      setMessage("Podaj e-mail, wybierz miasto i zaakceptuj zgodę.");
      setState("error");
      return;
    }

    setState("saving");
    setMessage("");
    try {
      const campaignId = campaignIdFromLocation();
      const referralCode = referralCodeFromLocation();
      const result = await crowdrelay.signupFan({
        email,
        city_slug: citySlug,
        ...(displayName ? { display_name: displayName } : {}),
        ...(campaignId ? { campaign_id: campaignId } : {}),
        ...(referralCode ? { referral_code: referralCode } : {}),
        locale: document.documentElement.lang || "pl",
        consent: {
          marketing: true,
          policy_version: "virya-signal-v1",
        },
      });
      setReferralUrl(result.referral_url);
      if (result.confirmation_required) {
        setMessage("Sprawdź skrzynkę i potwierdź adres e-mail. Dopiero wtedy dołączysz do Virya Signal.");
        setState("pending");
      } else {
        setMessage("Jesteś w Virya Signal. Udostępnij swój link znajomym.");
        setState("saved");
      }
      form.reset();
    } catch (error) {
      console.error("CrowdRelay signup failed", error);
      setMessage("Nie udało się zapisać. Sprawdź dane i spróbuj ponownie.");
      setState("error");
    }
  }

  async function copyReferral() {
    if (!referralUrl) return;
    await navigator.clipboard.writeText(referralUrl);
    setMessage("Link polecający skopiowany.");
  }

  return (
    <section aria-labelledby="virya-signal-title">
      <h1 id="virya-signal-title">Powiedz nam, gdzie mamy zagrać</h1>
      <p>Zapisz się do Virya Signal, wybierz miasto i otrzymuj informacje o koncertach oraz nagrodach.</p>
      <form onSubmit={submit}>
        <label>
          E-mail
          <input type="email" name="email" autoComplete="email" required />
        </label>
        <label>
          Imię lub nick (opcjonalnie)
          <input type="text" name="display_name" autoComplete="nickname" maxLength={160} />
        </label>
        <label>
          Miasto
          <select name="city" required disabled={state === "loading" || state === "saving"}>
            <option value="">Wybierz miasto</option>
            {cities.map((city) => (
              <option key={city.slug} value={city.slug}>
                {city.name} ({city.fan_count})
              </option>
            ))}
          </select>
        </label>
        <label>
          <input type="checkbox" name="consent" required />
          Chcę otrzymywać informacje od Viryi. Zgodę mogę wycofać w każdej chwili.
        </label>
        <button type="submit" disabled={state === "loading" || state === "saving"}>
          {state === "saving" ? "Zapisuję…" : "Dołączam"}
        </button>
      </form>
      <p role="status" aria-live="polite">{message}</p>
      {referralUrl ? (
        <div>
          <label>
            Twój link polecający
            <input type="url" readOnly value={referralUrl} />
          </label>
          <button type="button" onClick={copyReferral}>Kopiuj link</button>
        </div>
      ) : null}
    </section>
  );
}
