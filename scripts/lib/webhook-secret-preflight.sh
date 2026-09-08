#!/usr/bin/env bash
# Webhook secret drift preflight.
#
# The CrowdRelay worker signs each webhook delivery with the HMAC-SHA256
# secret keyed under a reference (default `n8n/current`) in a JSON file
# (CROWDRELAY_WEBHOOK_SECRETS_FILE, mounted at
# /run/secrets/crowdrelay-webhooks.json in production). The n8n bridge
# verifies with the raw bytes of a flat file
# (BRIDGE_WEBHOOK_SECRET_FILE, ${EDGE_ROOT}/crowdrelay-n8n-bridge/
# crowdrelay_webhook_secret). The two files live on different services and
# have no automated sync; rotating one without the other produces HTTP 401
# signature_mismatch deliveries that the outbox classifies as Permanent and
# never retries. This helper compares the two byte-for-byte before a deploy
# is allowed to mutate either side.
#
# Source this file, then call `webhook_secret_preflight`. It returns non-zero
# on mismatch (caller's `fail()` should fire) and prints
# `WEBHOOK_SECRET_PREFLIGHT=PASS ref=<ref>` on success. It never prints the
# secret value — only file paths, the ref name, and cmp results.
#
# Environment:
#   CROWDRELAY_WEBHOOK_SECRETS_FILE  worker JSON secrets file (required)
#   BRIDGE_WEBHOOK_SECRET_FILE      bridge flat secret file (required)
#   CROWDRELAY_WEBHOOK_SECRET_REF   secret reference to compare (default n8n/current)
#   CROWDRELAY_WORKER_SECRET_HASH   sha256 of the extracted worker secret bytes;
#                                   used when the worker file is not reachable
#                                   from the edge deploy host. Mutually
#                                   exclusive with CROWDRELAY_WEBHOOK_SECRETS_FILE.
#   CROWDRELAY_WEBHOOK_SECRET_PREFLIGHT  set to `skip` to bypass (emergency only).
#
# This helper is safe to source: it defines only `webhook_secret_preflight`.

webhook_secret_preflight() {
  # Escape hatch for genuine emergencies (bridge not yet deployed, etc.).
  # Default is fail-closed: a deploy that cannot verify the secret pair is
  # exactly the state that produced the 2026-09 incident.
  if [[ "${CROWDRELAY_WEBHOOK_SECRET_PREFLIGHT:-}" == "skip" ]]; then
    printf 'WEBHOOK_SECRET_PREFLIGHT=SKIP reason=operator-override\n'
    return 0
  fi

  local ref="${CROWDRELAY_WEBHOOK_SECRET_REF:-n8n/current}"
  local worker_file="${CROWDRELAY_WEBHOOK_SECRETS_FILE:-}"
  local bridge_file="${BRIDGE_WEBHOOK_SECRET_FILE:-}"
  local worker_hash="${CROWDRELAY_WORKER_SECRET_HASH:-}"

  command -v python3 >/dev/null 2>&1 || { printf 'ERROR: python3 is required for webhook secret preflight\n' >&2; return 1; }
  command -v cmp >/dev/null 2>&1 || { printf 'ERROR: cmp is required for webhook secret preflight\n' >&2; return 1; }

  # Resolve the worker secret bytes. Two modes:
  #   1. Direct file access: extract the named ref from the JSON file.
  #      This mirrors SecretValue::new(value.into_bytes()) in main.rs.
  #   2. Hash-only mode: the edge deploy host may not see the worker file.
  #      The operator supplies CROWDRELAY_WORKER_SECRET_HASH (sha256 of the
  #      extracted bytes) and we compare hashes instead of bytes.
  local worker_bytes_file worker_hash_computed

  if [[ -n "$worker_file" && -n "$worker_hash" ]]; then
    printf 'ERROR: set only one of CROWDRELAY_WEBHOOK_SECRETS_FILE or CROWDRELAY_WORKER_SECRET_HASH, not both\n' >&2
    return 1
  fi

  if [[ -z "$worker_file" && -z "$worker_hash" ]]; then
    printf 'ERROR: webhook secret preflight needs CROWDRELAY_WEBHOOK_SECRETS_FILE or CROWDRELAY_WORKER_SECRET_HASH\n' >&2
    return 1
  fi

  if [[ -z "$bridge_file" ]]; then
    printf 'ERROR: BRIDGE_WEBHOOK_SECRET_FILE is required for webhook secret preflight\n' >&2
    return 1
  fi

  if [[ -n "$worker_file" ]]; then
    [[ -f "$worker_file" && ! -L "$worker_file" ]] || { printf 'ERROR: worker webhook secrets file missing or unsafe: %s\n' "$worker_file" >&2; return 1; }
    # Extract the named ref's value bytes. Fails closed on a missing key,
    # a non-string value, or invalid JSON — none of which should ever ship.
    worker_bytes_file="$(mktemp)" || { printf 'ERROR: could not create temp file for worker secret extraction\n' >&2; return 1; }
    if ! python3 - "$worker_file" "$ref" "$worker_bytes_file" <<'PY' >&2
import json, sys
worker_file, ref, out = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    with open(worker_file, "rb") as fh:
        doc = json.load(fh)
except (OSError, json.JSONDecodeError) as exc:
    sys.stderr.write(f"ERROR: cannot read worker secrets file {worker_file!r}: {exc}\n")
    sys.exit(1)
if ref not in doc:
    sys.stderr.write(f"ERROR: secret reference {ref!r} not found in {worker_file!r}\n")
    sys.exit(1)
value = doc[ref]
if not isinstance(value, str):
    sys.stderr.write(f"ERROR: secret value for {ref!r} is not a string in {worker_file!r}\n")
    sys.exit(1)
with open(out, "wb") as fh:
    fh.write(value.encode("utf-8"))
PY
    then
      rm -f "$worker_bytes_file"
      return 1
    fi
  fi

  # Resolve the bridge secret bytes. The bridge reads this file raw with
  # fs.readFileSync and uses the bytes directly as the HMAC key, so a trailing
  # newline or BOM is a real drift, not a cosmetic one.
  [[ -f "$bridge_file" && ! -L "$bridge_file" ]] || { printf 'ERROR: bridge webhook secret file missing or unsafe: %s\n' "$bridge_file" >&2; [[ -n "$worker_bytes_file" ]] && rm -f "$worker_bytes_file"; return 1; }

  if [[ -n "$worker_file" ]]; then
    # Byte comparison — catches trailing newline, BOM, whitespace, any drift.
    if ! cmp -s "$worker_bytes_file" "$bridge_file"; then
      rm -f "$worker_bytes_file"
      printf 'ERROR: webhook secret drift detected for ref %s\n' "$ref" >&2
      printf '       worker file: %s\n' "$worker_file" >&2
      printf '       bridge file: %s\n' "$bridge_file" >&2
      printf '       The two files must hold identical bytes. A trailing newline in the\n' >&2
      printf '       bridge file is the most common cause: write it with\n' >&2
      printf '         printf %%s "$SECRET" > "$BRIDGE_WEBHOOK_SECRET_FILE"\n' >&2
      printf '       not `echo > file`. Rotate both files with the same bytes, then redeploy.\n' >&2
      return 1
    fi
    rm -f "$worker_bytes_file"
  else
    # Hash-only mode: compare the operator-supplied worker hash to the
    # bridge file hash. The operator must hash the *extracted* worker bytes
    # (the JSON value, UTF-8 encoded, no trailing newline), not the whole
    # JSON file.
    worker_hash_computed="$(sha256sum "$bridge_file" | awk '{print $1}')"
    if [[ "$worker_hash_computed" != "$worker_hash" ]]; then
      printf 'ERROR: webhook secret drift detected for ref %s (hash mode)\n' "$ref" >&2
      printf '       bridge file: %s\n' "$bridge_file" >&2
      printf '       bridge sha256: %s\n' "$worker_hash_computed" >&2
      printf '       expected (CROWDRELAY_WORKER_SECRET_HASH): %s\n' "$worker_hash" >&2
      printf '       The expected hash must be the sha256 of the extracted worker secret\n' >&2
      printf '       bytes (JSON value, UTF-8, no trailing newline), not the whole JSON file.\n' >&2
      return 1
    fi
  fi

  printf 'WEBHOOK_SECRET_PREFLIGHT=PASS ref=%s\n' "$ref"
}
