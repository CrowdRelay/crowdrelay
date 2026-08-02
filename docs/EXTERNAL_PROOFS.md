# Proof of Fair: local receipts + Rekor

CrowdRelay proves fairness in two independent layers:

1. **Local deterministic proof** — PostgreSQL stores the draw inputs, candidate and winner snapshots, revealed seed, algorithm version, public receipt hash and Merkle inclusion data. This is the source of truth and can be verified offline.
2. **Public existence proof** — an isolated worker signs a metadata-only commitment and publishes it to the Sigstore Rekor transparency log. Rekor contributes an append-only public timestamp, entry UUID, log index, Signed Entry Timestamp and Merkle inclusion proof.

There is no blockchain, wallet, smart contract, RPC endpoint, faucet, gas, token or cryptocurrency dependency.

## What is published

The signed payload contains only:

- proof batch UUID;
- proof kind and schema version;
- SHA-256 root;
- leaf count;
- hash and tree algorithms.

Fan records, winners, e-mail addresses, tickets, QR data, consent data and event payloads never leave CrowdRelay.

## Failure model

Rekor is always outside the critical path. Draw selection completes and remains auditable even if Rekor is unavailable. The worker uses a database lease, bounded retries and a crash-safe pending-confirmation journal. A successful Rekor upload is never converted into a failed batch merely because the callback to CrowdRelay temporarily failed. A lost POST response is recovered idempotently from Rekor's HTTP 409 `Location` entry, so the deterministic signature is not re-logged as a second proof.

PostgreSQL remains authoritative. Rekor proves that the exact signed commitment was publicly logged no later than the recorded integrated time; it does not decide the winner and does not hold business data.

## Verification

Local draw or audit proofs remain verifiable with:

```bash
python3 scripts/verify-external-proof.py <receipt.json>
```

The Rekor binding, signature and inclusion path are verifiable with:

```bash
node scripts/verify-rekor-proof.mjs <public-proof.json>
```

The verifier checks the canonical Rekor body, signed payload SHA-256, embedded public key, RSA signature, public batch fields and RFC 6962 inclusion path. Offline mode proves that the receipt is internally consistent and bound to the CrowdRelay signer. To establish that the entry is actually present in the public log, use the online check, which compares the stored body with the entry currently returned by Rekor:

```bash
node scripts/verify-rekor-proof.mjs <public-proof.json> --online
```

## Operations

The feature flag is `external_proof_anchoring_enabled`. Keep it disabled until migration 21 is applied and the Rekor worker is healthy. Enabling or disabling it never affects draws, ticketing, admission, mail delivery or consent flows.
