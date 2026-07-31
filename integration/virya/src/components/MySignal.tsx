import { useEffect, useState } from "preact/hooks";
import type { FanEventInterest, ReferralProgress } from "../lib/crowdrelay-client";
import { crowdrelay } from "../lib/crowdrelay";

interface State {
  progress: ReferralProgress;
  events: FanEventInterest[];
}

export function MySignal() {
  const [data, setData] = useState<State | null>(null);
  const [unauthorized, setUnauthorized] = useState(false);

  useEffect(() => {
    Promise.all([crowdrelay.getReferralProgress(), crowdrelay.listMyEvents()])
      .then(([progress, events]) => setData({ progress, events }))
      .catch((error) => {
        const status = typeof error === "object" && error !== null && "status" in error ? Number(error.status) : 0;
        if (status === 401) setUnauthorized(true);
        else console.error("CrowdRelay private state failed", error);
      });
  }, []);

  if (unauthorized) return <p>Dołącz do Virya Signal, aby zobaczyć swoje polecenia i koncerty.</p>;
  if (!data) return <p aria-busy="true">Ładuję Twój Virya Signal…</p>;

  return (
    <section>
      <h1>Twój Virya Signal</h1>
      <p>Potwierdzone polecenia: {data.progress.qualified_referrals}</p>
      <p>Oczekujące polecenia: {data.progress.pending_referrals}</p>
      {data.progress.next_reward_threshold ? (
        <p>Kolejna nagroda przy {data.progress.next_reward_threshold} poleceniach.</p>
      ) : null}
      <h2>Rabaty</h2>
      {data.progress.coupons.length === 0 ? <p>Brak aktywnych rabatów.</p> : (
        <ul>{data.progress.coupons.map((coupon) => <li key={coupon.id}>{coupon.code} — {coupon.discount_percent}%</li>)}</ul>
      )}
      <h2>Obserwowane koncerty</h2>
      {data.events.length === 0 ? <p>Nie obserwujesz jeszcze żadnego koncertu.</p> : (
        <ul>{data.events.map(({ event }) => <li key={event.id}>{event.title} — {new Date(event.starts_at).toLocaleString()}</li>)}</ul>
      )}
    </section>
  );
}
