import http from "node:http"
import process from "node:process"
import { mkdir, readFile, rename, rm, writeFile } from "node:fs/promises"
import { dirname } from "node:path"
import {
  buildProofPayload,
  buildRekorEntry,
  errorKind,
  loadSigner,
  parseRekorCreateResponse,
  rekorEntryUrlFromConflict,
  sha256Hex,
  signPayload,
  validateBatch,
  validatePending,
  validateWorkerId,
} from "./proof-utils.mjs"

const env = process.env
const crowdrelayUrl = requiredUrl("CROWDRELAY_INTERNAL_URL")
const rekorUrl = normalizeUrl(env.REKOR_URL || "https://rekor.sigstore.dev")
const workerId = validateWorkerId(env.ANCHOR_WORKER_ID || "virya-rekor-anchor-01")
const keyFile = env.REKOR_SIGNING_KEY_FILE || "/run/secrets/rekor_signing_key"
const tokenFile = env.CROWDRELAY_COMMERCE_API_KEY_FILE || "/run/secrets/crowdrelay_commerce_api_key"
const pendingFile = env.ANCHOR_PENDING_FILE || "/data/pending-confirmation.json"
const pollMs = boundedInt(env.ANCHOR_POLL_MS, 1_000, 60_000, 5_000)
const requestTimeoutMs = boundedInt(env.REQUEST_TIMEOUT_MS, 1_000, 60_000, 15_000)
const leaseSeconds = boundedInt(env.ANCHOR_LEASE_SECONDS, 30, 900, 300)
const claimLimit = boundedInt(env.ANCHOR_CLAIM_LIMIT, 1, 16, 8)
const port = boundedInt(env.PORT, 1, 65_535, 8081)

const [apiToken, privateKeyPem] = await Promise.all([
  readSecret(tokenFile),
  readSecret(keyFile, false),
])
const signer = loadSigner(privateKeyPem)

let stopping = false
let healthy = true
let lastSuccessAt = null
let lastError = null

const server = http.createServer((request, response) => {
  if (request.url === "/health/live") return json(response, 200, { status: "ok" })
  if (request.url === "/health/ready") {
    return json(response, healthy ? 200 : 503, {
      status: healthy ? "ok" : "degraded",
      anchor: "sigstore.rekor.v1",
      rekor_url: rekorUrl,
      signer_fingerprint: signer.fingerprint,
      last_success_at: lastSuccessAt,
      last_error: lastError,
    })
  }
  return json(response, 404, { error: "not_found" })
})
server.listen(port, "0.0.0.0")

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    stopping = true
    server.close()
  })
}

while (!stopping) {
  try {
    const worked = await processOne()
    healthy = true
    lastError = null
    if (worked) lastSuccessAt = new Date().toISOString()
    if (!worked) await delay(pollMs)
  } catch (error) {
    healthy = false
    lastError = errorKind(error)
    console.error(JSON.stringify({ level: "error", error_kind: lastError, message: safeMessage(error) }))
    await delay(pollMs)
  }
}

async function processOne() {
  const pending = await loadPending()
  if (pending) {
    await confirm(pending.batch.id, pending.confirmation)
    await clearPending()
    console.log(JSON.stringify({ level: "info", event: "proof.confirmed", batch_id: pending.batch.id, entry_uuid: pending.confirmation.entry_uuid }))
    return true
  }

  const claimed = await crowdrelay("/v1/internal/proofs/claim", {
    method: "POST",
    body: { worker_id: workerId, lease_seconds: leaseSeconds, limit: claimLimit },
  })
  const batches = Array.isArray(claimed?.batches) ? claimed.batches : []
  if (batches.length === 0) return false

  const batch = validateBatch(batches[0])
  try {
    const confirmation = await publish(batch)
    await savePending({ batch, confirmation })
    await confirm(batch.id, confirmation)
    await clearPending()
    console.log(JSON.stringify({ level: "info", event: "proof.confirmed", batch_id: batch.id, entry_uuid: confirmation.entry_uuid, log_index: confirmation.log_index }))
  } catch (error) {
    if (await loadPending()) throw error
    const kind = errorKind(error)
    await crowdrelay(`/v1/internal/proofs/${encodeURIComponent(batch.id)}/fail`, {
      method: "POST",
      body: { worker_id: workerId, error_kind: kind },
    }).catch((failError) => {
      throw new Error(`crowdrelay fail callback: ${safeMessage(failError)}`)
    })
    console.error(JSON.stringify({ level: "warn", event: "proof.failed", batch_id: batch.id, error_kind: kind }))
  }
  return true
}

async function publish(batch) {
  const payload = buildProofPayload(batch)
  const signature = signPayload(payload, signer)
  const entry = buildRekorEntry(payload, signature, signer.publicKeyPem)
  const created = await requestJsonResponse(`${rekorUrl}/api/v1/log/entries`, {
    method: "POST",
    headers: { accept: "application/json", "content-type": "application/json" },
    body: JSON.stringify(entry),
  }, new Set([201, 409]))
  const response = created.status === 201
    ? created.body
    : await requestJson(rekorEntryUrlFromConflict(created.headers.get("location"), rekorUrl), {
        method: "GET",
        headers: { accept: "application/json" },
      })
  return parseRekorCreateResponse(response, {
    anchorUrl: rekorUrl,
    payloadBase64: payload.toString("base64"),
    payloadSha256: sha256Hex(payload),
    signatureBase64: signature.toString("base64"),
    publicKeyBase64: Buffer.from(signer.publicKeyPem, "utf8").toString("base64"),
    publicKeyPem: signer.publicKeyPem,
    signerFingerprint: signer.fingerprint,
  })
}

async function confirm(batchId, confirmation) {
  return crowdrelay(`/v1/internal/proofs/${encodeURIComponent(batchId)}/confirm`, {
    method: "POST",
    body: { worker_id: workerId, ...confirmation },
  })
}

async function crowdrelay(path, options) {
  return requestJson(`${crowdrelayUrl}${path}`, {
    ...options,
    headers: {
      accept: "application/json",
      authorization: `Bearer ${apiToken}`,
      "content-type": "application/json",
      ...(options.headers || {}),
    },
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  })
}

async function requestJson(url, options) {
  return (await requestJsonResponse(url, options)).body
}

async function requestJsonResponse(url, options, allowedStatuses = null) {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(new Error("request timeout")), requestTimeoutMs)
  try {
    const response = await fetch(url, { ...options, signal: controller.signal, redirect: "error" })
    const text = await response.text()
    let body = null
    if (text) {
      try { body = JSON.parse(text) } catch { throw new Error(`invalid JSON from ${originLabel(url)}`) }
    }
    if (!(allowedStatuses?.has(response.status) ?? response.ok)) {
      throw new Error(`${originLabel(url)} HTTP ${response.status}`)
    }
    return { status: response.status, headers: response.headers, body }
  } finally {
    clearTimeout(timer)
  }
}

async function loadPending() {
  try {
    return validatePending(JSON.parse(await readFile(pendingFile, "utf8")))
  } catch (error) {
    if (error?.code === "ENOENT") return null
    throw new Error(`pending journal: ${safeMessage(error)}`)
  }
}

async function savePending(value) {
  validatePending(value)
  await mkdir(dirname(pendingFile), { recursive: true })
  const temp = `${pendingFile}.${process.pid}.tmp`
  await writeFile(temp, `${JSON.stringify(value)}\n`, { mode: 0o600 })
  await rename(temp, pendingFile)
}

async function clearPending() {
  await rm(pendingFile, { force: true })
}

async function readSecret(path, trim = true) {
  const value = await readFile(path, "utf8")
  const normalized = trim ? value.trim() : value
  if (!normalized) throw new Error(`empty secret file: ${path}`)
  return normalized
}

function requiredUrl(name) {
  const value = env[name]
  if (!value) throw new Error(`${name} is required`)
  return normalizeUrl(value)
}

function normalizeUrl(value) {
  const parsed = new URL(value)
  if (parsed.protocol !== "https:" && !(parsed.protocol === "http:" && ["localhost", "127.0.0.1"].includes(parsed.hostname))) {
    throw new Error("URL must use HTTPS")
  }
  parsed.pathname = parsed.pathname.replace(/\/+$/, "")
  parsed.search = ""
  parsed.hash = ""
  return parsed.toString().replace(/\/$/, "")
}

function boundedInt(value, minimum, maximum, fallback) {
  const parsed = value === undefined ? fallback : Number.parseInt(value, 10)
  if (!Number.isInteger(parsed) || parsed < minimum || parsed > maximum) throw new Error(`integer outside ${minimum}..${maximum}`)
  return parsed
}

function originLabel(url) {
  try { return new URL(url).origin.includes("rekor") ? "rekor" : "crowdrelay" } catch { return "remote" }
}

function safeMessage(error) {
  return String(error?.message ?? error).replace(/https?:\/\/[^\s]+/g, "[url]").slice(0, 240)
}

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

function json(response, status, body) {
  response.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" })
  response.end(JSON.stringify(body))
}
