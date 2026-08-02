import test from "node:test"
import assert from "node:assert/strict"
import { generateKeyPairSync } from "node:crypto"
import {
  buildProofPayload,
  buildRekorEntry,
  canonicalize,
  errorKind,
  loadSigner,
  parseRekorCreateResponse,
  rekorEntryUrlFromConflict,
  sha256Hex,
  signPayload,
  validateBatch,
  validatePending,
} from "./proof-utils.mjs"

const batch = {
  id: "018f86de-6e7e-7e87-9ce0-123456789abc",
  proof_kind: "reward_draw",
  schema_version: 1,
  root_sha256: "a".repeat(64),
  leaf_count: 12,
  tree_algorithm: "sha256-rfc6962",
  attempt: 1,
  lease_expires_at: "2026-08-03T00:00:00Z",
}

test("canonical JSON is stable", () => {
  assert.equal(canonicalize({ z: 1, a: { y: 2, x: 3 } }), '{"a":{"x":3,"y":2},"z":1}')
})

test("payload is deterministic and excludes retries", () => {
  const one = buildProofPayload(batch)
  const two = buildProofPayload({ ...batch, attempt: 9 })
  assert.deepEqual(one, two)
  assert.match(one.toString(), /crowdrelay\/proof-anchor\/v1/)
  assert.doesNotMatch(one.toString(), /lease_expires_at|attempt/)
})

test("RSA signer emits a deterministic Rekor rekord", () => {
  const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 })
  const pem = privateKey.export({ type: "pkcs8", format: "pem" })
  const signer = loadSigner(pem)
  const payload = buildProofPayload(batch)
  const signature1 = signPayload(payload, signer)
  const signature2 = signPayload(payload, signer)
  assert.deepEqual(signature1, signature2)
  const entry = buildRekorEntry(payload, signature1, signer.publicKeyPem)
  assert.equal(entry.kind, "rekord")
  assert.equal(entry.spec.signature.format, "x509")
})

test("parses and binds Rekor response", () => {
  const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 })
  const signer = loadSigner(privateKey.export({ type: "pkcs8", format: "pem" }))
  const payload = buildProofPayload(batch)
  const signature = signPayload(payload, signer)
  const entry = buildRekorEntry(payload, signature, signer.publicKeyPem)
  const body = Buffer.from(JSON.stringify(entry)).toString("base64")
  const expected = {
    anchorUrl: "https://rekor.sigstore.dev",
    payloadBase64: payload.toString("base64"),
    payloadSha256: sha256Hex(payload),
    signatureBase64: signature.toString("base64"),
    publicKeyBase64: Buffer.from(signer.publicKeyPem).toString("base64"),
    publicKeyPem: signer.publicKeyPem,
    signerFingerprint: signer.fingerprint,
  }
  const uuid = "a".repeat(64)
  const parsed = parseRekorCreateResponse({
    [uuid]: {
      body,
      integratedTime: 1_700_000_000,
      logID: "b".repeat(64),
      logIndex: 123,
      verification: {
        inclusionProof: { hashes: [], logIndex: 123, rootHash: "c".repeat(64), treeSize: 124 },
        signedEntryTimestamp: "dGVzdC1zaWduZWQtZW50cnktdGltZXN0YW1w",
      },
    },
  }, expected)
  assert.equal(parsed.entry_uuid, uuid)
  assert.equal(parsed.payload_sha256, sha256Hex(payload))
  validatePending({ batch, confirmation: parsed })
})

test("rejects malformed batches and classifies errors", () => {
  assert.throws(() => validateBatch({ ...batch, root_sha256: "no" }), /batch.root/)
  assert.equal(errorKind(new Error("pending journal corrupt")), "rekor.pending_journal")
  assert.equal(errorKind(new Error("rekor HTTP 500")), "rekor.api")
})


test("recovers a duplicate Rekor entry only from the configured origin", () => {
  const uuid = "a".repeat(64)
  assert.equal(
    rekorEntryUrlFromConflict(`/api/v1/log/entries/${uuid}`, "https://rekor.sigstore.dev"),
    `https://rekor.sigstore.dev/api/v1/log/entries/${uuid}`,
  )
  assert.throws(
    () => rekorEntryUrlFromConflict(`https://evil.example/api/v1/log/entries/${uuid}`, "https://rekor.sigstore.dev"),
    /rekor\.conflict_origin/,
  )
  assert.throws(
    () => rekorEntryUrlFromConflict("/api/v1/log/entries/not-a-uuid", "https://rekor.sigstore.dev"),
    /rekor\.conflict_path/,
  )
})
