import { useEffect, useRef, useState } from "preact/hooks";
import type { AdmissionPass } from "../lib/crowdrelay-client";
import { crowdrelay } from "../lib/crowdrelay";

interface PassState {
  pass: AdmissionPass | null;
  status: "loading" | "ready" | "invalid" | "error";
  message: string;
}

const PENDING_CLAIM_STORAGE_KEY = "crowdrelay_pending_admission_claim";

/** Claims a winner link, displays pass details, and keeps its gate QR short-lived. */
export function WinnerPass() {
  const [state, setState] = useState<PassState>({ pass: null, status: "loading", message: "" });
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    let cancelled = false;
    async function load() {
      const pendingClaim = preparePendingClaim();
      try {
        const pass = pendingClaim
          ? await crowdrelay.claimAdmissionPass(pendingClaim.token, pendingClaim.idempotencyKey)
          : await crowdrelay.getMyAdmissionPass();
        if (pendingClaim) {
          clearPendingClaim();
          removeFragment();
        }
        if (!cancelled) setState({ pass, status: "ready", message: "" });
      } catch (error) {
        const code = typeof error === "object" && error !== null && "status" in error ? Number(error.status) : 0;
        if (pendingClaim && [404, 409, 422].includes(code)) {
          clearPendingClaim();
          removeFragment();
        }
        if (!cancelled) setState({ pass: null, status: code === 401 || code === 404 || code === 409 || code === 422 ? "invalid" : "error", message: "" });
      }
    }
    void load();
    return () => { cancelled = true; };
  }, []);

  useEffect(() => {
    if (state.status !== "ready" || state.pass?.status !== "claimed") return;
    let cancelled = false;
    let timer = 0;

    async function refreshQr() {
      try {
        const qr = await crowdrelay.getAdmissionQr();
        if (cancelled || !canvasRef.current) return;
        const { toCanvas } = await import("qrcode");
        await toCanvas(canvasRef.current, qr.token, { width: 280, margin: 1, errorCorrectionLevel: "M" });
        if (!cancelled) setState((current) => ({ ...current, message: `Kod ważny do ${new Date(qr.expires_at).toLocaleTimeString()}` }));
      } catch {
        if (!cancelled) setState((current) => ({ ...current, message: "Odświeżenie kodu nie powiodło się. Sprawdź internet." }));
      } finally {
        if (!cancelled) timer = window.setTimeout(refreshQr, 15_000);
      }
    }

    void refreshQr();
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [state.pass?.pass_id, state.pass?.status]);

  if (state.status === "loading") return <p aria-busy="true">Ładuję wejściówkę…</p>;
  if (state.status === "invalid") return <p>Nie znaleziono aktywnej wejściówki. Link mógł wygasnąć albo zostać już wykorzystany.</p>;
  if (state.status === "error" || !state.pass) return <p>Nie udało się pobrać wejściówki. Spróbuj ponownie za chwilę.</p>;

  const pass = state.pass;
  return (
    <section aria-labelledby="winner-pass-title">
      <h1 id="winner-pass-title">Twoja wejściówka na {pass.event_title}</h1>
      <p>{new Date(pass.starts_at).toLocaleString(document.documentElement.lang || "pl-PL")}{pass.venue ? `, ${pass.venue}` : ""}</p>
      <p>Posiadacz: {pass.holder_name ?? pass.holder_email_masked}</p>
      <p>Numer: <strong>{pass.public_reference}</strong></p>
      {pass.status === "claimed" ? (
        <div>
          <canvas ref={canvasRef} aria-label="Rotujący kod QR wejściówki" />
          <p role="status">{state.message}</p>
          <p>Pokaż ten ekran obsłudze przy wejściu. Kod odświeża się automatycznie.</p>
        </div>
      ) : pass.status === "redeemed" ? (
        <p>Wejściówka została wykorzystana{pass.redeemed_at ? ` ${new Date(pass.redeemed_at).toLocaleString()}` : ""}.</p>
      ) : (
        <p>Wejściówka ma status: {pass.status}.</p>
      )}
    </section>
  );
}

function fragmentToken(): string | null {
  const raw = window.location.hash.replace(/^#/, "");
  if (!raw) return null;
  const params = new URLSearchParams(raw);
  return params.get("token") ?? (/^[a-fA-F0-9]{64}$/.test(raw) ? raw : null);
}

interface PendingClaim {
  token: string;
  idempotencyKey: string;
}

function preparePendingClaim(): PendingClaim | null {
  const tokenFromFragment = fragmentToken();
  const stored = readPendingClaim();
  if (!tokenFromFragment) return stored;

  const pendingClaim = stored?.token === tokenFromFragment
    ? stored
    : { token: tokenFromFragment, idempotencyKey: crypto.randomUUID() };
  try {
    sessionStorage.setItem(PENDING_CLAIM_STORAGE_KEY, JSON.stringify(pendingClaim));
    removeFragment();
  } catch {
    // If storage is unavailable, keep the fragment until the exchange succeeds
    // so a transient network failure cannot irreversibly consume the claim.
  }
  return pendingClaim;
}

function readPendingClaim(): PendingClaim | null {
  try {
    const raw = sessionStorage.getItem(PENDING_CLAIM_STORAGE_KEY);
    if (!raw) return null;
    const value: unknown = JSON.parse(raw);
    if (
      typeof value === "object"
      && value !== null
      && "token" in value
      && typeof value.token === "string"
      && /^[a-fA-F0-9]{64}$/.test(value.token)
      && "idempotencyKey" in value
      && typeof value.idempotencyKey === "string"
      && value.idempotencyKey.length > 0
    ) {
      return { token: value.token, idempotencyKey: value.idempotencyKey };
    }
    sessionStorage.removeItem(PENDING_CLAIM_STORAGE_KEY);
  } catch {
    // Storage may be disabled; the fragment path still supports a one-shot exchange.
  }
  return null;
}

function clearPendingClaim() {
  try {
    sessionStorage.removeItem(PENDING_CLAIM_STORAGE_KEY);
  } catch {
    // A disabled storage backend has nothing durable to clear.
  }
}

function removeFragment() {
  history.replaceState(null, "", `${location.pathname}${location.search}`);
}
