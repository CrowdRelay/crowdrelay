export interface EventCity {
  id: string;
  slug: string;
  name: string;
  country_code: string;
  region: string | null;
}

export interface PublicEvent {
  id: string;
  slug: string;
  title: string;
  description: string | null;
  city: EventCity | null;
  venue: string | null;
  venue_address: string | null;
  timezone: string;
  starts_at: string;
  doors_at: string | null;
  ends_at: string | null;
  ticket_url: string | null;
  listen_url: string | null;
  image_url: string | null;
  trailer_url: string | null;
  external_event_url: string | null;
  updated_at: string;
}

export type TicketSaleState =
  | "upcoming"
  | "open"
  | "closed"
  | "sold_out"
  | "inactive"
  | "event_unavailable";

export interface TicketTypeOffer {
  id: string;
  slug: string;
  name: string;
  description: string | null;
  price_gross_minor: number;
  capacity: number | null;
  sold: number;
  reserved: number;
  available: number;
  sort_order: number;
  active: boolean;
}

export interface TicketSaleOffer {
  event_id: string;
  event_slug: string;
  event_title: string;
  event_status: string;
  venue: string | null;
  timezone: string;
  starts_at: string;
  currency: string;
  vat_rate_basis_points: number;
  capacity: number;
  sold: number;
  reserved: number;
  available: number;
  max_per_order: number;
  sales_open_at: string;
  sales_close_at: string;
  active: boolean;
  sales_state: TicketSaleState;
  ticket_types: TicketTypeOffer[];
}

export interface ConfigureTicketTypeInput {
  slug: string;
  name: string;
  description?: string | null;
  price_gross_minor: number;
  capacity?: number | null;
  sort_order: number;
  active: boolean;
}

export interface ConfigureTicketSaleInput {
  currency: string;
  vat_rate_basis_points: number;
  capacity: number;
  max_per_order: number;
  hold_seconds: number;
  sales_open_at: string;
  sales_close_at: string;
  active: boolean;
  ticket_types: ConfigureTicketTypeInput[];
}

export interface TicketOrderSummary {
  order_id: string;
  public_reference: string;
  status: string;
  buyer_email_masked: string;
  amount_gross_minor: number;
  amount_refunded_minor: number;
  currency: string;
  paid_at: string | null;
}

export interface AdminTicketingOverview {
  sale: TicketSaleOffer;
  reserved_orders: number;
  checkout_created_orders: number;
  reserved_tickets: number;
  paid_orders: number;
  paid_tickets: number;
  gross_sales_minor: number;
  refunded_minor: number;
  recent_orders: TicketOrderSummary[];
}

export interface CitySignal {
  slug: string;
  name: string;
  country_code: string;
  fan_count: number;
}

export interface ConsentInput {
  marketing: true;
  policy_version: string;
}

export interface FanSignupInput {
  email: string;
  city_slug: string;
  display_name?: string;
  locale?: string;
  referral_code?: string;
  campaign_id?: string;
  consent: ConsentInput;
}

export interface FanSignupResult {
  fan_id: string;
  status: "pending" | "active";
  referral_url: string | null;
  confirmation_required: boolean;
}

export interface FanConfirmationResult {
  fan_id: string;
  status: "active";
  referral_url: string;
}

export interface FanUnsubscribeResult {
  fan_id: string;
  status: "unsubscribed" | "suppressed";
}

export type AdmissionPassStatus = "issued" | "claimed" | "redeemed" | "revoked" | "expired";

export interface AdmissionPass {
  pass_id: string;
  session_id: string | null;
  event_id: string;
  event_slug: string;
  event_title: string;
  venue: string | null;
  starts_at: string;
  holder_name: string | null;
  holder_email_masked: string;
  public_reference: string;
  status: AdmissionPassStatus;
  session_expires_at: string;
  redeemed_at: string | null;
}

export interface AdmissionPassIssued {
  pass_id: string;
  event_id: string;
  fan_id: string;
  public_reference: string;
  claim_token: string;
  claim_expires_at: string;
  created: boolean;
}

export interface AdmissionQr {
  token: string;
  expires_at: string;
}

export type AdmissionRedemptionStatus =
  | "redeemed"
  | "already_redeemed"
  | "revoked"
  | "expired"
  | "not_claimed";

export interface AdmissionRedemptionResult {
  pass_id: string;
  event_id: string;
  public_reference: string;
  holder_name: string | null;
  holder_email_masked: string;
  status: AdmissionRedemptionStatus;
  redeemed_at: string | null;
}

export interface EventInterestInput {
  campaign_id?: string;
  source?: string;
}

export interface EventInterestResult {
  event_id: string;
  fan_id: string;
  created: boolean;
  reminder_count: number;
}

export interface ConcertCheckinResult {
  event_id: string;
  event_slug: string;
  campaign_id: string;
  created: boolean;
  checked_in_at: string;
}

export interface CreateConcertQrCampaignInput {
  event_slug: string;
  label: string;
  valid_from: string;
  valid_until: string;
  max_checkins?: number;
}

export interface StaffConcertEvent {
  id: string;
  slug: string;
  title: string;
  venue: string | null;
  starts_at: string;
}

export interface ConcertQrOverview {
  events: StaffConcertEvent[];
  campaigns: ConcertQrCampaign[];
}

export interface ConcertQrCampaign {
  id: string;
  event_id: string;
  event_slug: string;
  event_title: string;
  venue: string | null;
  starts_at: string;
  label: string;
  valid_from: string;
  valid_until: string;
  max_checkins: number | null;
  checkin_count: number;
  active: boolean;
  revoked_at: string | null;
  created_at: string;
  token: string | null;
}

export interface FanEventInterest {
  event: PublicEvent;
  interested_at: string;
}

export interface MerchCoupon {
  id: string;
  reward_grant_id: string;
  reward_rule_id: string;
  code: string;
  discount_percent: number;
  max_uses: number;
  used_count: number;
  status: "issued" | "redeemed" | "expired" | "revoked";
  expires_at: string | null;
}

export interface PhysicalRewardGrant {
  reward_grant_id: string;
  reward_rule_id: string;
  item_name: string;
  sku: string;
  status: "issued" | "fulfilled" | "expired" | "revoked";
  granted_at: string;
  expires_at: string | null;
}

export interface WeightedDrawEntry {
  draw_id: string;
  slug: string;
  name: string;
  prize_kind: "admission_pass" | "physical_item";
  closes_at: string;
  draw_at: string;
  qualified_referrals: number;
  base_entries: number;
  referral_entries: number;
  concert_checkins: number;
  checkin_entries: number;
  total_entries: number;
  max_entries: number;
}

export interface ReferralProgress {
  referral_code: string;
  qualified_referrals: number;
  pending_referrals: number;
  next_reward_threshold: number | null;
  draw_entries: WeightedDrawEntry[];
  coupons: MerchCoupon[];
  physical_rewards: PhysicalRewardGrant[];
}

export interface ProblemDetails {
  type: string;
  title: string;
  status: number;
  detail?: string;
  request_id?: string;
}

export class CrowdRelayError extends Error {
  public readonly status: number;
  public readonly problem: ProblemDetails | undefined;

  public constructor(status: number, message: string, problem?: ProblemDetails) {
    super(message);
    this.name = "CrowdRelayError";
    this.status = status;
    this.problem = problem;
  }
}

export interface CrowdRelayClientOptions {
  baseUrl: string;
  timeoutMs?: number;
  fetch?: typeof globalThis.fetch;
}

export class CrowdRelayClient {
  readonly #baseUrl: URL;
  readonly #timeoutMs: number;
  readonly #fetch: typeof globalThis.fetch;

  public constructor(options: CrowdRelayClientOptions) {
    const baseUrl = new URL(ensureTrailingSlash(options.baseUrl));
    if (!["http:", "https:"].includes(baseUrl.protocol) || baseUrl.username !== "" || baseUrl.password !== "") {
      throw new TypeError("CrowdRelay baseUrl must be an HTTP(S) URL without credentials");
    }
    const timeoutMs = options.timeoutMs ?? 5_000;
    if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) {
      throw new RangeError("CrowdRelay timeoutMs must be a positive finite number");
    }
    const fetchImplementation = options.fetch ?? globalThis.fetch;
    if (typeof fetchImplementation !== "function") {
      throw new TypeError("CrowdRelay requires a fetch implementation");
    }
    this.#baseUrl = baseUrl;
    this.#timeoutMs = timeoutMs;
    this.#fetch = fetchImplementation.bind(globalThis);
  }

  public async listEvents(limit = 50): Promise<PublicEvent[]> {
    const response = await this.#request<{ events: PublicEvent[] }>(`public/events?limit=${limit}`);
    return response.events;
  }

  public getEvent(slug: string, campaignId?: string): Promise<PublicEvent> {
    return this.#request(`public/events/${encodeURIComponent(slug)}${campaignQuery(campaignId)}`);
  }

  public getTicketSale(slug: string): Promise<TicketSaleOffer> {
    return this.#request(`public/events/${encodeURIComponent(slug)}/tickets`);
  }

  public getAdminTicketingOverview(
    slug: string,
    adminApiKey: string,
  ): Promise<AdminTicketingOverview> {
    return this.#request(`admin/events/${encodeURIComponent(slug)}/ticketing`, {
      bearerToken: adminApiKey,
    });
  }

  public getStaffTicketingOverview(
    slug: string,
    staffApiKey: string,
  ): Promise<AdminTicketingOverview> {
    return this.#request(`staff/events/${encodeURIComponent(slug)}/ticketing`, {
      bearerToken: staffApiKey,
    });
  }

  public configureTicketSale(
    slug: string,
    input: ConfigureTicketSaleInput,
    adminApiKey: string,
    idempotencyKey = crypto.randomUUID(),
  ): Promise<TicketSaleOffer> {
    return this.#request(`admin/events/${encodeURIComponent(slug)}/ticketing`, {
      method: "POST",
      body: input,
      idempotencyKey,
      bearerToken: adminApiKey,
    });
  }

  public async listCities(limit = 100): Promise<CitySignal[]> {
    const response = await this.#request<{ items: CitySignal[] }>(`public/cities?limit=${limit}`);
    return response.items;
  }

  public signupFan(input: FanSignupInput, idempotencyKey = crypto.randomUUID()): Promise<FanSignupResult> {
    return this.#request("fans", {
      method: "POST",
      body: input,
      idempotencyKey,
    });
  }


  public confirmFan(
    token: string,
    idempotencyKey = crypto.randomUUID(),
  ): Promise<FanConfirmationResult> {
    return this.#request("fans/confirm", {
      method: "POST",
      body: { token },
      idempotencyKey,
    });
  }

  public unsubscribeFan(token: string): Promise<FanUnsubscribeResult> {
    return this.#request("fans/unsubscribe", { method: "POST", body: { token } });
  }

  public issueAdmissionPass(
    input: { event_slug: string; pool_slug: string; fan_email: string; claim_expires_hours?: number },
    adminApiKey: string,
    idempotencyKey = crypto.randomUUID(),
  ): Promise<AdmissionPassIssued> {
    return this.#request("admin/admission/passes", {
      method: "POST",
      body: input,
      idempotencyKey,
      bearerToken: adminApiKey,
    });
  }

  public claimAdmissionPass(token: string, idempotencyKey = crypto.randomUUID()): Promise<AdmissionPass> {
    return this.#request("passes/claim", {
      method: "POST",
      body: { token },
      idempotencyKey,
    });
  }

  public getMyAdmissionPass(): Promise<AdmissionPass> {
    return this.#request("me/pass");
  }

  public getAdmissionQr(): Promise<AdmissionQr> {
    return this.#request("me/pass/qr");
  }

  public redeemAdmissionPass(
    input: { event_slug: string; qr_token?: string; public_reference?: string },
    staffApiKey: string,
    idempotencyKey = crypto.randomUUID(),
  ): Promise<AdmissionRedemptionResult> {
    return this.#request("staff/admission/redeem", {
      method: "POST",
      body: input,
      idempotencyKey,
      bearerToken: staffApiKey,
    });
  }

  public revokeAdmissionPass(
    publicReference: string,
    adminApiKey: string,
    idempotencyKey = crypto.randomUUID(),
  ): Promise<AdmissionPass> {
    return this.#request(`admin/admission/passes/${encodeURIComponent(publicReference)}/revoke`, {
      method: "POST",
      idempotencyKey,
      bearerToken: adminApiKey,
    });
  }

  public registerEventInterest(
    slug: string,
    input: EventInterestInput = {},
    idempotencyKey = crypto.randomUUID(),
  ): Promise<EventInterestResult> {
    return this.#request(`events/${encodeURIComponent(slug)}/interest`, {
      method: "POST",
      body: input,
      idempotencyKey,
    });
  }

  public checkInToEvent(
    slug: string,
    token: string,
    idempotencyKey = crypto.randomUUID(),
  ): Promise<ConcertCheckinResult> {
    return this.#request(`events/${encodeURIComponent(slug)}/check-in`, {
      method: "POST",
      body: { token },
      idempotencyKey,
    });
  }

  public getConcertQrOverview(
    adminApiKey: string,
  ): Promise<ConcertQrOverview> {
    return this.#request("admin/event-qr/overview", {
      bearerToken: adminApiKey,
    });
  }

  public createConcertQrCampaign(
    input: CreateConcertQrCampaignInput,
    adminApiKey: string,
    idempotencyKey = crypto.randomUUID(),
  ): Promise<ConcertQrCampaign> {
    return this.#request("admin/event-qr/campaigns", {
      method: "POST",
      body: input,
      idempotencyKey,
      bearerToken: adminApiKey,
    });
  }

  public async listConcertQrCampaigns(
    adminApiKey: string,
    limit = 50,
  ): Promise<ConcertQrCampaign[]> {
    const response = await this.#request<{ campaigns: ConcertQrCampaign[] }>(
      `admin/event-qr/campaigns?limit=${limit}`,
      { bearerToken: adminApiKey },
    );
    return response.campaigns;
  }

  public async revokeConcertQrCampaign(
    campaignId: string,
    adminApiKey: string,
    idempotencyKey = crypto.randomUUID(),
  ): Promise<void> {
    await this.#request<void>(
      `admin/event-qr/campaigns/${encodeURIComponent(campaignId)}/revoke`,
      {
        method: "POST",
        idempotencyKey,
        bearerToken: adminApiKey,
        expectEmpty: true,
      },
    );
  }

  public listMyEvents(limit = 50): Promise<FanEventInterest[]> {
    return this.#request(`me/events?limit=${limit}`);
  }

  public getReferralProgress(): Promise<ReferralProgress> {
    return this.#request("me/referral");
  }

  public async trackView(slug: string, campaignId?: string): Promise<void> {
    await this.#request<void>(`public/events/${encodeURIComponent(slug)}/view${campaignQuery(campaignId)}`, {
      method: "POST",
      expectEmpty: true,
    });
  }

  public async trackShare(slug: string, campaignId?: string): Promise<void> {
    await this.#request<void>(`public/events/${encodeURIComponent(slug)}/share${campaignQuery(campaignId)}`, {
      method: "POST",
      expectEmpty: true,
    });
  }

  public eventTicketUrl(slug: string, campaignId?: string): string {
    return this.#url(`public/events/${encodeURIComponent(slug)}/ticket${campaignQuery(campaignId)}`).toString();
  }

  public eventListenUrl(slug: string, campaignId?: string): string {
    return this.#url(`public/events/${encodeURIComponent(slug)}/listen${campaignQuery(campaignId)}`).toString();
  }

  public eventCalendarUrl(slug: string, campaignId?: string): string {
    return this.#url(`public/events/${encodeURIComponent(slug)}/calendar.ics${campaignQuery(campaignId)}`).toString();
  }

  public smartLinkUrl(slug: string): string {
    return this.#url(`go/${encodeURIComponent(slug)}`).toString();
  }

  async #request<T>(
    path: string,
    options: {
      method?: "GET" | "POST";
      body?: unknown;
      idempotencyKey?: string;
      expectEmpty?: boolean;
      bearerToken?: string;
    } = {},
  ): Promise<T> {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), this.#timeoutMs);
    try {
      const headers = new Headers({ Accept: "application/json" });
      if (options.body !== undefined) headers.set("Content-Type", "application/json");
      if (options.idempotencyKey !== undefined) headers.set("Idempotency-Key", options.idempotencyKey);
      if (options.bearerToken !== undefined) headers.set("Authorization", `Bearer ${options.bearerToken}`);

      const request: RequestInit = {
        method: options.method ?? "GET",
        headers,
        credentials: "include",
        signal: controller.signal,
      };
      if (options.body !== undefined) request.body = JSON.stringify(options.body);

      const response = await this.#fetch(this.#url(path), request);
      if (!response.ok) throw await toError(response);
      if (options.expectEmpty || response.status === 204) return undefined as T;
      try {
        return (await response.json()) as T;
      } catch {
        throw new CrowdRelayError(response.status, "CrowdRelay returned invalid JSON");
      }
    } catch (error) {
      if (error instanceof CrowdRelayError) throw error;
      if (error instanceof Error && error.name === "AbortError") {
        throw new CrowdRelayError(0, "CrowdRelay request timed out");
      }
      throw new CrowdRelayError(0, error instanceof Error ? error.message : "CrowdRelay request failed");
    } finally {
      clearTimeout(timeout);
    }
  }

  #url(path: string): URL {
    return new URL(path.replace(/^\//, ""), this.#baseUrl);
  }
}

async function toError(response: Response): Promise<CrowdRelayError> {
  const contentType = response.headers.get("content-type") ?? "";
  if (contentType.includes("application/problem+json")) {
    try {
      const problem = (await response.json()) as Partial<ProblemDetails>;
      if (
        typeof problem.type === "string"
        && typeof problem.title === "string"
        && typeof problem.status === "number"
      ) {
        return new CrowdRelayError(
          response.status,
          typeof problem.detail === "string" ? problem.detail : problem.title,
          problem as ProblemDetails,
        );
      }
    } catch {
      // Preserve the HTTP status when an upstream proxy returns malformed
      // problem JSON instead of misclassifying the response as a network error.
    }
  }
  return new CrowdRelayError(response.status, `CrowdRelay returned HTTP ${response.status}`);
}

function ensureTrailingSlash(value: string): string {
  return value.endsWith("/") ? value : `${value}/`;
}

function campaignQuery(campaignId?: string): string {
  return campaignId === undefined ? "" : `?campaign_id=${encodeURIComponent(campaignId)}`;
}
