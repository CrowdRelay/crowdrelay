import { CrowdRelayClient } from "./crowdrelay-client";

const configuredUrl = import.meta.env.PUBLIC_CROWDRELAY_API_URL as string | undefined;

if (!configuredUrl) {
  throw new Error("PUBLIC_CROWDRELAY_API_URL is required");
}

export const crowdrelay = new CrowdRelayClient({
  baseUrl: configuredUrl,
  timeoutMs: 1_500,
});

export function campaignIdFromLocation(): string | undefined {
  if (typeof window === "undefined") return undefined;
  const value = new URLSearchParams(window.location.search).get("campaign_id");
  return value || undefined;
}

export function referralCodeFromLocation(): string | undefined {
  if (typeof window === "undefined") return undefined;
  const value = new URLSearchParams(window.location.search).get("ref");
  return value || undefined;
}
