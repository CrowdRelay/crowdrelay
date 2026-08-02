import http from "node:http"
import process from "node:process"
import { mkdir, readFile, rename, rm, writeFile } from "node:fs/promises"
import { dirname } from "node:path"
import {
  createPublicClient,
  createWalletClient,
  defineChain,
  http as viemHttp,
  keccak256,
  stringToHex,
} from "viem"
import { privateKeyToAccount } from "viem/accounts"
import {
  batchKey,
  errorKind,
  validateBatch,
  validatePending,
  validWorkerId,
} from "./proof-utils.mjs"

const env = (name, fallback = "") => (process.env[name] ?? fallback).trim()
const required = name => {
  const value = env(name)
  if (!value) throw new Error(`missing_${name.toLowerCase()}`)
  return value
}
const integer = (name, fallback, min, max) => {
  const value = Number(env(name, String(fallback)))
  if (!Number.isInteger(value) || value < min || value > max) throw new Error(`invalid_${name.toLowerCase()}`)
  return value
}
const secret = async (fileName, legacyName) => {
  const path = required(fileName)
  const value = (await readFile(path, "utf8")).trim()
  if (!value) throw new Error(`empty_${fileName.toLowerCase()}`)
  if (env(legacyName)) throw new Error(`legacy_secret_env_forbidden_${legacyName.toLowerCase()}`)
  return value
}

const crowdRelayUrl = required("CROWDRELAY_INTERNAL_URL").replace(/\/$/, "")
const crowdRelayToken = await secret("CROWDRELAY_COMMERCE_API_KEY_FILE", "CROWDRELAY_COMMERCE_API_KEY")
const rpcUrl = await secret("EVM_RPC_URL_FILE", "EVM_RPC_URL")
const chainId = integer("EVM_CHAIN_ID", 84532, 1, Number.MAX_SAFE_INTEGER)
const chainName = env("EVM_CHAIN_NAME", "Base Sepolia")
const contractAddress = required("EVM_PROOF_ANCHOR_ADDRESS").toLowerCase()
const privateKey = await secret("EVM_ANCHOR_PRIVATE_KEY_FILE", "EVM_ANCHOR_PRIVATE_KEY")
const pollMs = integer("ANCHOR_POLL_MS", 15_000, 1_000, 300_000)
const confirmations = integer("ANCHOR_CONFIRMATIONS", 2, 1, 64)
const claimLimit = integer("ANCHOR_BATCH_SIZE", 16, 1, 16)
const healthPort = integer("PORT", 8081, 1, 65_535)
const workerId = required("ANCHOR_WORKER_ID")
const pendingFile = env("ANCHOR_PENDING_FILE", "/data/pending-confirmation.json")

if (!/^0x[0-9a-fA-F]{40}$/.test(contractAddress)) throw new Error("invalid_contract_address")
if (!/^0x[0-9a-fA-F]{64}$/.test(privateKey)) throw new Error("invalid_private_key")
if (!validWorkerId(workerId)) throw new Error("invalid_worker_id")
if (crowdRelayToken.length < 16 || crowdRelayToken.length > 4096) throw new Error("invalid_crowdrelay_token")

const chain = defineChain({
  id: chainId,
  name: chainName,
  nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
  rpcUrls: { default: { http: [rpcUrl] } },
})
const account = privateKeyToAccount(privateKey)
const transport = viemHttp(rpcUrl, { timeout: 30_000, retryCount: 2, retryDelay: 1_000 })
const publicClient = createPublicClient({ chain, transport })
const walletClient = createWalletClient({ account, chain, transport })

const abi = [
  {
    type: "function",
    name: "anchorSigner",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "address" }],
  },
  {
    type: "function",
    name: "MAX_BATCH_SIZE",
    stateMutability: "view",
    inputs: [],
    outputs: [{ name: "", type: "uint256" }],
  },
  {
    type: "function",
    name: "anchorMany",
    stateMutability: "nonpayable",
    inputs: [
      { name: "batchKeys", type: "bytes32[]" },
      { name: "roots", type: "bytes32[]" },
      { name: "leafCounts", type: "uint32[]" },
      { name: "schemaHashes", type: "bytes32[]" },
    ],
    outputs: [],
  },
]

let stopping = false
let busy = false
let lastSuccessAt = null
let lastErrorKind = null
let processed = 0
let transactions = 0
let lastChainCheckAt = 0

const delay = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds))
const headers = correlationId => ({
  "authorization": `Bearer ${crowdRelayToken}`,
  "accept": "application/json",
  "content-type": "application/json",
  "x-crowdrelay-correlation-id": correlationId,
})

const request = async (path, body, correlationId) => {
  const response = await fetch(`${crowdRelayUrl}${path}`, {
    method: "POST",
    headers: headers(correlationId),
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(30_000),
  })
  const payload = await response.json().catch(() => ({}))
  if (!response.ok) throw new Error(`crowdrelay_http_${response.status}`)
  return payload
}

const verifyChain = async () => {
  const now = Date.now()
  if (now - lastChainCheckAt < 300_000) return
  const currentChainId = await publicClient.getChainId()
  if (currentChainId !== chainId) throw new Error("chain_id_mismatch")
  const bytecode = await publicClient.getBytecode({ address: contractAddress })
  if (!bytecode || bytecode === "0x") throw new Error("anchor_contract_missing")
  const [configuredSigner, contractBatchLimit] = await Promise.all([
    publicClient.readContract({ address: contractAddress, abi, functionName: "anchorSigner" }),
    publicClient.readContract({ address: contractAddress, abi, functionName: "MAX_BATCH_SIZE" }),
  ])
  if (String(configuredSigner).toLowerCase() !== account.address.toLowerCase()) {
    throw new Error("anchor_signer_mismatch")
  }
  if (BigInt(contractBatchLimit) < BigInt(claimLimit)) throw new Error("anchor_batch_limit_mismatch")
  lastChainCheckAt = now
}

const writePending = async pending => {
  const validated = validatePending(pending)
  await mkdir(dirname(pendingFile), { recursive: true })
  const temporary = `${pendingFile}.${process.pid}.tmp`
  await writeFile(temporary, `${JSON.stringify(validated)}\n`, { mode: 0o600 })
  await rename(temporary, pendingFile)
}

const readPending = async () => {
  try {
    return validatePending(JSON.parse(await readFile(pendingFile, "utf8")))
  } catch (error) {
    if (error?.code === "ENOENT") return null
    throw error
  }
}

const clearPending = async () => {
  await rm(pendingFile, { force: true })
}

const ensureStateWritable = async () => {
  await mkdir(dirname(pendingFile), { recursive: true })
  const probe = `${pendingFile}.${process.pid}.probe`
  await writeFile(probe, "ok\n", { mode: 0o600, flag: "wx" })
  await rm(probe, { force: true })
}


const failClaimed = async (batches, kind, claimedWorkerId = workerId) => {
  await Promise.allSettled(
    batches.map(batch =>
      request(
        `/v1/internal/proofs/${batch.id}/fail`,
        { worker_id: claimedWorkerId, error_kind: kind },
        `${claimedWorkerId}:${batch.id}:fail`,
      ),
    ),
  )
}

const resolveReceipt = async pending => {
  if (pending.blockNumber !== undefined) return pending
  const receipt = await publicClient.waitForTransactionReceipt({
    hash: pending.transactionHash,
    confirmations,
    timeout: 240_000,
  })
  if (receipt.status !== "success") {
    await clearPending()
    await failClaimed(pending.batches, "evm.contract_revert", pending.workerId)
    throw new Error("transaction_reverted")
  }
  const completed = validatePending({
    ...pending,
    blockNumber: Number(receipt.blockNumber),
    blockHash: receipt.blockHash,
  })
  await writePending(completed)
  return completed
}

const confirmPending = async pendingInput => {
  const pending = await resolveReceipt(validatePending(pendingInput))
  const confirmationsToWrite = pending.batches.map((batch, index) =>
    request(
      `/v1/internal/proofs/${batch.id}/confirm`,
      {
        worker_id: pending.workerId,
        chain_namespace: "eip155",
        chain_id: chainId,
        contract_address: contractAddress,
        transaction_hash: pending.transactionHash,
        block_number: pending.blockNumber,
        block_hash: pending.blockHash,
        transaction_batch_index: index,
      },
      `${pending.workerId}:${batch.id}:confirm`,
    ),
  )
  const results = await Promise.allSettled(confirmationsToWrite)
  const failed = results.filter(result => result.status === "rejected")
  if (failed.length) throw new Error("crowdrelay_confirmation_incomplete")
  processed += pending.batches.length
  lastSuccessAt = new Date().toISOString()
  lastErrorKind = null
  await clearPending()
}

const recoverPending = async () => {
  const pending = await readPending()
  if (!pending) return false
  await confirmPending(pending)
  return true
}

const processBatch = async () => {
  if (busy || stopping) return false
  busy = true
  let batches = []
  let transactionSubmitted = false
  try {
    if (await recoverPending()) return true
    await verifyChain()
    const claim = await request(
      "/v1/internal/proofs/claim",
      { worker_id: workerId, lease_seconds: 600, limit: claimLimit },
      `${workerId}:claim`,
    )
    batches = Array.isArray(claim.batches) ? claim.batches.map(validateBatch) : []
    if (!batches.length) {
      lastErrorKind = null
      return false
    }

    const hash = await walletClient.writeContract({
      address: contractAddress,
      abi,
      functionName: "anchorMany",
      args: [
        batches.map(batch => batchKey(batch.id)),
        batches.map(batch => `0x${batch.root_sha256}`),
        batches.map(batch => batch.leaf_count),
        batches.map(batch =>
          keccak256(stringToHex(`crowdrelay/${batch.proof_kind}/v${batch.schema_version}`)),
        ),
      ],
    })
    transactionSubmitted = true
    transactions += 1
    const pending = {
      workerId,
      transactionHash: hash,
      batches: batches.map(batch => ({ id: batch.id })),
    }
    await writePending(pending)
    await confirmPending(pending)
    return true
  } catch (error) {
    const kind = errorKind(error)
    lastErrorKind = kind
    if (!transactionSubmitted && batches.length) await failClaimed(batches, kind)
    console.error(JSON.stringify({
      level: "error",
      message: "proof anchor batch failed",
      kind,
      batchCount: batches.length,
      transactionSubmitted,
    }))
    return false
  } finally {
    busy = false
  }
}

await ensureStateWritable()
await verifyChain()

const server = http.createServer((requestMessage, response) => {
  if (requestMessage.url !== "/health") {
    response.writeHead(404).end()
    return
  }
  response.writeHead(lastErrorKind ? 503 : 200, {
    "content-type": "application/json",
    "cache-control": "no-store",
  })
  response.end(JSON.stringify({
    status: lastErrorKind ? "degraded" : "ok",
    busy,
    processed,
    transactions,
    lastSuccessAt,
    lastErrorKind,
    chainId,
  }))
})
server.listen(healthPort, "0.0.0.0")

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    stopping = true
    server.close()
  })
}

while (!stopping) {
  const worked = await processBatch()
  if (!worked) await delay(pollMs)
}
