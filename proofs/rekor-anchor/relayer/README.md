# CrowdRelay Rekor proof anchor

This isolated worker publishes deterministic, signed CrowdRelay proof commitments to the Sigstore Rekor transparency log. It has no wallet, contract, RPC, chain ID, gas or cryptocurrency dependency.

PostgreSQL remains authoritative. Draw execution, ticketing and consent do not wait for Rekor. The worker leases an already-created local proof, signs a canonical metadata-only payload, uploads a `rekord` entry and persists Rekor's UUID, log index, integrated time, Signed Entry Timestamp and inclusion proof through CrowdRelay's internal confirmation endpoint.

Only these fields leave CrowdRelay: batch UUID, proof kind, schema version, SHA-256 Merkle root, leaf count and tree algorithm. No fan, winner, e-mail, ticket, QR or consent data is published.

## Generate the signing key

```bash
mkdir -p secrets
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:3072 \
  -out secrets/rekor_signing_key.pem
chmod 600 secrets/rekor_signing_key.pem
```

The public-key SHA-256 fingerprint is exposed on `/health/ready` and stored with every confirmation. Rotate by deploying a new isolated worker key; historical entries remain independently verifiable.

## Validate

```bash
node --check index.mjs
node --test *.test.mjs
docker build -t crowdrelay-rekor-proof-anchor:test .
```

Startup fails closed unless the persisted pending-confirmation directory is writable by the non-root worker.

The upload is idempotent across a lost response: Rekor HTTP 409 conflicts are resolved through the returned same-origin entry `Location`, then confirmed normally.
