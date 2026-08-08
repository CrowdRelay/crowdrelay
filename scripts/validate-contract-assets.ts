#!/usr/bin/env -S node --experimental-strip-types

import { readFileSync, readdirSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const HTTP_METHODS = new Set(["get", "put", "post", "delete", "options", "head", "patch", "trace"]);
const REQUIRED_PATHS = [
  "/fans",
  "/public/cities",
  "/public/events",
  "/public/events/{slug}",
  "/events/{slug}/interest",
  "/events/{slug}/check-in",
  "/admin/event-qr/overview",
  "/admin/audience/overview",
  "/admin/audience/fans",
  "/admin/audience/segments",
  "/admin/audience/segments/{slug}/preview",
  "/admin/communications/campaigns",
  "/admin/communications/campaigns/{campaign_id}/schedule",
  "/admin/communications/campaigns/{campaign_id}/cancel",
  "/internal/communications/campaigns/{campaign_id}/delivery-plan",
  "/internal/communications/campaigns/{campaign_id}/complete",
  "/admin/analytics/funnel",
  "/admin/analytics/revenue",
  "/admin/event-qr/campaigns",
  "/admin/event-qr/campaigns/{campaign_id}/revoke",
  "/staff/events/{slug}/ticketing",
  "/staff/coupons/redeem",
  "/staff/event-qr/overview",
  "/staff/event-qr/campaigns",
  "/staff/event-qr/campaigns/{campaign_id}/revoke",
  "/staff/admission/redeem",
  "/me/events",
  "/me/referral",
] as const;

interface MappingEntry {
  readonly key: string;
  readonly value: string;
  readonly indent: number;
  readonly line: number;
  readonly path: readonly string[];
}

interface BootstrapCity {
  readonly slug?: unknown;
}

interface BootstrapSmartLink {
  readonly slug?: unknown;
  readonly destination_url?: unknown;
}

interface BootstrapCampaign {
  readonly smart_links?: unknown;
}

interface BootstrapEvent {
  readonly slug?: unknown;
  readonly city_slug?: unknown;
  readonly starts_at?: unknown;
  readonly ticket_url?: unknown;
  readonly listen_url?: unknown;
  readonly image_url?: unknown;
  readonly trailer_url?: unknown;
  readonly external_event_url?: unknown;
}

interface BootstrapPool {
  readonly event_slug?: unknown;
  readonly slug?: unknown;
  readonly capacity?: unknown;
}

interface BootstrapDocument {
  readonly cities?: unknown;
  readonly campaigns?: unknown;
  readonly events?: unknown;
  readonly admission_pools?: unknown;
}

function failValidation(message: string): never {
  console.error(`contract validation failed: ${message}`);
  process.exit(1);
}

function readText(relativePath: string): string {
  try {
    return readFileSync(join(ROOT, relativePath), "utf8");
  } catch (error) {
    failValidation(`${relativePath}: ${errorMessage(error)}`);
  }
}

function parseJson<T>(relativePath: string): T {
  try {
    return JSON.parse(readText(relativePath)) as T;
  } catch (error) {
    failValidation(`${relativePath}: invalid JSON: ${errorMessage(error)}`);
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function stripYamlComment(line: string): string {
  let quote: "'" | '"' | null = null;
  let escaped = false;

  for (let index = 0; index < line.length; index += 1) {
    const character = line[index];

    if (quote === '"' && escaped) {
      escaped = false;
      continue;
    }
    if (quote === '"' && character === "\\") {
      escaped = true;
      continue;
    }
    if (quote !== null) {
      if (character === quote) quote = null;
      continue;
    }
    if (character === "'" || character === '"') {
      quote = character;
      continue;
    }
    if (character === "#" && (index === 0 || /\s/.test(line[index - 1] ?? ""))) {
      return line.slice(0, index).trimEnd();
    }
  }

  return line.trimEnd();
}

function findMappingColon(content: string): number {
  let quote: "'" | '"' | null = null;
  let escaped = false;
  let squareDepth = 0;
  let curlyDepth = 0;

  for (let index = 0; index < content.length; index += 1) {
    const character = content[index];

    if (quote === '"' && escaped) {
      escaped = false;
      continue;
    }
    if (quote === '"' && character === "\\") {
      escaped = true;
      continue;
    }
    if (quote !== null) {
      if (character === quote) quote = null;
      continue;
    }
    if (character === "'" || character === '"') {
      quote = character;
      continue;
    }
    if (character === "[") squareDepth += 1;
    if (character === "]") squareDepth -= 1;
    if (character === "{") curlyDepth += 1;
    if (character === "}") curlyDepth -= 1;
    if (character === ":" && squareDepth === 0 && curlyDepth === 0) return index;
  }

  return -1;
}

function parseYamlScalar(rawValue: string): string {
  const value = rawValue.trim();
  if (value.length >= 2 && value.startsWith('"') && value.endsWith('"')) {
    try {
      return JSON.parse(value) as string;
    } catch (error) {
      failValidation(`invalid double-quoted YAML scalar ${value}: ${errorMessage(error)}`);
    }
  }
  if (value.length >= 2 && value.startsWith("'") && value.endsWith("'")) {
    return value.slice(1, -1).replaceAll("''", "'");
  }
  return value;
}

function parseYamlMappings(source: string): MappingEntry[] {
  const mappings: MappingEntry[] = [];
  const stack: MappingEntry[] = [];

  source.split(/\r?\n/u).forEach((rawLine, zeroBasedLine) => {
    if (/^\s*\t/u.test(rawLine)) {
      failValidation(`openapi/openapi.yaml:${zeroBasedLine + 1}: tabs are not allowed in indentation`);
    }

    const withoutComment = stripYamlComment(rawLine);
    if (withoutComment.trim() === "" || /^\s*(?:---|\.\.\.)\s*$/u.test(withoutComment)) return;

    const leadingWhitespace = withoutComment.match(/^ */u)?.[0].length ?? 0;
    let content = withoutComment.slice(leadingWhitespace);
    let indent = leadingWhitespace;

    if (content.startsWith("- ")) {
      content = content.slice(2);
      indent += 2;
    }

    const colon = findMappingColon(content);
    if (colon <= 0) return;

    const key = parseYamlScalar(content.slice(0, colon));
    const value = content.slice(colon + 1).trim();
    if (key === "") return;

    while (stack.length > 0 && stack[stack.length - 1]!.indent >= indent) stack.pop();

    const entry: MappingEntry = {
      key,
      value,
      indent,
      line: zeroBasedLine + 1,
      path: [...stack.map((item) => item.key), key],
    };
    mappings.push(entry);

    if (value === "" || /^&[A-Za-z0-9_-]+$/u.test(value)) stack.push(entry);
  });

  return mappings;
}

function pointerFor(path: readonly string[]): string {
  return `#/${path.map((segment) => segment.replaceAll("~", "~0").replaceAll("/", "~1")).join("/")}`;
}

function validateOpenApi(): number {
  const relativePath = "openapi/openapi.yaml";
  const mappings = parseYamlMappings(readText(relativePath));
  const openapi = mappings.find((entry) => entry.path.length === 1 && entry.key === "openapi");
  const version = openapi ? parseYamlScalar(openapi.value) : "";
  if (!/^3\.\d+(?:\.\d+)?$/u.test(version)) {
    failValidation(`${relativePath}: expected an OpenAPI 3 document, got ${version || "no version"}`);
  }

  const pointers = new Set(mappings.map((entry) => pointerFor(entry.path)));
  for (const entry of mappings.filter((candidate) => candidate.key === "$ref")) {
    const reference = parseYamlScalar(entry.value);
    if (!reference.startsWith("#/")) {
      failValidation(`${relativePath}:${entry.line}: unsupported external OpenAPI ref: ${reference}`);
    }
    if (!pointers.has(reference)) {
      failValidation(`${relativePath}:${entry.line}: missing OpenAPI ref: ${reference}`);
    }
  }

  const requiredSchemaPointers = [
    "#/components/schemas/ReferralProgress/properties/draw_entries",
    "#/components/schemas/WeightedDrawEntry",
    "#/components/schemas/ReferralProgress/properties/physical_rewards",
    "#/components/schemas/PhysicalRewardGrant",
    "#/components/schemas/WeightedDrawEntry/properties/concert_checkins",
    "#/components/schemas/WeightedDrawEntry/properties/checkin_entries",
    "#/components/schemas/ConcertCheckinResult",
    "#/components/schemas/CreateConcertQrCampaignRequest",
    "#/components/schemas/ConcertQrCampaign",
    "#/components/schemas/ConcertQrOverview",
    "#/components/schemas/StaffConcertEvent",
  ];
  const missingSchemas = requiredSchemaPointers.filter((pointer) => !pointers.has(pointer));
  if (missingSchemas.length > 0) {
    failValidation(`${relativePath}: missing required reward contract: ${missingSchemas.join(", ")}`);
  }

  const pathEntries = mappings.filter(
    (entry) => entry.path.length === 2 && entry.path[0] === "paths" && entry.key.startsWith("/"),
  );
  const actualPaths = new Set(pathEntries.map((entry) => entry.key));
  const missingPaths = REQUIRED_PATHS.filter((path) => !actualPaths.has(path));
  if (missingPaths.length > 0) {
    failValidation(`${relativePath}: missing required API paths: ${missingPaths.sort().join(", ")}`);
  }

  const operations = mappings.filter(
    (entry) => entry.path.length === 3 && entry.path[0] === "paths" && HTTP_METHODS.has(entry.key),
  );
  const pathsWithOperations = new Set(operations.map((entry) => entry.path[1]));
  const pathsWithoutOperations = [...actualPaths].filter((path) => !pathsWithOperations.has(path));
  if (pathsWithoutOperations.length > 0) {
    failValidation(`${relativePath}: paths without HTTP operations: ${pathsWithoutOperations.sort().join(", ")}`);
  }

  const operationIds = new Map<string, number>();
  for (const entry of mappings.filter((candidate) => candidate.key === "operationId")) {
    const operationId = parseYamlScalar(entry.value);
    if (operationId === "") failValidation(`${relativePath}:${entry.line}: empty operationId`);
    const previousLine = operationIds.get(operationId);
    if (previousLine !== undefined) {
      failValidation(`${relativePath}:${entry.line}: duplicate operationId ${operationId}; first used at line ${previousLine}`);
    }
    operationIds.set(operationId, entry.line);
  }

  return actualPaths.size;
}

function requireArray<T>(value: unknown, field: string): T[] {
  if (!Array.isArray(value)) failValidation(`deploy/bootstrap.example.json: ${field} must be an array`);
  return value as T[];
}

function requireNonEmptyString(value: unknown, field: string): string {
  if (typeof value !== "string" || value.trim() === "") {
    failValidation(`deploy/bootstrap.example.json: ${field} must be a non-empty string`);
  }
  return value;
}

function assertUnique(values: readonly string[], label: string): void {
  const seen = new Set<string>();
  const duplicates = new Set<string>();
  for (const value of values) {
    if (seen.has(value)) duplicates.add(value);
    seen.add(value);
  }
  if (duplicates.size > 0) failValidation(`${label} contains duplicate values: ${[...duplicates].sort().join(", ")}`);
}

function validateOptionalUrl(value: unknown, field: string): void {
  if (value === null || value === undefined) return;
  const raw = requireNonEmptyString(value, field);
  try {
    const url = new URL(raw);
    if (url.protocol !== "https:" && url.protocol !== "http:") throw new Error("unsupported protocol");
  } catch (error) {
    failValidation(`deploy/bootstrap.example.json: ${field} is not a valid HTTP(S) URL: ${errorMessage(error)}`);
  }
}

function validateBootstrap(): void {
  const path = "deploy/bootstrap.example.json";
  const bootstrap = parseJson<BootstrapDocument>(path);
  const cities = requireArray<BootstrapCity>(bootstrap.cities, "cities");
  const campaigns = requireArray<BootstrapCampaign>(bootstrap.campaigns, "campaigns");
  const events = requireArray<BootstrapEvent>(bootstrap.events, "events");
  const pools = requireArray<BootstrapPool>(bootstrap.admission_pools, "admission_pools");

  const citySlugs = cities.map((city, index) => requireNonEmptyString(city.slug, `cities[${index}].slug`));
  const eventSlugs = events.map((event, index) => requireNonEmptyString(event.slug, `events[${index}].slug`));
  const poolKeys = pools.map((pool, index) => {
    const eventSlug = requireNonEmptyString(pool.event_slug, `admission_pools[${index}].event_slug`);
    const poolSlug = requireNonEmptyString(pool.slug, `admission_pools[${index}].slug`);
    return `${eventSlug}/${poolSlug}`;
  });

  assertUnique(citySlugs, "city slugs");
  assertUnique(eventSlugs, "event slugs");
  assertUnique(poolKeys, "admission pool event/slug pairs");

  const citySet = new Set(citySlugs);
  const eventSet = new Set(eventSlugs);

  events.forEach((event, index) => {
    const citySlug = requireNonEmptyString(event.city_slug, `events[${index}].city_slug`);
    if (!citySet.has(citySlug)) failValidation(`${path}: events[${index}] references unknown city ${citySlug}`);

    const startsAt = requireNonEmptyString(event.starts_at, `events[${index}].starts_at`);
    if (Number.isNaN(Date.parse(startsAt))) failValidation(`${path}: events[${index}].starts_at is not RFC 3339-compatible`);

    validateOptionalUrl(event.ticket_url, `events[${index}].ticket_url`);
    validateOptionalUrl(event.listen_url, `events[${index}].listen_url`);
    validateOptionalUrl(event.image_url, `events[${index}].image_url`);
    validateOptionalUrl(event.trailer_url, `events[${index}].trailer_url`);
    validateOptionalUrl(event.external_event_url, `events[${index}].external_event_url`);
  });

  pools.forEach((pool, index) => {
    const eventSlug = requireNonEmptyString(pool.event_slug, `admission_pools[${index}].event_slug`);
    if (!eventSet.has(eventSlug)) failValidation(`${path}: admission_pools[${index}] references unknown event ${eventSlug}`);
    if (!Number.isSafeInteger(pool.capacity) || Number(pool.capacity) <= 0) {
      failValidation(`${path}: admission_pools[${index}].capacity must be a positive integer`);
    }
  });

  const smartLinkSlugs: string[] = [];
  campaigns.forEach((campaign, campaignIndex) => {
    const links = requireArray<BootstrapSmartLink>(campaign.smart_links, `campaigns[${campaignIndex}].smart_links`);
    links.forEach((link, linkIndex) => {
      smartLinkSlugs.push(requireNonEmptyString(link.slug, `campaigns[${campaignIndex}].smart_links[${linkIndex}].slug`));
      validateOptionalUrl(link.destination_url, `campaigns[${campaignIndex}].smart_links[${linkIndex}].destination_url`);
    });
  });
  assertUnique(smartLinkSlugs, "smart-link slugs");
}

function validateJsonAssets(): number {
  const jsonFiles = [
    ...readdirSync(join(ROOT, "n8n"))
      .filter((name) => name.endsWith(".json"))
      .sort()
      .map((name) => join("n8n", name)),
    "deploy/bootstrap.example.json",
    "deploy/webhook-secrets.example.json",
    "integration/virya/package-additions.json",
    "packages/crowdrelay-js/package.json",
    "packages/crowdrelay-js/tsconfig.json",
  ];

  for (const path of jsonFiles) parseJson<unknown>(path);
  return jsonFiles.length;
}

function validateN8nWorkflows(): void {
  const directory = join(ROOT, "n8n");
  for (const name of readdirSync(directory).filter((entry) => entry.endsWith(".json")).sort()) {
    const relativePath = join("n8n", name);
    const workflow = parseJson<Record<string, unknown>>(relativePath);
    requireNonEmptyString(workflow.name, `${relativePath}.name`);
    if (!Array.isArray(workflow.nodes)) failValidation(`${relativePath}: nodes must be an array`);
    if (typeof workflow.connections !== "object" || workflow.connections === null || Array.isArray(workflow.connections)) {
      failValidation(`${relativePath}: connections must be an object`);
    }
    if (workflow.active !== false) failValidation(`${relativePath}: exported workflow must remain inactive`);
  }
}

function validateClientMirror(): void {
  const packageClient = readText("packages/crowdrelay-js/src/index.ts").replaceAll("\r\n", "\n");
  const viryaClient = readText("integration/virya/src/lib/crowdrelay-client.ts").replaceAll("\r\n", "\n");
  if (packageClient !== viryaClient) {
    failValidation(
      "integration/virya/src/lib/crowdrelay-client.ts differs from packages/crowdrelay-js/src/index.ts",
    );
  }
}

const openApiPathCount = validateOpenApi();
const jsonAssetCount = validateJsonAssets();
validateBootstrap();
validateN8nWorkflows();
validateClientMirror();

console.log(
  `validated OpenAPI (${openApiPathCount} paths), ${jsonAssetCount} JSON assets, bootstrap relations, n8n exports, and the Virya client mirror`,
);
