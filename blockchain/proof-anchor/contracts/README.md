# Virya Proof Anchor

Minimal EVM contract for CrowdRelay proof roots. It stores only an opaque
`bytes32` batch key and one commitment binding the root, leaf count and proof schema. Block metadata remains in the transaction receipt and CrowdRelay confirmation record, avoiding additional storage writes per proof. No fan, ticket, consent, winner or
audit payload is written on-chain.

The contract supports `anchorMany` (maximum 32 roots) so the relayer can anchor
up to 16 CrowdRelay batches in one transaction. Replaying the same batch/root/count/schema is
idempotent; changing any committed proof metadata reverts.

## Build and test

```bash
forge install OpenZeppelin/openzeppelin-contracts@v5.6.1 --no-commit
forge install foundry-rs/forge-std --no-commit
forge fmt --check
forge test -vv
```

## Ownership model

- `owner`: cold wallet or multisig; can rotate the hot signer and uses
  `Ownable2Step` for ownership transfer;
- `anchorSigner`: separately funded hot account; can only anchor roots.

Deploy first to Base Sepolia (`84532`) or another test EVM chain. Use a dedicated
RPC in production rather than a shared public endpoint.

Example with a Foundry keystore account:

```bash
forge create src/ViryaProofAnchor.sol:ViryaProofAnchor \
  --rpc-url "$EVM_RPC_URL" \
  --account virya-deployer \
  --constructor-args "$OWNER_ADDRESS" "$ANCHOR_SIGNER_ADDRESS" \
  --broadcast
```

After deployment, verify the source, put the contract address in the relayer
`.env`, and mount the hot signer key through the relayer Docker secret file.
CrowdRelay itself never receives the key.
