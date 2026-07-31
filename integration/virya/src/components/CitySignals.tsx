import { useEffect, useState } from "preact/hooks";
import type { CitySignal } from "../lib/crowdrelay-client";
import { crowdrelay } from "../lib/crowdrelay";

export function CitySignals() {
  const [cities, setCities] = useState<CitySignal[] | null>(null);

  useEffect(() => {
    let cancelled = false;
    crowdrelay.listCities(100).then((items) => {
      if (!cancelled) setCities(items);
    }).catch((error) => {
      console.error("CrowdRelay city signals failed", error);
      if (!cancelled) setCities([]);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  if (cities === null) return <p aria-busy="true">Ładuję miasta…</p>;
  if (cities.length === 0) return null;

  return (
    <ol>
      {cities.map((city) => (
        <li key={city.slug}>
          <strong>{city.name}</strong> — {city.fan_count} {city.fan_count === 1 ? "osoba" : "osób"}
        </li>
      ))}
    </ol>
  );
}
