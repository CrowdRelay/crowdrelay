import http from "node:http"
import process from "node:process"
import { mkdir, readFile, rename, rm, writeFile } from "node:fs/promises"
import { dirname } from "node:path"
import { processClaimedBatches } from "./batch-runner.mjs"
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
const crowdrelayUrl = requiredUrl("CROWDRELAY_INTERNAL_URL", true)
const rekorUrl = normalizeUrl(env.REKOR_URL || "https://rekor.sigstore.dev")
const workerId = validateWorkerId(env.ANCHOR_WORKER_ID || "virya-rekor-anchor-01")
const keyFile = env.REKOR_SIGNING_KEY_FILE || "/run/secrets/rekor_signing_key"
const tokenFile = env.CROWDRELAY_COMMERCE_API_KEY_FILE || "/run/secrets/crowdrelay_commerce_api_key"
const pendingFile = env.ANCHOR_PENDING_FILE || "/data/pending-confirmation.json"
const pollMs = boundedInt(env.ANCHOR_POLL_MS, 1_000, 60_000, 5_000)
const dependencyProbeMs = boundedInt(env.DEPENDENCY_PROBE_MS, 5_000, 300_000, 30_000)
const requestTimeoutMs = boundedInt(env.REQUEST_TIMEOUT_MS, 1_000, 60_000, 15_000)
const maxJsonResponseBytes = boundedInt(env.MAX_JSON_RESPONSE_BYTES, 16 * 1024, 2 * 1024 * 1024, 512 * 1024)
const maxTextResponseBytes = boundedInt(env.MAX_TEXT_RESPONSE_BYTES, 16 * 1024, 1024 * 1024, 256 * 1024)
const leaseSeconds = boundedInt(env.ANCHOR_LEASE_SECONDS, 30, 900, 300)
const claimLimit = boundedInt(env.ANCHOR_CLAIM_LIMIT, 1, 16, 8)
const port = boundedInt(env.PORT, 1, 65_535, 8081)

await verifyPendingStorage()
const [apiToken, privateKeyPem] = await Promise.all([
  readSecret(tokenFile),
  readSecret(keyFile, false),
])
const signer = loadSigner(privateKeyPem)

let stopping = false
let ready = false
let lastSuccessAt = null
let lastError = "startup.dependencies_unchecked"
let lastDependencyCheckAt = null
let lastDependencyCheckMs = 0
let dependencies = {
  crowdrelay: { ready: false, error: "unchecked" },
  rekor: { ready: false, error: "unchecked" },
}

const server = http.createServer((request, response) => {
  if (request.url === "/health/live") return json(response, 200, { status: "ok" })
  if (request.url === "/health/ready") {
    return json(response, ready ? 200 : 503, {
      status: ready ? "ok" : "degraded",
      anchor: "sigstore.rekor.v1",
      rekor_url: rekorUrl,
      signer_fingerprint: signer.fingerprint,
      dependencies,
      last_dependency_check_at: lastDependencyCheckAt,
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
    await ensureDependenciesReady()
    const worked = await processOne()
    ready = true
    lastError = null
    if (worked) lastSuccessAt = new Date().toISOString()
    if (!worked) await delay(pollMs)
  } catch (error) {
    ready = false
    lastError = errorKind(error)
    console.error(JSON.stringify({ level: "error", error_kind: lastError, message: safeMessage(error) }))
    await delay(pollMs)
  }
}

async function ensureDependenciesReady(force = false) {
  const now = Date.now()
  if (!force && ready && now - lastDependencyCheckMs < dependencyProbeMs) return

  const checkedAt = new Date().toISOString()
  const next = {
    crowdrelay: { ready: false, error: null },
    rekor: { ready: false, error: null },
  }

  try {
    await requestJson(`${crowdrelayUrl}/v1/health/ready`, {
      method: "GET",
      headers: { accept: "application/json" },
    })
    next.crowdrelay.ready = true
  } catch (error) {
    next.crowdrelay.error = safeMessage(error)
  }

  try {
    const publicKey = await requestText(`${rekorUrl}/api/v1/log/publicKey`, {
      method: "GET",
      headers: { accept: "text/plain, application/x-pem-file;q=0.9, */*;q=0.1" },
    })
    if (!publicKey.includes("BEGIN PUBLIC KEY") && !publicKey.includes("BEGIN CERTIFICATE")) {
      throw new Error("rekor public key response invalid")
    }
    next.rekor.ready = true
  } catch (error) {
    next.rekor.error = safeMessage(error)
  }

  dependencies = next
  lastDependencyCheckAt = checkedAt
  lastDependencyCheckMs = now
  ready = next.crowdrelay.ready && next.rekor.ready
  if (!ready) throw new Error("dependency readiness failed")
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

  const processed = await processClaimedBatches(batches, {
    validate: validateBatch,
    publish,
    savePending,
    confirm,
    clearPending,
    hasPending: loadPending,
    fail: async (batch, error) => {
      const kind = errorKind(error)
      await crowdrelay(`/v1/internal/proofs/${encodeURIComponent(batch.id)}/fail`, {
        method: "POST",
        body: { worker_id: workerId, error_kind: kind },
      }).catch((failError) => {
        throw new Error(`crowdrelay fail callback: ${safeMessage(failError)}`)
      })
    },
    onConfirmed: (batch, confirmation) => {
      console.log(JSON.stringify({ level: "info", event: "proof.confirmed", batch_id: batch.id, entry_uuid: confirmation.entry_uuid, log_index: confirmation.log_index }))
    },
    onFailed: (batch, error) => {
      console.error(JSON.stringify({ level: "warn", event: "proof.failed", batch_id: batch.id, error_kind: errorKind(error) }))
    },
  })
  return processed > 0
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

async function requestText(url, options) {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(new Error("request timeout")), requestTimeoutMs)
  try {
    const response = await fetch(url, { ...options, signal: controller.signal, redirect: "error" })
    const text = await readBodyLimited(response, maxTextResponseBytes, originLabel(url))
    if (!response.ok) throw new Error(`${originLabel(url)} HTTP ${response.status}`)
    if (text.length < 32) throw new Error(`${originLabel(url)} response size invalid`)
    return text
  } finally {
    clearTimeout(timer)
  }
}

async function requestJsonResponse(url, options, allowedStatuses = null) {
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(new Error("request timeout")), requestTimeoutMs)
  try {
    const response = await fetch(url, { ...options, signal: controller.signal, redirect: "error" })
    const text = await readBodyLimited(response, maxJsonResponseBytes, originLabel(url))
    if (!(allowedStatuses?.has(response.status) ?? response.ok)) {
      throw new Error(`${originLabel(url)} HTTP ${response.status}`)
    }
    let body = null
    if (text) {
      try { body = JSON.parse(text) } catch { throw new Error(`invalid JSON from ${originLabel(url)}`) }
    }
    return { status: response.status, headers: response.headers, body }
  } finally {
    clearTimeout(timer)
  }
}

async function readBodyLimited(response, maximumBytes, label) {
  const declaredLength = Number.parseInt(response.headers.get("content-length") || "", 10)
  if (Number.isFinite(declaredLength) && declaredLength > maximumBytes) {
    response.body?.cancel().catch(() => {})
    throw new Error(`${label} response too large`)
  }
  if (!response.body) return ""

  const reader = response.body.getReader()
  const chunks = []
  let total = 0
  while (true) {
    const { done, value } = await reader.read()
    if (done) break
    total += value.byteLength
    if (total > maximumBytes) {
      await reader.cancel().catch(() => {})
      throw new Error(`${label} response too large`)
    }
    chunks.push(Buffer.from(value))
  }
  return Buffer.concat(chunks, total).toString("utf8")
}

async function verifyPendingStorage() {
  const directory = dirname(pendingFile)
  await mkdir(directory, { recursive: true })
  const probe = `${directory}/.write-probe-${process.pid}`
  try {
    await writeFile(probe, "", { flag: "wx", mode: 0o600 })
  } catch (error) {
    throw new Error(`pending storage is not writable: ${safeMessage(error)}`)
  } finally {
    await rm(probe, { force: true }).catch(() => {})
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

function requiredUrl(name, allowTrustedHttp = false) {
  const value = env[name]
  if (!value) throw new Error(`${name} is required`)
  return normalizeUrl(value, allowTrustedHttp)
}

function normalizeUrl(value, allowTrustedHttp = false) {
  const parsed = new URL(value)
  const trustedHttpHosts = new Set(["localhost", "127.0.0.1", "crowdrelay-api", "api"])
  if (parsed.protocol !== "https:" && !(allowTrustedHttp && parsed.protocol === "http:" && trustedHttpHosts.has(parsed.hostname))) {
    throw new Error("URL must use HTTPS or a trusted local service name")
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
