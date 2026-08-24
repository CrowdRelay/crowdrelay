#!/usr/bin/env bash
set -Eeuo pipefail

# Rotate the signing secret of one CrowdRelay webhook endpoint.
#
# CrowdRelay stores only a *reference* per endpoint; the value lives in the
# webhook secrets file mounted by the worker. This helper prepares the new
# generation and prints the exact follow-up steps; it never edits production
# state on its own. See docs/SECRET_ROTATION.md for the full overlap window.

usage() {
  cat <<'USAGE'
usage: ops/rotate-webhook-secret.sh --secrets-file PATH --endpoint NAME [--bootstrap-file PATH]

  --secrets-file    Path to the webhook secrets JSON consumed by the worker
                    (deploy/webhook-secrets.production.json or a copy).
  --endpoint        Endpoint name as registered in bootstrap.webhook_endpoints.
  --bootstrap-file  Optional bootstrap file whose signing_secret_ref should be
                    updated alongside the database row.

The script appends a fresh "<endpoint>.v<n>" reference with a generated
32-byte secret, then prints the SQL and deployment steps for the operator.
Nothing is written to the database by this script.
USAGE
}

die() { printf '[rotate-webhook] ERROR: %s\n' "$*" >&2; exit 1; }

SECRETS_FILE=""
ENDPOINT=""
BOOTSTRAP_FILE=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --secrets-file) SECRETS_FILE="${2:-}"; shift 2 ;;
    --endpoint) ENDPOINT="${2:-}"; shift 2 ;;
    --bootstrap-file) BOOTSTRAP_FILE="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[[ -n "$SECRETS_FILE" ]] || { usage >&2; die "--secrets-file is required"; }
[[ -n "$ENDPOINT" ]] || { usage >&2; die "--endpoint is required"; }
command -v python3 >/dev/null || die "python3 is required"
[[ -r "$SECRETS_FILE" ]] || die "secrets file not readable: $SECRETS_FILE"

NEW_REF="${ENDPOINT}.v$(python3 - "$SECRETS_FILE" "$ENDPOINT" <<'PY'
import json, re, sys
path, endpoint = sys.argv[1], sys.argv[2]
document = json.load(open(path))
pattern = re.compile(rf"^{re.escape(endpoint)}\.v(\d+)$")
generation = max((int(m.group(1)) for key in document for m in [pattern.match(key)] if m), default=0)
print(generation + 1)
PY
)"

if [[ -x /dev/urandom ]]; then
  SECRET="$(dd if=/dev/urandom bs=32 count=1 status=none | base64 | tr -d '\n')"
else
  SECRET="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
fi
[[ ${#SECRET} -ge 32 ]] || die "generated secret is too short"

cp "$SECRETS_FILE" "$SECRETS_FILE.bak.$(date +%Y%m%d%H%M%S)"
python3 - "$SECRETS_FILE" "$NEW_REF" "$SECRET" <<'PY'
import json, sys
path, ref, secret = sys.argv[1], sys.argv[2], sys.argv[3]
document = json.load(open(path))
document[ref] = secret
json.dump(document, open(path, "w"), indent=2)
open(path, "a").write("\n")
PY

printf '[rotate-webhook] prepared reference %s\n' "$NEW_REF"
printf '[rotate-webhook] next steps:\n'
cat <<STEPS
  1. Deploy the consumer so it accepts BOTH the previous secret value and the
     new value of $NEW_REF during the overlap window.
  2. Update the endpoint row (psql against the authoritative database):

       UPDATE webhook_endpoints
          SET signing_secret_ref = '$NEW_REF'
        WHERE name = '$ENDPOINT'
          AND active;

  3. Restart the worker so the updated secrets file is loaded:
         crowdrelayctl deploy      # or: docker compose up -d worker
  4. After the consumer fleet has converged (one delivery cycle is enough),
     remove the superseded reference from $SECRETS_FILE and redeploy.
STEPS

if [[ -n "$BOOTSTRAP_FILE" ]]; then
  [[ -r "$BOOTSTRAP_FILE" ]] || die "bootstrap file not readable: $BOOTSTRAP_FILE"
  cp "$BOOTSTRAP_FILE" "$BOOTSTRAP_FILE.bak.$(date +%Y%m%d%H%M%S)"
  python3 - "$BOOTSTRAP_FILE" "$ENDPOINT" "$NEW_REF" <<'PY'
import json, sys
path, endpoint, ref = sys.argv[1], sys.argv[2], sys.argv[3]
document = json.load(open(path))
endpoints = document.get("webhook_endpoints", [])
matches = [entry for entry in endpoints if entry.get("name") == endpoint]
if len(matches) != 1:
    sys.exit(f"expected exactly one bootstrap endpoint named {endpoint}, found {len(matches)}")
matches[0]["signing_secret_ref"] = ref
json.dump(document, open(path, "w"), indent=2)
open(path, "a").write("\n")
PY
  printf '[rotate-webhook] bootstrap file %s now pins %s\n' "$BOOTSTRAP_FILE" "$NEW_REF"
else
  printf '[rotate-webhook] NOTE: update deploy/bootstrap.production.json signing_secret_ref before the next setup run,\n'
  printf '                otherwise bootstrap fails with WebhookSecretReferenceConflict.\n'
fi
