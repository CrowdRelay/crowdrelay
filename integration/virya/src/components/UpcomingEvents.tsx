import { useEffect, useState } from "preact/hooks";
import type { PublicEvent } from "../lib/crowdrelay-client";
import { crowdrelay } from "../lib/crowdrelay";

export function UpcomingEvents() {
  const [events, setEvents] = useState<PublicEvent[] | null>(null);

  useEffect(() => {
    let cancelled = false;
    crowdrelay
      .listEvents(20)
      .then((items) => {
        if (!cancelled) setEvents(items);
      })
      .catch((error) => {
        console.error("CrowdRelay event list failed", error);
        if (!cancelled) setEvents([]);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (events === null) return <p aria-busy="true">Ładuję koncerty…</p>;
  if (events.length === 0) return <p>Nowe koncerty pojawią się tutaj.</p>;

  return (
    <ol>
      {events.map((event) => (
        <li key={event.id}>
          <a href={`/live/${event.slug}`}>
            <strong>{event.title}</strong>
          </a>
          <span>
            {" — "}
            {new Date(event.starts_at).toLocaleString(document.documentElement.lang || "pl-PL")}
            {event.city ? `, ${event.city.name}` : ""}
            {event.venue ? `, ${event.venue}` : ""}
          </span>
        </li>
      ))}
    </ol>
  );
}
