import { useState } from "preact/hooks";
import type { AdmissionPassIssued } from "../lib/crowdrelay-client";
import { crowdrelay } from "../lib/crowdrelay";

/** Small operator utility for issuing a pass from a configured event pool. */
export function AdmissionIssuer() {
  const [adminKey, setAdminKey] = useState("");
  const [result, setResult] = useState<AdmissionPassIssued | null>(null);
  const [message, setMessage] = useState("");

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    const data = new FormData(event.currentTarget as HTMLFormElement);
    setMessage("Wystawiam wejściówkę…");
    setResult(null);
    try {
      const issued = await crowdrelay.issueAdmissionPass({
        event_slug: String(data.get("event_slug") ?? "").trim(),
        pool_slug: String(data.get("pool_slug") ?? "").trim(),
        fan_email: String(data.get("fan_email") ?? "").trim(),
        claim_expires_hours: Number(data.get("claim_expires_hours") ?? 72),
      }, adminKey.trim());
      setResult(issued);
      setMessage("Wejściówka została wystawiona. Mail zostanie wysłany przez workflow n8n.");
    } catch (error) {
      const code = typeof error === "object" && error !== null && "status" in error ? Number(error.status) : 0;
      setMessage(code === 401 ? "Nieprawidłowy klucz administratora." : code === 409 ? "Pula jest pełna albo fan ma już wejściówkę." : "Nie udało się wystawić wejściówki.");
    }
  }

  return (
    <section>
      <h1>Wystaw darmową wejściówkę</h1>
      <form onSubmit={submit}>
        <label>Klucz administratora<input type="password" value={adminKey} onInput={(event) => setAdminKey(event.currentTarget.value)} required /></label>
        <label>Slug koncertu<input name="event_slug" required /></label>
        <label>Slug puli<input name="pool_slug" required /></label>
        <label>E-mail fana<input type="email" name="fan_email" required /></label>
        <label>Ważność linku w godzinach<input type="number" name="claim_expires_hours" min="1" max="720" value="72" required /></label>
        <button type="submit">Wystaw</button>
      </form>
      <p role="status">{message}</p>
      {result ? <p>Numer: <strong>{result.public_reference}</strong></p> : null}
    </section>
  );
}
