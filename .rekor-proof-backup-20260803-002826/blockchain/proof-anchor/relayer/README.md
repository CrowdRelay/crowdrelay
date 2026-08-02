# CrowdRelay proof anchor relayer

A deliberately isolated, single-nonce process. It leases up to 16 queued proof
roots, sends one bounded `anchorMany` EVM transaction, waits for confirmations
and writes the receipt back to CrowdRelay concurrently.

The relayer writes a tiny pending-transaction journal to `/data` immediately
after transaction submission. On restart it resumes receipt waiting and replays
idempotent CrowdRelay confirmations with the original lease identity before
claiming new work. This avoids duplicate transactions after process or network
failures.

The private key never enters the CrowdRelay API/worker, Virya, mobile, n8n or
PostgreSQL. The service is optional and read-only apart from its state volume.
Keep `blockchain_anchoring_enabled=false` until the contract is deployed and the
hot wallet is funded. Private keys and the CrowdRelay service token are mounted
as Docker secret files; they are never placed in container environment variables.

```bash
cp .env.example .env
mkdir -p secrets state
printf '%s' "$CROWDRELAY_COMMERCE_API_KEY" > secrets/crowdrelay_commerce_api_key
printf '%s' "$EVM_ANCHOR_PRIVATE_KEY" > secrets/evm_anchor_private_key
printf '%s' "$EVM_RPC_URL" > secrets/evm_rpc_url
chmod 600 .env secrets/*
docker compose config
docker compose up -d --build
docker compose ps
curl -fsS http://127.0.0.1:8081/health
```


Required secret files (mode `0600`): `secrets/crowdrelay_commerce_api_key`, `secrets/evm_anchor_private_key`, and `secrets/evm_rpc_url`. Startup verifies chain ID, contract bytecode, hot signer identity, and batch capacity before the health endpoint becomes available.
