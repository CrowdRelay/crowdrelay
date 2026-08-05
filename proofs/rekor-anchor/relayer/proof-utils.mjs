import { createHash, createPrivateKey, createPublicKey, sign as cryptoSign, verify as cryptoVerify } from "node:crypto"

const uuidPattern = /^[0-9a-f]{64,128}$/i
const sha256Pattern = /^[0-9a-f]{64}$/i
const workerPattern = /^[A-Za-z0-9._:-]{8,128}$/
const proofKindPattern = /^[a-z0-9._/-]{1,64}$/
const treePattern = /^[a-z0-9._/-]{1,64}$/

export function canonicalize(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalize).join(",")}]`
  if (value !== null && typeof value === "object") {
    const keys = Object.keys(value).sort()
    return `{${keys.map((key) => `${JSON.stringify(key)}:${canonicalize(value[key])}`).join(",")}}`
  }
  return JSON.stringify(value)
}

export function sha256Hex(data) {
  return createHash("sha256").update(data).digest("hex")
}

export function validateBatch(batch) {
  if (!batch || typeof batch !== "object") throw new Error("batch.invalid")
  if (typeof batch.id !== "string" || !/^[0-9a-f-]{36}$/i.test(batch.id)) throw new Error("batch.id")
  if (typeof batch.proof_kind !== "string" || !proofKindPattern.test(batch.proof_kind)) throw new Error("batch.proof_kind")
  if (!Number.isInteger(batch.schema_version) || batch.schema_version <= 0) throw new Error("batch.schema_version")
  if (typeof batch.root_sha256 !== "string" || !sha256Pattern.test(batch.root_sha256)) throw new Error("batch.root")
  if (!Number.isInteger(batch.leaf_count) || batch.leaf_count <= 0) throw new Error("batch.leaf_count")
  if (typeof batch.tree_algorithm !== "string" || !treePattern.test(batch.tree_algorithm)) throw new Error("batch.tree_algorithm")
  if (!Number.isInteger(batch.attempt) || batch.attempt <= 0) throw new Error("batch.attempt")
  return batch
}

export function validateWorkerId(value) {
  if (typeof value !== "string" || !workerPattern.test(value)) throw new Error("worker.invalid")
  return value
}

export function buildProofPayload(batch) {
  validateBatch(batch)
  const payload = {
    batch_id: batch.id,
    hash_algorithm: "sha256",
    leaf_count: batch.leaf_count,
    proof_kind: batch.proof_kind,
    root_sha256: batch.root_sha256.toLowerCase(),
    schema: "crowdrelay/proof-anchor/v1",
    schema_version: batch.schema_version,
    tree_algorithm: batch.tree_algorithm,
  }
  return Buffer.from(canonicalize(payload), "utf8")
}

export function loadSigner(privateKeyPem) {
  const privateKey = createPrivateKey(privateKeyPem)
  const publicKey = createPublicKey(privateKey)
  if (privateKey.asymmetricKeyType !== "rsa") throw new Error("signer.key_type")
  const publicKeyPem = publicKey.export({ type: "spki", format: "pem" }).toString()
  const publicKeyDer = publicKey.export({ type: "spki", format: "der" })
  return {
    privateKey,
    publicKey,
    publicKeyPem,
    fingerprint: `sha256:${sha256Hex(publicKeyDer)}`,
  }
}

export function signPayload(payloadBytes, signer) {
  const signature = cryptoSign("sha256", payloadBytes, signer.privateKey)
  if (!cryptoVerify("sha256", payloadBytes, signer.publicKey, signature)) throw new Error("signer.self_verify")
  return signature
}

export function buildRekorEntry(payloadBytes, signature, publicKeyPem) {
  return {
    apiVersion: "0.0.1",
    kind: "rekord",
    spec: {
      data: { content: payloadBytes.toString("base64") },
      signature: {
        content: signature.toString("base64"),
        format: "x509",
        publicKey: { content: Buffer.from(publicKeyPem, "utf8").toString("base64") },
      },
    },
  }
}

export function rekorEntryUrlFromConflict(location, anchorUrl) {
  if (typeof location !== "string" || location.length === 0) throw new Error("rekor.conflict_location")
  const anchor = new URL(anchorUrl)
  const resolved = new URL(location, anchor)
  if (resolved.origin !== anchor.origin) throw new Error("rekor.conflict_origin")
  if (!/^\/api\/v1\/log\/entries\/[0-9a-f]{64,128}$/i.test(resolved.pathname)) {
    throw new Error("rekor.conflict_path")
  }
  resolved.search = ""
  resolved.hash = ""
  return resolved.toString()
}

export function parseRekorCreateResponse(responseJson, expected) {
  if (!responseJson || typeof responseJson !== "object" || Array.isArray(responseJson)) throw new Error("rekor.response")
  const entries = Object.entries(responseJson)
  if (entries.length !== 1) throw new Error("rekor.response_count")
  const [entryUuid, record] = entries[0]
  if (!uuidPattern.test(entryUuid)) throw new Error("rekor.entry_uuid")
  if (!record || typeof record !== "object") throw new Error("rekor.record")
  if (!Number.isSafeInteger(record.logIndex) || record.logIndex < 0) throw new Error("rekor.log_index")
  if (!Number.isSafeInteger(record.integratedTime) || record.integratedTime <= 0) throw new Error("rekor.integrated_time")
  if (typeof record.logID !== "string" || !sha256Pattern.test(record.logID)) throw new Error("rekor.log_id")
  if (typeof record.body !== "string" || record.body.length < 8) throw new Error("rekor.body")
  const verification = record.verification
  if (!verification || typeof verification !== "object") throw new Error("rekor.verification")
  if (typeof verification.signedEntryTimestamp !== "string" || verification.signedEntryTimestamp.length < 16) {
    throw new Error("rekor.set")
  }
  const inclusionProof = verification.inclusionProof
  if (!inclusionProof || typeof inclusionProof !== "object") throw new Error("rekor.inclusion_proof")
  if (inclusionProof.logIndex !== record.logIndex) throw new Error("rekor.inclusion_log_index")
  if (!verifyInclusionProof(Buffer.from(record.body, "base64"), inclusionProof)) {
    throw new Error("rekor.inclusion_invalid")
  }

  const canonicalBody = JSON.parse(Buffer.from(record.body, "base64").toString("utf8"))
  if (canonicalBody.kind !== "rekord" || canonicalBody.apiVersion !== "0.0.1") throw new Error("rekor.body_kind")
  const spec = canonicalBody.spec
  if (!spec) throw new Error("rekor.body_payload")
  const payloadContentMatches = spec.data?.content === expected.payloadBase64
  const payloadHashMatches = spec.data?.hash?.algorithm === "sha256"
    && typeof spec.data.hash.value === "string"
    && spec.data.hash.value.toLowerCase() === expected.payloadSha256.toLowerCase()
  if (!payloadContentMatches && !payloadHashMatches) throw new Error("rekor.body_payload")
  if (spec.signature?.content !== expected.signatureBase64) throw new Error("rekor.body_signature")
  if (spec.signature?.format !== "x509") throw new Error("rekor.body_format")
  if (spec.signature?.publicKey?.content !== expected.publicKeyBase64) throw new Error("rekor.body_public_key")

  return {
    anchor_kind: "sigstore.rekor.v1",
    anchor_url: expected.anchorUrl,
    canonicalized_body: record.body,
    entry_uuid: entryUuid.toLowerCase(),
    inclusion_proof: inclusionProof,
    integrated_time: record.integratedTime,
    log_id: record.logID.toLowerCase(),
    log_index: record.logIndex,
    payload_sha256: expected.payloadSha256,
    public_key_pem: expected.publicKeyPem,
    signature_base64: expected.signatureBase64,
    signed_entry_timestamp: verification.signedEntryTimestamp,
    signer_fingerprint: expected.signerFingerprint,
  }
}


export function verifyInclusionProof(canonicalBody, proof) {
  if (!Buffer.isBuffer(canonicalBody) || canonicalBody.length === 0) return false
  if (!proof || !Number.isSafeInteger(proof.logIndex) || !Number.isSafeInteger(proof.treeSize)) return false
  if (proof.logIndex < 0 || proof.treeSize <= proof.logIndex || !Array.isArray(proof.hashes)) return false

  let fn = proof.logIndex
  let sn = proof.treeSize - 1
  let hash = createHash("sha256").update(Buffer.concat([Buffer.from([0]), canonicalBody])).digest()
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
  return createHash("sha256").update(Buffer.concat([Buffer.from([1]), left, right])).digest()
}

function decodeHash(value) {
  if (typeof value !== "string") return null
  if (/^[0-9a-f]{64}$/i.test(value)) return Buffer.from(value, "hex")
  try {
    const decoded = Buffer.from(value, "base64")
    return decoded.length === 32 ? decoded : null
  } catch {
    return null
  }
}

export function validatePending(value) {
  if (!value || typeof value !== "object") throw new Error("pending.invalid")
  validateBatch(value.batch)
  if (!value.confirmation || typeof value.confirmation !== "object") throw new Error("pending.confirmation")
  const c = value.confirmation
  if (c.anchor_kind !== "sigstore.rekor.v1") throw new Error("pending.anchor_kind")
  if (typeof c.anchor_url !== "string" || !c.anchor_url.startsWith("https://")) throw new Error("pending.anchor_url")
  if (typeof c.entry_uuid !== "string" || !uuidPattern.test(c.entry_uuid)) throw new Error("pending.entry_uuid")
  if (!Number.isSafeInteger(c.log_index) || c.log_index < 0) throw new Error("pending.log_index")
  if (!Number.isSafeInteger(c.integrated_time) || c.integrated_time <= 0) throw new Error("pending.integrated_time")
  if (typeof c.log_id !== "string" || !sha256Pattern.test(c.log_id)) throw new Error("pending.log_id")
  if (typeof c.payload_sha256 !== "string" || !sha256Pattern.test(c.payload_sha256)) throw new Error("pending.payload_sha256")
  if (typeof c.signature_base64 !== "string" || c.signature_base64.length < 32) throw new Error("pending.signature")
  if (typeof c.public_key_pem !== "string" || !c.public_key_pem.includes("BEGIN PUBLIC KEY")) throw new Error("pending.public_key")
  if (typeof c.signer_fingerprint !== "string" || !c.signer_fingerprint.startsWith("sha256:")) throw new Error("pending.fingerprint")
  if (typeof c.canonicalized_body !== "string" || c.canonicalized_body.length < 8) throw new Error("pending.body")
  if (typeof c.signed_entry_timestamp !== "string" || c.signed_entry_timestamp.length < 16) throw new Error("pending.set")
  if (!c.inclusion_proof || typeof c.inclusion_proof !== "object") throw new Error("pending.inclusion_proof")
  return value
}

export function errorKind(error) {
  const message = String(error?.message ?? error).toLowerCase()
  if (message.includes("pending")) return "rekor.pending_journal"
  if (message.includes("signer") || message.includes("key")) return "rekor.signer"
  if (message.includes("timeout") || message.includes("abort")) return "rekor.timeout"
  if (message.includes("crowdrelay")) return "rekor.crowdrelay"
  if (message.includes("rekor")) return "rekor.api"
  if (message.includes("batch")) return "rekor.batch"
  return "rekor.unexpected"
}
