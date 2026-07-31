import { useEffect, useState } from "preact/hooks";
import { crowdrelay } from "../lib/crowdrelay";

interface ConfirmationState {
  status: "loading" | "success" | "invalid" | "error";
  referralUrl?: string;
}

const PENDING_CONFIRMATION_STORAGE_KEY = "crowdrelay_pending_fan_confirmation";

/** Confirms a pending Virya Signal signup without putting the token in server logs. */
export function ConfirmSignal() {
  const [state, setState] = useState<ConfirmationState>({ status: "loading" });

  useEffect(() => {
    let cancelled = false;
    const pending = preparePendingConfirmation();
    if (!pending) {
      setState({ status: "invalid" });
      return;
    }

    crowdrelay.confirmFan(pending.token, pending.idempotencyKey)
      .then((result) => {
        clearPendingConfirmation();
        removeFragment();
        if (!cancelled) {
          setState({ status: "success", referralUrl: result.referral_url });
        }
      })
      .catch((error) => {
        const status = typeof error === "object" && error !== null && "status" in error ? Number(error.status) : 0;
        const definitive = status === 404 || status === 409 || status === 422;
        if (definitive) {
          clearPendingConfirmation();
          removeFragment();
        }
        if (!cancelled) {
          setState({ status: definitive ? "invalid" : "error" });
        }
      });
    return () => { cancelled = true; };
  }, []);

  async function copyReferral() {
    if (!state.referralUrl) return;
    await navigator.clipboard.writeText(state.referralUrl);
  }

  if (state.status === "loading") return <p aria-busy="true">Potwierdzam zapis…</p>;
  if (state.status === "invalid") return <p>Ten link potwierdzający jest nieprawidłowy, wygasł albo został już użyty.</p>;
  if (state.status === "error") return <p>Nie udało się potwierdzić zapisu. Spróbuj ponownie za chwilę.</p>;

  return (
    <section>
      <h1>Witaj w Virya Signal</h1>
      <p>Adres został potwierdzony. Od teraz możemy informować Cię o koncertach, nagrodach i nowych materiałach.</p>
      {state.referralUrl ? (
        <div>
          <label>
            Twój link polecający
            <input type="url" readOnly value={state.referralUrl} />
          </label>
          <button type="button" onClick={copyReferral}>Kopiuj link</button>
        </div>
      ) : null}
      <p><a href="/my-signal">Przejdź do swojego Virya Signal</a></p>
    </section>
  );
}

interface PendingConfirmation {
  token: string;
  idempotencyKey: string;
}

function preparePendingConfirmation(): PendingConfirmation | null {
  const fragment = fragmentConfirmation();
  const stored = readPendingConfirmation();
  if (!fragment) return stored;

  const pending = stored?.token === fragment.token
    ? stored
    : {
        token: fragment.token,
        idempotencyKey: validIdempotencyKey(fragment.idempotencyKey)
          ? fragment.idempotencyKey
          : crypto.randomUUID(),
      };
  try {
    sessionStorage.setItem(PENDING_CONFIRMATION_STORAGE_KEY, JSON.stringify(pending));
    removeFragment();
  } catch {
    // Fragments are not sent in HTTP requests. Keeping the token and stable
    // idempotency key there preserves a safe retry path when storage is disabled.
    const params = new URLSearchParams({
      token: pending.token,
      idempotency_key: pending.idempotencyKey,
    });
    history.replaceState(null, "", `${location.pathname}${location.search}#${params}`);
  }
  return pending;
}

function fragmentConfirmation(): PendingConfirmation | null {
  const raw = window.location.hash.replace(/^#/, "");
  if (!raw) return null;
  const params = new URLSearchParams(raw);
  const token = params.get("token") ?? (/^[a-fA-F0-9]{64}$/.test(raw) ? raw : null);
  if (!token || !/^[a-fA-F0-9]{64}$/.test(token)) return null;
  const idempotencyKey = params.get("idempotency_key");
  return {
    token,
    idempotencyKey: idempotencyKey && validIdempotencyKey(idempotencyKey) ? idempotencyKey : "",
  };
}

function readPendingConfirmation(): PendingConfirmation | null {
  try {
    const raw = sessionStorage.getItem(PENDING_CONFIRMATION_STORAGE_KEY);
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
      && validIdempotencyKey(value.idempotencyKey)
    ) {
      return { token: value.token, idempotencyKey: value.idempotencyKey };
    }
    sessionStorage.removeItem(PENDING_CONFIRMATION_STORAGE_KEY);
  } catch {
    // Storage may be unavailable; the fragment remains the durable fallback.
  }
  return null;
}

function clearPendingConfirmation() {
  try {
    sessionStorage.removeItem(PENDING_CONFIRMATION_STORAGE_KEY);
  } catch {
    // A disabled storage backend has nothing durable to clear.
  }
}

function validIdempotencyKey(value: string): boolean {
  return value.length >= 8
    && value.length <= 128
    && [...value].every((character) => {
      const code = character.charCodeAt(0);
      return code >= 0x21 && code <= 0x7e && character !== "\"" && character !== "\\";
    });
}

function removeFragment() {
  history.replaceState(null, "", `${location.pathname}${location.search}`);
}
