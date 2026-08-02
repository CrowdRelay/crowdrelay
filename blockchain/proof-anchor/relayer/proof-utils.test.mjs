import test from "node:test"
import assert from "node:assert/strict"
import { batchKey, errorKind, validateBatch, validatePending } from "./proof-utils.mjs"

test("UUID is mapped to a stable right-aligned bytes32", () => {
  assert.equal(
    batchKey("018f7a42-7c2a-7e58-8b1e-111111111111"),
    `0x${"0".repeat(32)}018f7a427c2a7e588b1e111111111111`,
  )
})

test("proof batch validation is strict", () => {
  const batch = {
    id: "018f7a42-7c2a-7e58-8b1e-111111111111",
    root_sha256: "ab".repeat(32),
    leaf_count: 2,
    schema_version: 1,
    proof_kind: "audit_ledger",
  }
  assert.equal(validateBatch(batch), batch)
  assert.throws(() => validateBatch({ ...batch, leaf_count: 0 }))
  assert.throws(() => validateBatch({ ...batch, id: "not-a-uuid" }))
})

test("pending transaction journal survives worker PID changes", () => {
  const pending = {
    workerId: "virya-anchor-oracle-01",
    transactionHash: `0x${"ab".repeat(32)}`,
    batches: [{ id: "018f7a42-7c2a-7e58-8b1e-111111111111" }],
  }
  assert.equal(validatePending(pending), pending)
  assert.throws(() => validatePending({ ...pending, workerId: "short" }))
  assert.throws(() => validatePending({ ...pending, batches: [...pending.batches, ...pending.batches] }))
})

test("confirmed journal validates receipt identity", () => {
  const pending = {
    workerId: "virya-anchor-oracle-01",
    transactionHash: `0x${"ab".repeat(32)}`,
    blockNumber: 123,
    blockHash: `0x${"cd".repeat(32)}`,
    batches: [{ id: "018f7a42-7c2a-7e58-8b1e-111111111111" }],
  }
  assert.equal(validatePending(pending), pending)
  assert.throws(() => validatePending({ ...pending, blockHash: "0x12" }))
})

test("provider errors are reduced to bounded non-secret kinds", () => {
  assert.equal(errorKind(new Error("insufficient funds for gas")), "evm.insufficient_funds")
  assert.equal(errorKind(new Error("crowdrelay_http_403")), "crowdrelay.authorization")
  assert.equal(errorKind(new Error("anchor_signer_mismatch")), "evm.signer_mismatch")
  assert.equal(errorKind(new Error("anchor_contract_missing")), "evm.contract_missing")
  assert.equal(errorKind(new Error("pending_journal broken")), "relayer.pending_journal")
  assert.equal(errorKind(new Error("https://secret-rpc.example token=abc")), "evm.rpc")
})
