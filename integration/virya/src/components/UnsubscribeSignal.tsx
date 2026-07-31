import { useEffect, useState } from "preact/hooks";
import { crowdrelay } from "../lib/crowdrelay";

/** Uses a one-time fragment token to remove a fan from marketing communication. */
export function UnsubscribeSignal() {
  const [status, setStatus] = useState<"loading" | "success" | "invalid" | "error">("loading");

  useEffect(() => {
    const raw = window.location.hash.replace(/^#/, "");
    const params = new URLSearchParams(raw);
    const token = params.get("token") ?? (/^[a-fA-F0-9]{64}$/.test(raw) ? raw : null);
    history.replaceState(null, "", `${location.pathname}${location.search}`);
    if (!token) {
      setStatus("invalid");
      return;
    }
    crowdrelay.unsubscribeFan(token)
      .then(() => setStatus("success"))
      .catch((error) => {
        const code = typeof error === "object" && error !== null && "status" in error ? Number(error.status) : 0;
        setStatus(code === 404 || code === 409 || code === 422 ? "invalid" : "error");
      });
  }, []);

  if (status === "loading") return <p aria-busy="true">Wycofuję zgodę…</p>;
  if (status === "success") return <p>Gotowe. Nie będziemy już wysyłać Ci wiadomości marketingowych.</p>;
  if (status === "invalid") return <p>Link jest nieprawidłowy, wygasł albo został już użyty.</p>;
  return <p>Nie udało się wykonać operacji. Spróbuj ponownie za chwilę.</p>;
}
