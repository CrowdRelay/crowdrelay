import { useEffect, useState } from "preact/hooks";
import { campaignIdFromLocation, crowdrelay } from "../lib/crowdrelay";

interface Props {
  eventSlug: string;
  hasTicket?: boolean;
  hasListen?: boolean;
}

export function EventActions({ eventSlug, hasTicket = true, hasListen = false }: Props) {
  const campaignId = campaignIdFromLocation();
  const [status, setStatus] = useState<"idle" | "saving" | "saved" | "login" | "error">("idle");

  useEffect(() => {
    crowdrelay.trackView(eventSlug, campaignId).catch(() => undefined);
  }, [eventSlug, campaignId]);

  async function registerInterest() {
    setStatus("saving");
    try {
      await crowdrelay.registerEventInterest(eventSlug, {
        ...(campaignId ? { campaign_id: campaignId } : {}),
        source: "virya_live_page",
      });
      setStatus("saved");
    } catch (error) {
      const statusCode = typeof error === "object" && error !== null && "status" in error ? Number(error.status) : 0;
      setStatus(statusCode === 401 ? "login" : "error");
    }
  }

  async function share() {
    await crowdrelay.trackShare(eventSlug, campaignId).catch(() => undefined);
    const url = window.location.href;
    if (navigator.share) {
      await navigator.share({ title: document.title, url }).catch(() => undefined);
    } else {
      await navigator.clipboard.writeText(url);
    }
  }

  return (
    <div>
      {hasTicket ? <a href={crowdrelay.eventTicketUrl(eventSlug, campaignId)}>Kup bilet</a> : null}
      {hasListen ? <a href={crowdrelay.eventListenUrl(eventSlug, campaignId)}>Posłuchaj Viryi</a> : null}
      <a href={crowdrelay.eventCalendarUrl(eventSlug, campaignId)}>Dodaj do kalendarza</a>
      <button type="button" onClick={share}>Udostępnij</button>
      <button type="button" disabled={status === "saving" || status === "saved"} onClick={registerInterest}>
        {status === "saving" ? "Zapisuję…" : status === "saved" ? "Jesteś na liście" : "Przypomnij mi o koncercie"}
      </button>
      {status === "login" ? <p>Najpierw dołącz do Virya Signal na stronie /join.</p> : null}
      {status === "error" ? <p>Nie udało się zapisać zainteresowania. Spróbuj ponownie.</p> : null}
    </div>
  );
}
