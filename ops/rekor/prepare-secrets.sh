#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
secret_dir="${CROWDRELAY_SECRET_DIR:-$repo_root/deploy/secrets}"
mkdir -p "$secret_dir"
chmod 700 "$secret_dir"

write_secret() {
  local name="$1" value="${2:-}" target="$secret_dir/$1"
  if [[ -z "$value" ]]; then
    read -r -s -p "$name: " value
    printf '\n' >&2
  fi
  if (( ${#value} < 24 || ${#value} > 512 )); then
    printf 'Invalid length for %s\n' "$name" >&2
    return 1
  fi
  printf '%s\n' "$value" > "$target"
  chmod 600 "$target"
}

write_secret crowdrelay_commerce_api_key "${CROWDRELAY_COMMERCE_API_KEY:-}"
write_secret crowdrelay_admin_api_key "${CROWDRELAY_ADMIN_API_KEY:-}"
unset CROWDRELAY_COMMERCE_API_KEY CROWDRELAY_ADMIN_API_KEY || true

key="$secret_dir/rekor_signing_key.pem"
if [[ ! -s "$key" ]]; then
  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:3072 -out "$key"
fi
chmod 600 "$key"
openssl pkey -in "$key" -check -noout >/dev/null
fingerprint="$(openssl pkey -in "$key" -pubout -outform DER 2>/dev/null | openssl dgst -sha256 | awk '{print $2}')"
printf 'Rekor signer fingerprint: sha256:%s\n' "$fingerprint"
printf 'Secrets ready in %s\n' "$secret_dir"
