# Rekor production deployment

This rollout is deliberately isolated from ticketing, fan mail, the CrowdRelay outbox and n8n. PostgreSQL remains authoritative. Disabling or stopping the Rekor anchor only pauses external timestamps; local draw and audit proofs continue to exist.

## Preconditions

1. Deploy one validated `sha-*` image tag for `setup`, `api`, `worker` and `rekor-proof-anchor`.
2. Run `setup` successfully so migrations through `0028` are applied.
3. Confirm `https://signal-api.virya.music/v1/health/ready` returns 200.
4. Keep `external_proof_anchoring_enabled=false` until the relayer healthcheck is green.

## One-time files

```bash
cp deploy/rekor-anchor.env.example deploy/rekor-anchor.env
chmod 600 deploy/rekor-anchor.env
./ops/rekor/prepare-secrets.sh
```

`prepare-secrets.sh` writes two API keys without exposing them in process arguments and generates a 3072-bit RSA signing key. Back up the private key outside the repository. Losing it does not invalidate old Rekor entries, but a replacement key will have a new fingerprint.

Required private files:

- `deploy/secrets/crowdrelay_commerce_api_key`
- `deploy/secrets/crowdrelay_admin_api_key`
- `deploy/secrets/rekor_signing_key.pem`

## Deploy and canary

After the normal CrowdRelay deployment has updated the API and applied migrations:

```bash
export CROWDRELAY_IMAGE_TAG=sha-<validated-commit>
./ops/rekor/install-anchor.sh
```

The installer:

1. validates Compose, secret permissions and the RSA key;
2. starts only `crowdrelay-rekor-proof-anchor`, without recreating API, worker or n8n;
3. waits for a real dependency healthcheck against CrowdRelay and Rekor;
4. enables `external_proof_anchoring_enabled`;
5. creates one bounded audit proof batch;
6. waits for confirmation and fetches the public Rekor entry;
7. automatically disables the feature flag if any canary step fails.

## Emergency rollback

```bash
./ops/rekor/rollback-anchor.sh
```

This first disables the CrowdRelay feature flag, then stops the relayer. It does not roll back migrations and does not touch existing proofs, mail flow, ticketing or n8n.

## Operational checks

```bash
docker inspect --format '{{json .State.Health}}' crowdrelay-rekor-proof-anchor
docker logs --tail=100 crowdrelay-rekor-proof-anchor
docker exec crowdrelay-rekor-proof-anchor node -e "fetch('http://127.0.0.1:8081/health/ready').then(async r=>{console.log(await r.text());process.exit(r.ok?0:1)}).catch(()=>process.exit(1))"
```

The readiness payload must show both dependencies as ready and must expose the expected signer fingerprint. The container starts unready; it cannot report a successful READY state before checking both CrowdRelay and Rekor.
