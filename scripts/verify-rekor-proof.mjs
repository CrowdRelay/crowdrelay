#!/usr/bin/env node
import { createHash, createPublicKey, verify as cryptoVerify, generateKeyPairSync, sign as cryptoSign } from "node:crypto"
import { readFile } from "node:fs/promises"

const args = process.argv.slice(2)
if (args[0] === "self-test") {
  await selfTest()
  console.log("rekor_verifier_self_test=ok")
  process.exit(0)
}
if (!args[0]) usage()

const proof = JSON.parse(await readFile(args[0], "utf8"))
const result = await verifyProof(proof, args.includes("--online"))
console.log(JSON.stringify(result, null, 2))
process.exit(result.valid ? 0 : 1)

async function verifyProof(proof, online) {
  const anchor = proof.anchor ?? proof
  const receipt = anchor.receipt ?? anchor.anchor_receipt
  const errors = []
  if (!receipt || typeof receipt !== "object") return { valid: false, errors: ["missing anchor receipt"] }

  let body
  try {
    body = JSON.parse(Buffer.from(receipt.canonicalized_body, "base64").toString("utf8"))
  } catch {
    return { valid: false, errors: ["invalid canonicalized Rekor body"] }
  }
  if (body.kind !== "rekord" || body.apiVersion !== "0.0.1") errors.push("unexpected Rekor entry kind")

  const payloadBytes = decodeBase64(body.spec?.data?.content, "payload", errors)
  const signature = decodeBase64(body.spec?.signature?.content, "signature", errors)
  const publicKeyBytes = decodeBase64(body.spec?.signature?.publicKey?.content, "public key", errors)
  const payloadHash = payloadBytes ? sha256(payloadBytes).toString("hex") : null
  if (payloadHash && anchor.signed_payload_sha256 && payloadHash !== anchor.signed_payload_sha256) {
    errors.push("signed payload hash mismatch")
  }

  let payload = null
  if (payloadBytes) {
    try { payload = JSON.parse(payloadBytes.toString("utf8")) } catch { errors.push("invalid signed payload JSON") }
  }
  if (payload?.schema !== "crowdrelay/proof-anchor/v1") errors.push("unexpected CrowdRelay proof schema")
  if (payloadBytes && payload && !payloadBytes.equals(Buffer.from(canonicalize(payload), "utf8"))) {
    errors.push("signed payload is not canonical JSON")
  }

  const expectedBatchId = proof.id ?? proof.batch_id ?? anchor.batch_id
  const expectedRoot = proof.root_sha256 ?? proof.receipt_sha256
  const expectedLeafCount = proof.leaf_count ?? (proof.receipt_sha256 ? 1 : undefined)
  const expectedProofKind = proof.proof_kind ?? (proof.receipt_sha256 ? "draw_receipt" : undefined)
  const expectedSchemaVersion = proof.schema_version ?? (proof.receipt_sha256 ? 1 : undefined)
  const expectedTreeAlgorithm = proof.tree_algorithm ?? (proof.receipt_sha256 ? "single-leaf-v1" : undefined)
  if (payload?.hash_algorithm !== "sha256") errors.push("unexpected proof hash algorithm")
  if (expectedBatchId && payload?.batch_id !== expectedBatchId) errors.push("batch id mismatch")
  if (expectedRoot && payload?.root_sha256 !== expectedRoot) errors.push("proof root mismatch")
  if (expectedLeafCount !== undefined && payload?.leaf_count !== expectedLeafCount) errors.push("leaf count mismatch")
  if (expectedProofKind && payload?.proof_kind !== expectedProofKind) errors.push("proof kind mismatch")
  if (expectedSchemaVersion !== undefined && payload?.schema_version !== expectedSchemaVersion) errors.push("schema version mismatch")
  if (expectedTreeAlgorithm && payload?.tree_algorithm !== expectedTreeAlgorithm) errors.push("tree algorithm mismatch")

  if (receipt.signature_base64 && body.spec?.signature?.content !== receipt.signature_base64) {
    errors.push("receipt signature mismatch")
  }
  if (receipt.public_key_pem && publicKeyBytes && publicKeyBytes.toString("utf8") !== receipt.public_key_pem) {
    errors.push("receipt public key mismatch")
  }

  if (payloadBytes && signature && publicKeyBytes) {
    try {
      const key = createPublicKey(publicKeyBytes.toString("utf8"))
      if (!cryptoVerify("sha256", payloadBytes, key, signature)) errors.push("payload signature invalid")
      const fingerprint = `sha256:${sha256(key.export({ type: "spki", format: "der" })).toString("hex")}`
      if (anchor.signer_fingerprint && fingerprint !== anchor.signer_fingerprint) errors.push("signer fingerprint mismatch")
    } catch {
      errors.push("invalid embedded public key")
    }
  }

  const inclusion = receipt.inclusion_proof
  const anchorSequence = anchor.sequence ?? anchor.log_index
  if (anchorSequence !== undefined && inclusion?.logIndex !== anchorSequence) {
    errors.push("Rekor log index mismatch")
  }
  if (typeof receipt.signed_entry_timestamp !== "string" || receipt.signed_entry_timestamp.length < 16) {
    errors.push("missing Rekor Signed Entry Timestamp")
  }
  if (!verifyInclusion(Buffer.from(receipt.canonicalized_body, "base64"), inclusion)) {
    errors.push("Rekor inclusion proof invalid")
  }

  if (online) {
    try {
      const base = String(anchor.anchor_url).replace(/\/+$/, "")
      const response = await fetch(`${base}/api/v1/log/entries/${anchor.entry_id ?? anchor.entry_uuid}`, { redirect: "error" })
      if (!response.ok) errors.push(`online Rekor lookup returned HTTP ${response.status}`)
      else {
        const remote = await response.json()
        const record = remote[anchor.entry_id ?? anchor.entry_uuid]
        if (!record || record.body !== receipt.canonicalized_body) errors.push("online Rekor body mismatch")
      }
    } catch {
      errors.push("online Rekor lookup failed")
    }
  }

  return {
    valid: errors.length === 0,
    errors,
    batch_id: payload?.batch_id ?? null,
    root_sha256: payload?.root_sha256 ?? null,
    entry_id: anchor.entry_id ?? anchor.entry_uuid ?? null,
    log_index: anchor.sequence ?? anchor.log_index ?? null,
    integrated_at: anchor.integrated_at ?? null,
  }
}

function verifyInclusion(canonicalBody, proof) {
  if (!proof || !Number.isSafeInteger(proof.logIndex) || !Number.isSafeInteger(proof.treeSize)) return false
  if (proof.logIndex < 0 || proof.treeSize <= proof.logIndex || !Array.isArray(proof.hashes)) return false
  let fn = proof.logIndex
  let sn = proof.treeSize - 1
  let hash = sha256(Buffer.concat([Buffer.from([0]), canonicalBody]))
  for (const encoded of proof.hashes) {
    const sibling = decodeHash(encoded)
    if (!sibling) return false
    if ((fn & 1) === 1 || fn === sn) {
      hash = nodeHash(sibling, hash)
      while ((fn & 1) === 0 && fn !== 0) {
        fn >>= 1
        sn >>= 1
      }
    } else {
      hash = nodeHash(hash, sibling)
    }
    fn >>= 1
    sn >>= 1
  }
  const expected = decodeHash(proof.rootHash)
  return expected !== null && hash.equals(expected)
}

function nodeHash(left, right) {
  return sha256(Buffer.concat([Buffer.from([1]), left, right]))
}

function decodeHash(value) {
  if (typeof value !== "string") return null
  if (/^[0-9a-f]{64}$/i.test(value)) return Buffer.from(value, "hex")
  try {
    const decoded = Buffer.from(value, "base64")
    return decoded.length === 32 ? decoded : null
  } catch { return null }
}

function decodeBase64(value, label, errors) {
  if (typeof value !== "string") { errors.push(`missing ${label}`); return null }
  try {
    const decoded = Buffer.from(value, "base64")
    if (decoded.length === 0) throw new Error()
    return decoded
  } catch { errors.push(`invalid ${label}`); return null }
}

function canonicalize(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalize).join(",")}]`
  if (value !== null && typeof value === "object") {
    const keys = Object.keys(value).sort()
    return `{${keys.map((key) => `${JSON.stringify(key)}:${canonicalize(value[key])}`).join(",")}}`
  }
  return JSON.stringify(value)
}

function sha256(value) {
  return createHash("sha256").update(value).digest()
}

async function selfTest() {
  const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 })
  const payload = Buffer.from('{"batch_id":"018f86de-6e7e-7e87-9ce0-123456789abc","hash_algorithm":"sha256","leaf_count":1,"proof_kind":"draw_receipt","root_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","schema":"crowdrelay/proof-anchor/v1","schema_version":1,"tree_algorithm":"single-leaf-v1"}')
  const signature = cryptoSign("sha256", payload, privateKey)
  const body = {
    apiVersion: "0.0.1",
    kind: "rekord",
    spec: {
      data: { content: payload.toString("base64") },
      signature: {
        content: signature.toString("base64"),
        format: "x509",
        publicKey: { content: Buffer.from(publicKey.export({ type: "spki", format: "pem" })).toString("base64") },
      },
    },
  }
  const canonicalizedBody = Buffer.from(JSON.stringify(body)).toString("base64")
  const rootHash = sha256(Buffer.concat([Buffer.from([0]), Buffer.from(canonicalizedBody, "base64")])).toString("hex")
  const fingerprint = `sha256:${sha256(publicKey.export({ type: "spki", format: "der" })).toString("hex")}`
  const result = await verifyProof({
    id: "018f86de-6e7e-7e87-9ce0-123456789abc",
    root_sha256: "a".repeat(64),
    leaf_count: 1,
    anchor: {
      entry_id: "b".repeat(64),
      signed_payload_sha256: sha256(payload).toString("hex"),
      signer_fingerprint: fingerprint,
      receipt: {
        canonicalized_body: canonicalizedBody,
        inclusion_proof: { hashes: [], logIndex: 0, rootHash, treeSize: 1 },
        public_key_pem: publicKey.export({ type: "spki", format: "pem" }).toString(),
        signature_base64: signature.toString("base64"),
        signed_entry_timestamp: "dGVzdC1zaWduZWQtZW50cnktdGltZXN0YW1w",
      },
    },
  }, false)
  if (!result.valid) throw new Error(result.errors.join(", "))
}

function usage() {
  console.error("usage: node scripts/verify-rekor-proof.mjs <public-proof.json> [--online]")
  process.exit(2)
}
