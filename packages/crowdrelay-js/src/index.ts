// @generated-contract openapi-sha256:e8cb4ee07f70239649e781ed3d6eafc0905a65d1ec39ecaac7180247cf61345b
export interface EventCity {
  id: string
  slug: string
  name: string
  country_code: string
  region: string | null
}

export type TicketSaleState =
  | "upcoming"
  | "open"
  | "closed"
  | "sold_out"
  | "inactive"
  | "event_unavailable"

export interface TicketSaleSummary {
  currency: string
  capacity: number
  sold: number
  reserved: number
  available: number
  sales_open_at: string
  sales_close_at: string
  sales_state:
    | "upcoming"
    | "open"
    | "closed"
    | "sold_out"
    | "inactive"
    | "event_unavailable"
  from_price_gross_minor: number | null
  active_ticket_type_count: number
}

export interface PublicEvent {
  id: string
  slug: string
  title: string
  description: string | null
  city: EventCity | null
  venue: string | null
  venue_address: string | null
  timezone: string
  starts_at: string
  doors_at: string | null
  ends_at: string | null
  ticket_url: string | null
  listen_url: string | null
  image_url: string | null
  trailer_url: string | null
  external_event_url: string | null
  updated_at: string
  source?: "crowdrelay" | "bandsintown" | "curated"
  ticket_sale?: TicketSaleSummary | null
}

export interface TicketTypeOffer {
  id: string
  slug: string
  name: string
  description: string | null
  price_gross_minor: number
  capacity: number | null
  sold: number
  reserved: number
  available: number
  sort_order: number
  active: boolean
}

export interface TicketSaleOffer {
  event_id: string
  event_slug: string
  event_title: string
  event_status: string
  venue: string | null
  timezone: string
  starts_at: string
  currency: string
  vat_rate_basis_points: number
  capacity: number
  sold: number
  reserved: number
  available: number
  max_per_order: number
  sales_open_at: string
  sales_close_at: string
  active: boolean
  sales_state:
    | "upcoming"
    | "open"
    | "closed"
    | "sold_out"
    | "inactive"
    | "event_unavailable"
  ticket_types: TicketTypeOffer[]
}


export interface ConfigureTicketTypeInput {
  slug: string
  name: string
  description?: string | null
  price_gross_minor: number
  capacity?: number | null
  sort_order: number
  active: boolean
}

export interface ConfigureTicketSaleInput {
  currency: string
  vat_rate_basis_points: number
  capacity: number
  max_per_order: number
  hold_seconds: number
  sales_open_at: string
  sales_close_at: string
  active: boolean
  ticket_types: ConfigureTicketTypeInput[]
}

export interface TicketOrderSummary {
  order_id: string
  public_reference: string
  status: string
  buyer_email_masked: string
  amount_gross_minor: number
  amount_refunded_minor: number
  currency: string
  paid_at: string | null
}

export interface AdminTicketingOverview {
  sale: TicketSaleOffer
  reserved_orders: number
  checkout_created_orders: number
  reserved_tickets: number
  paid_orders: number
  paid_tickets: number
  gross_sales_minor: number
  refunded_minor: number
  recent_orders: TicketOrderSummary[]
}

export interface TicketOrderItem {
  id: string
  ticket_type_slug: string
  ticket_type_name: string
  quantity: number
  unit_gross_minor: number
  unit_net_minor: number
  unit_vat_minor: number
  total_gross_minor: number
  total_net_minor: number
  total_vat_minor: number
}

export interface IssuedTicket {
  pass_id: string
  order_item_id: string
  sequence: number
  public_reference: string
  status: string
  holder_name: string | null
  holder_email_masked: string
  redeemed_at: string | null
}

export interface TicketOrder {
  order_id: string
  public_reference: string
  event_slug: string
  event_title: string
  venue: string | null
  timezone: string
  starts_at: string
  status: string
  buyer_email_masked: string
  buyer_name: string | null
  currency: string
  amount_gross_minor: number
  amount_net_minor: number
  amount_vat_minor: number
  amount_refunded_minor: number
  vat_rate_basis_points: number
  invoice_requested: boolean
  expires_at: string
  paid_at: string | null
  refunded_at: string | null
  items: TicketOrderItem[]
  tickets: IssuedTicket[]
}

export interface TicketWalletPass {
  pass_id: string
  order_item_id: string
  ticket_type_slug: string
  ticket_type_name: string
  sequence: number
  public_reference: string
  status: string
  holder_name: string | null
  holder_email_masked: string
  redeemed_at: string | null
  qr_token: string | null
  qr_not_before: string
  qr_expires_at: string
}

export interface TicketWallet {
  order: TicketOrder
  tickets: TicketWalletPass[]
}

export interface TicketDeliveryRequestResult {
  accepted: boolean
  duplicate: boolean
  requested_at: string
}

export interface CitySignal {
  slug: string
  name: string
  country_code: string
  fan_count: number
}

export interface ConsentInput {
  marketing: true
  policy_version: string
}

export interface FanSignupInput {
  email: string
  city_slug: string
  display_name?: string
  locale?: string
  referral_code?: string
  campaign_id?: string
  consent: ConsentInput
}

export interface FanSignupResult {
  fan_id: string
  status: "pending" | "active"
  referral_url: string | null
  confirmation_required: boolean
  email_kind?: "confirmation" | "session_recovery" | null
  email_queued?: boolean
  retry_after_seconds?: number | null
}

export interface FanAccessRequestResult {
  accepted: true
}

export interface FanConfirmationResult {
  fan_id: string
  status: "active"
  referral_url: string
}

export interface FanUnsubscribeResult {
  fan_id: string
  status: "unsubscribed" | "suppressed"
}

export interface EventInterestInput {
  campaign_id?: string
  source?: string
}

export interface EventInterestResult {
  event_id: string
  fan_id: string
  created: boolean
  reminder_count: number
}

export interface ConcertCheckinResult {
  event_id: string
  event_slug: string
  campaign_id: string
  created: boolean
  checked_in_at: string
}


export interface CreateConcertQrCampaignInput {
  event_slug: string
  label: string
  valid_from: string
  valid_until: string
  max_checkins?: number
}

export interface StaffConcertEvent {
  id: string
  slug: string
  title: string
  venue: string | null
  starts_at: string
}

export interface ConcertQrCampaign {
  id: string
  event_id: string
  event_slug: string
  event_title: string
  venue: string | null
  starts_at: string
  label: string
  valid_from: string
  valid_until: string
  max_checkins: number | null
  checkin_count: number
  active: boolean
  revoked_at: string | null
  created_at: string
  token: string | null
}

export interface ConcertQrOverview {
  events: StaffConcertEvent[]
  campaigns: ConcertQrCampaign[]
}

export interface FanEventInterest {
  event: PublicEvent
  interested_at: string
}

export interface MerchCoupon {
  id: string
  reward_grant_id: string
  reward_rule_id: string
  code: string
  discount_percent: number
  max_uses: number
  used_count: number
  status: "issued" | "redeemed" | "expired" | "revoked"
  expires_at: string | null
}

export interface WeightedDrawEntry {
  draw_id: string
  slug: string
  name: string
  prize_kind: "admission_pass" | "physical_item"
  closes_at: string
  draw_at: string
  qualified_referrals: number
  base_entries: number
  referral_entries: number
  concert_checkins: number
  checkin_entries: number
  total_entries: number
  max_entries: number
}

export interface PhysicalRewardGrant {
  reward_grant_id: string
  reward_rule_id: string
  item_name: string
  sku: string
  status: "issued" | "fulfilled" | "expired" | "revoked"
  granted_at: string
  expires_at: string | null
}

export interface ReferralProgress {
  referral_code: string
  qualified_referrals: number
  pending_referrals: number
  next_reward_threshold: number | null
  draw_entries: WeightedDrawEntry[]
  coupons: MerchCoupon[]
  physical_rewards: PhysicalRewardGrant[]
}

export type AdmissionPassStatus =
  | "issued"
  | "claimed"
  | "redeemed"
  | "revoked"
  | "expired"

export interface AdmissionPass {
  pass_id: string
  session_id: string | null
  event_id: string
  event_slug: string
  event_title: string
  venue: string | null
  starts_at: string
  holder_name: string | null
  holder_email_masked: string
  public_reference: string
  status: AdmissionPassStatus
  session_expires_at: string
  redeemed_at: string | null
}


export interface AdmissionPassIssued {
  pass_id: string
  event_id: string
  fan_id: string
  public_reference: string
  claim_token: string
  claim_expires_at: string
  created: boolean
}

export type AdmissionRedemptionStatus =
  | "redeemed"
  | "already_redeemed"
  | "revoked"
  | "expired"
  | "not_claimed"

export interface AdmissionRedemptionResult {
  pass_id: string
  event_id: string
  public_reference: string
  holder_name: string | null
  holder_email_masked: string
  status: AdmissionRedemptionStatus
  redeemed_at: string | null
}

export interface AdmissionQr {
  token: string
  expires_at: string
}

export interface FanHomeSnapshot {
  schema_version: number
  generated_at: string
  profile: {
    display_name: string | null
    locale: string | null
    primary_city: string | null
  }
  next_event: null | {
    slug: string
    title: string
    venue: string | null
    city: string | null
    starts_at: string
    doors_at: string | null
    ends_at: string | null
    phase: "upcoming" | "live" | "afterglow" | string
    ticket_url: string | null
    interested: boolean
    has_pass: boolean
    has_paid_ticket: boolean
    ticket_sale_active: boolean
  }
  synesthesia: {
    started: boolean
    completed: boolean
    rooms_completed: number
    client_total_elapsed_ms: number | null
    linked_at: string | null
    reward_entered: boolean
  }
  referral: { qualified: number; pending: number }
  counts: {
    event_interests: number
    active_passes: number
    paid_orders: number
    area_claims: number
  }
  recommended_action:
    | "continue_synesthesia"
    | "open_wallet"
    | "open_live_event"
    | "share_post_show_feedback"
    | "get_ticket"
    | "follow_next_event"
    | "explore_signal"
    | string
}

export interface FanEventContextSnapshot {
  schema_version: number
  slug: string
  title: string
  venue: string | null
  city: string | null
  starts_at: string
  doors_at: string | null
  ends_at: string | null
  phase: "upcoming" | "live" | "afterglow" | string
  ticket_url: string | null
  interested: boolean
  pass_status: string | null
  paid_ticket_quantity: number
  ticket_sale_active: boolean
  recommended_action: string
}

export interface SynesthesiaLinkResult {
  linked: boolean
  run_id: string
  rooms_completed: number
  client_total_elapsed_ms: number
}

export interface ProblemDetails {
  type: string
  title: string
  status: number
  detail?: string
  request_id?: string
}

const MAX_API_RESPONSE_BYTES = 1024 * 1024

async function readBoundedJson<T>(response: Response): Promise<T> {
  const declared = Number(response.headers.get("content-length") ?? "0")
  if (Number.isFinite(declared) && declared > MAX_API_RESPONSE_BYTES) {
    throw new CrowdRelayError(
      0,
      "CrowdRelay response exceeded the safety limit",
    )
  }
  if (!response.body)
    throw new CrowdRelayError(0, "CrowdRelay returned an empty response")
  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let total = 0
  try {
    while (true) {
      const { done, value } = await reader.read()
      if (done) break
      total += value.byteLength
      if (total > MAX_API_RESPONSE_BYTES) {
        await reader.cancel("response too large")
        throw new CrowdRelayError(
          0,
          "CrowdRelay response exceeded the safety limit",
        )
      }
      chunks.push(value)
    }
  } finally {
    reader.releaseLock()
  }
  const bytes = new Uint8Array(total)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  try {
    return JSON.parse(new TextDecoder().decode(bytes)) as T
  } catch {
    throw new CrowdRelayError(0, "CrowdRelay returned invalid JSON")
  }
}

export class CrowdRelayError extends Error {
  readonly status: number
  readonly problem?: ProblemDetails

  constructor(status: number, message: string, problem?: ProblemDetails) {
    super(message)
    this.name = "CrowdRelayError"
    this.status = status
    this.problem = problem
  }
}

interface RequestOptions {
  method?: "GET" | "POST"
  body?: unknown
  idempotencyKey?: string
  expectEmpty?: boolean
  timeoutMs?: number
  bearerToken?: string
}

export interface CrowdRelayClientOptions {
  baseUrl: string
  timeoutMs?: number
  fetch?: typeof globalThis.fetch
}

export class CrowdRelayClient {
  readonly #baseUrl: URL
  readonly #timeoutMs: number
  readonly #fetch: typeof globalThis.fetch

  constructor(options: CrowdRelayClientOptions) {
    const baseUrl = new URL(ensureTrailingSlash(options.baseUrl))
    if (!["http:", "https:"].includes(baseUrl.protocol) || baseUrl.username || baseUrl.password) {
      throw new TypeError("CrowdRelay baseUrl must be an HTTP(S) URL without credentials")
    }
    const timeoutMs = options.timeoutMs ?? 2_500
    if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) {
      throw new RangeError("CrowdRelay timeoutMs must be a positive finite number")
    }
    const fetchImplementation = options.fetch ?? globalThis.fetch
    if (typeof fetchImplementation !== "function") {
      throw new TypeError("CrowdRelay requires a fetch implementation")
    }
    this.#baseUrl = baseUrl
    this.#timeoutMs = timeoutMs
    this.#fetch = fetchImplementation.bind(globalThis)
  }

  async listCities(limit = 100): Promise<CitySignal[]> {
    const response = await this.#request<{ items: CitySignal[] }>(
      `public/cities?limit=${limit}`,
      { timeoutMs: 1_800 },
    )
    return response.items
  }

  async listEvents(limit = 20): Promise<PublicEvent[]> {
    const response = await this.#request<{ events: PublicEvent[] }>(
      `public/events?limit=${limit}`,
      { timeoutMs: 1_800 },
    )
    return response.events
  }

  getEvent(slug: string, campaignId?: string): Promise<PublicEvent> {
    return this.#request(
      `public/events/${encodeURIComponent(slug)}${campaignQuery(campaignId)}`,
      { timeoutMs: 1_800 },
    )
  }

  getTicketSale(slug: string): Promise<TicketSaleOffer> {
    return this.#request(`public/events/${encodeURIComponent(slug)}/tickets`, {
      timeoutMs: 2_500,
    })
  }

  getAdminTicketingOverview(
    slug: string,
    adminApiKey: string,
  ): Promise<AdminTicketingOverview> {
    return this.#request(`admin/events/${encodeURIComponent(slug)}/ticketing`, {
      bearerToken: adminApiKey,
    })
  }

  getStaffTicketingOverview(
    slug: string,
    staffApiKey: string,
  ): Promise<AdminTicketingOverview> {
    return this.#request(`staff/events/${encodeURIComponent(slug)}/ticketing`, {
      bearerToken: staffApiKey,
    })
  }

  configureTicketSale(
    slug: string,
    input: ConfigureTicketSaleInput,
    adminApiKey: string,
    idempotencyKey = newIdempotencyKey(),
  ): Promise<TicketSaleOffer> {
    return this.#request(`admin/events/${encodeURIComponent(slug)}/ticketing`, {
      method: "POST",
      body: input,
      idempotencyKey,
      bearerToken: adminApiKey,
    })
  }

  getTicketOrder(orderId: string, token: string): Promise<TicketOrder> {
    return this.#request(
      `public/ticket-orders/${encodeURIComponent(orderId)}`,
      {
        bearerToken: token,
        timeoutMs: 4_000,
      },
    )
  }

  getTicketWallet(orderId: string, token: string): Promise<TicketWallet> {
    return this.#request(
      `public/ticket-orders/${encodeURIComponent(orderId)}/wallet`,
      { bearerToken: token, timeoutMs: 5_000 },
    )
  }

  requestTicketDelivery(
    orderId: string,
    token: string,
    idempotencyKey = newIdempotencyKey(),
  ): Promise<TicketDeliveryRequestResult> {
    return this.#request(
      `public/ticket-orders/${encodeURIComponent(orderId)}/delivery-requests`,
      { method: "POST", bearerToken: token, idempotencyKey, timeoutMs: 7_000 },
    )
  }

  signupFan(
    input: FanSignupInput,
    idempotencyKey = newIdempotencyKey(),
  ): Promise<FanSignupResult> {
    return this.#request("fans", {
      method: "POST",
      body: input,
      idempotencyKey,
      timeoutMs: 5_000,
    })
  }

  requestFanAccess(
    email: string,
    locale = "pl",
    idempotencyKey = newIdempotencyKey(),
  ): Promise<FanAccessRequestResult> {
    return this.#request("fans/access", {
      method: "POST",
      body: { email, locale },
      idempotencyKey,
      timeoutMs: 5_000,
    })
  }

  confirmFan(
    token: string,
    idempotencyKey = newIdempotencyKey(),
  ): Promise<FanConfirmationResult> {
    return this.#request("fans/confirm", {
      method: "POST",
      body: { token },
      idempotencyKey,
      timeoutMs: 5_000,
    })
  }

  unsubscribeFan(token: string): Promise<FanUnsubscribeResult> {
    return this.#request("fans/unsubscribe", {
      method: "POST",
      body: { token },
      timeoutMs: 5_000,
    })
  }

  registerEventInterest(
    slug: string,
    input: EventInterestInput = {},
    idempotencyKey = newIdempotencyKey(),
  ): Promise<EventInterestResult> {
    return this.#request(`events/${encodeURIComponent(slug)}/interest`, {
      method: "POST",
      body: input,
      idempotencyKey,
    })
  }

  checkInToEvent(
    slug: string,
    token: string,
    idempotencyKey = newIdempotencyKey(),
  ): Promise<ConcertCheckinResult> {
    return this.#request(`events/${encodeURIComponent(slug)}/check-in`, {
      method: "POST",
      body: { token },
      idempotencyKey,
      timeoutMs: 5_000,
    })
  }

  getConcertQrOverview(adminApiKey: string): Promise<ConcertQrOverview> {
    return this.#request("admin/event-qr/overview", { bearerToken: adminApiKey })
  }

  createConcertQrCampaign(
    input: CreateConcertQrCampaignInput,
    adminApiKey: string,
    idempotencyKey = newIdempotencyKey(),
  ): Promise<ConcertQrCampaign> {
    return this.#request("admin/event-qr/campaigns", {
      method: "POST", body: input, idempotencyKey, bearerToken: adminApiKey,
    })
  }

  async listConcertQrCampaigns(adminApiKey: string, limit = 50): Promise<ConcertQrCampaign[]> {
    const response = await this.#request<{ campaigns: ConcertQrCampaign[] }>(
      `admin/event-qr/campaigns?limit=${limit}`,
      { bearerToken: adminApiKey },
    )
    return response.campaigns
  }

  async revokeConcertQrCampaign(
    campaignId: string,
    adminApiKey: string,
    idempotencyKey = newIdempotencyKey(),
  ): Promise<void> {
    await this.#request<void>(`admin/event-qr/campaigns/${encodeURIComponent(campaignId)}/revoke`, {
      method: "POST", idempotencyKey, bearerToken: adminApiKey, expectEmpty: true,
    })
  }

  listMyEvents(limit = 50): Promise<FanEventInterest[]> {
    return this.#request(`me/events?limit=${limit}`)
  }

  getReferralProgress(): Promise<ReferralProgress> {
    return this.#request("me/referral")
  }

  getFanHome(): Promise<FanHomeSnapshot> {
    return this.#request("me/home", { timeoutMs: 3_000 })
  }

  getFanEventContext(slug: string): Promise<FanEventContextSnapshot> {
    return this.#request(`me/events/${encodeURIComponent(slug)}/context`, {
      timeoutMs: 3_000,
    })
  }

  linkSynesthesiaCompletion(
    handoffCode: string,
  ): Promise<SynesthesiaLinkResult> {
    return this.#request("me/synesthesia/link", {
      method: "POST",
      body: { handoff_code: handoffCode },
      idempotencyKey: `synesthesia-link-${handoffCode.slice(0, 16)}`,
      timeoutMs: 5_000,
    })
  }

  issueAdmissionPass(
    input: { event_slug: string; pool_slug: string; fan_email: string; claim_expires_hours?: number },
    adminApiKey: string,
    idempotencyKey = newIdempotencyKey(),
  ): Promise<AdmissionPassIssued> {
    return this.#request("admin/admission/passes", {
      method: "POST", body: input, idempotencyKey, bearerToken: adminApiKey,
    })
  }

  claimAdmissionPass(token: string): Promise<AdmissionPass> {
    return this.#request("passes/claim", {
      method: "POST",
      body: { token },
      timeoutMs: 5_000,
    })
  }

  getMyAdmissionPass(): Promise<AdmissionPass> {
    return this.#request("me/pass")
  }

  getAdmissionQr(): Promise<AdmissionQr> {
    return this.#request("me/pass/qr")
  }

  redeemAdmissionPass(
    input: { event_slug: string; qr_token?: string; public_reference?: string },
    staffApiKey: string,
    idempotencyKey = newIdempotencyKey(),
  ): Promise<AdmissionRedemptionResult> {
    return this.#request("staff/admission/redeem", {
      method: "POST", body: input, idempotencyKey, bearerToken: staffApiKey,
    })
  }

  revokeAdmissionPass(
    publicReference: string,
    adminApiKey: string,
    idempotencyKey = newIdempotencyKey(),
  ): Promise<AdmissionPass> {
    return this.#request(`admin/admission/passes/${encodeURIComponent(publicReference)}/revoke`, {
      method: "POST", idempotencyKey, bearerToken: adminApiKey,
    })
  }

  async trackView(slug: string, campaignId?: string): Promise<void> {
    await this.#request<void>(
      `public/events/${encodeURIComponent(slug)}/view${campaignQuery(campaignId)}`,
      { method: "POST", expectEmpty: true, timeoutMs: 1_200 },
    )
  }

  async trackShare(slug: string, campaignId?: string): Promise<void> {
    await this.#request<void>(
      `public/events/${encodeURIComponent(slug)}/share${campaignQuery(campaignId)}`,
      { method: "POST", expectEmpty: true, timeoutMs: 1_200 },
    )
  }

  eventTicketUrl(slug: string, campaignId?: string): string {
    return this.#url(
      `public/events/${encodeURIComponent(slug)}/ticket${campaignQuery(campaignId)}`,
    ).toString()
  }

  eventListenUrl(slug: string, campaignId?: string): string {
    return this.#url(
      `public/events/${encodeURIComponent(slug)}/listen${campaignQuery(campaignId)}`,
    ).toString()
  }

  eventCalendarUrl(slug: string, campaignId?: string): string {
    return this.#url(
      `public/events/${encodeURIComponent(slug)}/calendar.ics${campaignQuery(campaignId)}`,
    ).toString()
  }

  smartLinkUrl(slug: string): string {
    return this.#url(`go/${encodeURIComponent(slug)}`).toString()
  }

  async #request<T>(path: string, options: RequestOptions = {}): Promise<T> {
    const method = options.method ?? "GET"
    const retryDelays = method === "GET" ? [0, 180, 600] : [0]
    let lastError: CrowdRelayError | null = null

    for (let attempt = 0; attempt < retryDelays.length; attempt += 1) {
      const delay = retryDelays[attempt] ?? 0
      if (delay > 0) await sleep(delay)

      try {
        return await this.#requestOnce<T>(path, { ...options, method })
      } catch (error) {
        const normalized = normalizeRequestError(error)
        lastError = normalized
        const hasAnotherAttempt = attempt + 1 < retryDelays.length
        if (!hasAnotherAttempt || !isRetryableReadFailure(normalized)) {
          throw normalized
        }
      }
    }

    throw lastError ?? new CrowdRelayError(0, "CrowdRelay request failed")
  }

  async #requestOnce<T>(path: string, options: RequestOptions): Promise<T> {
    const controller = new AbortController()
    const timeout = globalThis.setTimeout(
      () => controller.abort(),
      options.timeoutMs ?? this.#timeoutMs,
    )

    try {
      const headers = new Headers({ Accept: "application/json" })
      if (options.body !== undefined) {
        headers.set("Content-Type", "application/json")
      }
      if (options.idempotencyKey) {
        headers.set("Idempotency-Key", options.idempotencyKey)
      }
      if (options.bearerToken) {
        headers.set("Authorization", `Bearer ${options.bearerToken}`)
      }

      const response = await this.#fetch(this.#url(path), {
        method: options.method ?? "GET",
        headers,
        credentials: "include",
        signal: controller.signal,
        body:
          options.body === undefined ? undefined : JSON.stringify(options.body),
      })

      if (!response.ok) throw await toError(response)
      if (options.expectEmpty || response.status === 204) return undefined as T
      return await readBoundedJson<T>(response)
    } finally {
      globalThis.clearTimeout(timeout)
    }
  }

  #url(path: string): URL {
    return new URL(path.replace(/^\//, ""), this.#baseUrl)
  }
}

const RETRYABLE_READ_STATUSES = new Set([0, 408, 425, 429, 500, 502, 503, 504])

const sleep = (delayMs: number) =>
  new Promise<void>(resolve => globalThis.setTimeout(resolve, delayMs))

function normalizeRequestError(error: unknown): CrowdRelayError {
  if (error instanceof CrowdRelayError) return error
  if (
    (typeof DOMException !== "undefined" &&
      error instanceof DOMException &&
      error.name === "AbortError") ||
    (error instanceof Error && error.name === "AbortError")
  ) {
    return new CrowdRelayError(0, "CrowdRelay request timed out")
  }
  return new CrowdRelayError(
    0,
    error instanceof Error ? error.message : "CrowdRelay request failed",
  )
}

function isRetryableReadFailure(error: CrowdRelayError): boolean {
  return RETRYABLE_READ_STATUSES.has(error.status)
}

async function toError(response: Response): Promise<CrowdRelayError> {
  const contentType = response.headers.get("content-type") ?? ""
  if (contentType.includes("application/problem+json")) {
    const problem = await readBoundedJson<ProblemDetails>(response)
    return new CrowdRelayError(
      response.status,
      problem.detail ?? problem.title,
      problem,
    )
  }
  return new CrowdRelayError(
    response.status,
    `CrowdRelay returned HTTP ${response.status}`,
  )
}

function ensureTrailingSlash(value: string): string {
  return value.endsWith("/") ? value : `${value}/`
}

function campaignQuery(campaignId?: string): string {
  return campaignId ? `?campaign_id=${encodeURIComponent(campaignId)}` : ""
}

function newIdempotencyKey(): string {
  return typeof crypto.randomUUID === "function"
    ? crypto.randomUUID()
    : `virya-${Date.now()}-${Math.random().toString(36).slice(2)}`
}
