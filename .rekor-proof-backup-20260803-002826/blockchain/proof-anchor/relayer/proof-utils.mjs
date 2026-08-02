const uuidPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i
const txPattern = /^0x[0-9a-f]{64}$/i
const blockPattern = /^0x[0-9a-f]{64}$/i
const workerPattern = /^[A-Za-z0-9._:-]{8,128}$/

export const errorKind = error => {
  const text = String(error?.shortMessage ?? error?.message ?? error ?? "unknown").toLowerCase()
  if (text.includes("insufficient funds")) return "evm.insufficient_funds"
  if (text.includes("nonce")) return "evm.nonce"
  if (text.includes("timeout")) return "evm.timeout"
  if (text.includes("anchor_signer")) return "evm.signer_mismatch"
  if (text.includes("anchor_contract")) return "evm.contract_missing"
  if (text.includes("anchor_batch_limit")) return "evm.batch_limit_mismatch"
  if (text.includes("chain")) return "evm.chain_mismatch"
  if (text.includes("crowdrelay_http_401") || text.includes("crowdrelay_http_403")) return "crowdrelay.authorization"
  if (text.includes("crowdrelay_http_409")) return "crowdrelay.lease_conflict"
  if (text.includes("crowdrelay_http_")) return "crowdrelay.http"
  if (text.includes("revert")) return "evm.contract_revert"
  if (text.includes("pending_journal")) return "relayer.pending_journal"
  return "evm.rpc"
}

export const validWorkerId = value => workerPattern.test(String(value))

export const batchKey = id => {
  const compact = String(id).replaceAll("-", "")
  if (!/^[0-9a-fA-F]{32}$/.test(compact)) throw new Error("invalid_batch_id")
  return `0x${"0".repeat(32)}${compact.toLowerCase()}`
}

export const validateBatch = batch => {
  if (!batch || typeof batch.id !== "string" || !uuidPattern.test(batch.id)) throw new Error("invalid_batch")
  batchKey(batch.id)
  if (!/^[0-9a-f]{64}$/i.test(batch.root_sha256)) throw new Error("invalid_root")
  if (!Number.isInteger(batch.leaf_count) || batch.leaf_count < 1 || batch.leaf_count > 100_000) {
    throw new Error("invalid_leaf_count")
  }
  if (!Number.isInteger(batch.schema_version) || batch.schema_version < 1) throw new Error("invalid_schema_version")
  if (!/^[a-z0-9_]{3,32}$/.test(batch.proof_kind)) throw new Error("invalid_proof_kind")
  return batch
}

export const validatePending = pending => {
  if (!pending || typeof pending !== "object") throw new Error("invalid_pending_journal")
  if (!validWorkerId(pending.workerId)) throw new Error("invalid_pending_journal_worker")
  if (!txPattern.test(String(pending.transactionHash))) throw new Error("invalid_pending_journal_tx")
  if (!Array.isArray(pending.batches) || pending.batches.length < 1 || pending.batches.length > 16) {
    throw new Error("invalid_pending_journal_batches")
  }
  const seen = new Set()
  for (const batch of pending.batches) {
    if (!batch || !uuidPattern.test(String(batch.id)) || seen.has(batch.id)) {
      throw new Error("invalid_pending_journal_batch")
    }
    seen.add(batch.id)
  }
  const hasReceipt = pending.blockNumber !== undefined || pending.blockHash !== undefined
  if (hasReceipt) {
    if (!Number.isSafeInteger(pending.blockNumber) || pending.blockNumber < 0) {
      throw new Error("invalid_pending_journal_block")
    }
    if (!blockPattern.test(String(pending.blockHash))) throw new Error("invalid_pending_journal_block_hash")
  }
  return pending
}
