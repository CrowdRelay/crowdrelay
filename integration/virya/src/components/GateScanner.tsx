import { useEffect, useRef, useState } from "preact/hooks";
import type { AdmissionRedemptionResult } from "../lib/crowdrelay-client";
import { crowdrelay } from "../lib/crowdrelay";

type ScanStatus = "idle" | "starting" | "scanning" | "redeeming" | "error";

/** Mobile-first QR scanner with a manual public-reference fallback. */
export function GateScanner() {
  const videoRef = useRef<HTMLVideoElement>(null);
  const controlsRef = useRef<{ stop(): void } | null>(null);
  const redeemingRef = useRef(false);
  const [staffKey, setStaffKey] = useState("");
  const [eventSlug, setEventSlug] = useState("");
  const [status, setStatus] = useState<ScanStatus>("idle");
  const [result, setResult] = useState<AdmissionRedemptionResult | null>(null);
  const [message, setMessage] = useState("");

  useEffect(() => {
    setStaffKey(sessionStorage.getItem("crowdrelay_staff_key") ?? "");
    setEventSlug(sessionStorage.getItem("crowdrelay_gate_event") ?? "");
    return () => controlsRef.current?.stop();
  }, []);

  async function startScanner() {
    if (!staffKey.trim() || !eventSlug.trim()) {
      setMessage("Podaj wydarzenie i klucz obsługi bramki.");
      return;
    }
    sessionStorage.setItem("crowdrelay_staff_key", staffKey.trim());
    sessionStorage.setItem("crowdrelay_gate_event", eventSlug.trim());
    setStatus("starting");
    setMessage("");
    try {
      const { BrowserQRCodeReader } = await import("@zxing/browser");
      const reader = new BrowserQRCodeReader();
      const controls = await reader.decodeFromVideoDevice(undefined, videoRef.current!, (decoded, error) => {
        if (decoded && !redeemingRef.current) {
          controlsRef.current?.stop();
          void redeem({ qr_token: decoded.getText() });
        } else if (error && error.name !== "NotFoundException") {
          setMessage("Kamera nie mogła odczytać kodu.");
        }
      });
      controlsRef.current = controls;
      setStatus("scanning");
    } catch (error) {
      console.error("Gate scanner failed", error);
      setStatus("error");
      redeemingRef.current = false;
      setMessage("Nie udało się uruchomić kamery. Użyj numeru wejściówki poniżej.");
    }
  }

  async function manualRedeem(event: SubmitEvent) {
    event.preventDefault();
    const form = event.currentTarget as HTMLFormElement;
    const reference = String(new FormData(form).get("reference") ?? "").trim().toUpperCase();
    if (!reference || !staffKey.trim() || !eventSlug.trim()) {
      setMessage("Podaj wydarzenie, klucz obsługi i numer wejściówki.");
      return;
    }
    sessionStorage.setItem("crowdrelay_staff_key", staffKey.trim());
    sessionStorage.setItem("crowdrelay_gate_event", eventSlug.trim());
    await redeem({ public_reference: reference });
  }

  async function redeem(input: { qr_token?: string; public_reference?: string }) {
    if (redeemingRef.current) return;
    redeemingRef.current = true;
    setStatus("redeeming");
    setResult(null);
    setMessage("");
    try {
      const next = await crowdrelay.redeemAdmissionPass(
        { event_slug: eventSlug.trim(), ...input },
        staffKey.trim(),
      );
      setResult(next);
      setStatus("idle");
      redeemingRef.current = false;
      if (navigator.vibrate) navigator.vibrate(next.status === "redeemed" ? [100, 50, 100] : [400]);
    } catch (error) {
      const code = typeof error === "object" && error !== null && "status" in error ? Number(error.status) : 0;
      setStatus("error");
      redeemingRef.current = false;
      setMessage(code === 401 ? "Nieprawidłowy klucz obsługi." : code === 409 ? "Kod wygasł albo wejściówka ma konflikt stanu." : "Nie udało się zweryfikować wejściówki.");
    }
  }

  function nextScan() {
    redeemingRef.current = false;
    setResult(null);
    setMessage("");
    void startScanner();
  }

  return (
    <section aria-labelledby="gate-title">
      <h1 id="gate-title">Virya — kontrola wejściówek</h1>
      <label>
        Slug wydarzenia
        <input
          type="text"
          autoComplete="off"
          value={eventSlug}
          onInput={(event) => setEventSlug(event.currentTarget.value)}
          placeholder="virya-live-2026"
        />
      </label>
      <label>
        Klucz obsługi
        <input type="password" autoComplete="off" value={staffKey} onInput={(event) => setStaffKey(event.currentTarget.value)} />
      </label>
      <div>
        <button type="button" disabled={status === "starting" || status === "scanning" || status === "redeeming"} onClick={startScanner}>
          {status === "starting" ? "Uruchamiam…" : status === "scanning" ? "Skanuję…" : "Skanuj QR"}
        </button>
        <button type="button" onClick={() => { controlsRef.current?.stop(); setStatus("idle"); }}>Zatrzymaj kamerę</button>
      </div>
      <video ref={videoRef} muted playsInline style={{ width: "100%", maxWidth: "36rem" }} />
      <form onSubmit={manualRedeem}>
        <label>
          Numer wejściówki
          <input name="reference" placeholder="VIRYA-…" autoCapitalize="characters" />
        </label>
        <button type="submit" disabled={status === "redeeming"}>Sprawdź ręcznie</button>
      </form>
      <p role="status" aria-live="assertive">{message}</p>
      {result ? (
        <article data-admission-status={result.status}>
          <h2>{result.status === "redeemed" ? "WEJŚCIE POTWIERDZONE" : result.status === "already_redeemed" ? "JUŻ WYKORZYSTANA" : result.status.toUpperCase()}</h2>
          <p>{result.holder_name ?? result.holder_email_masked}</p>
          <p>{result.public_reference}</p>
          {result.redeemed_at ? <p>{new Date(result.redeemed_at).toLocaleString()}</p> : null}
          <button type="button" onClick={nextScan}>Następna osoba</button>
        </article>
      ) : null}
    </section>
  );
}
