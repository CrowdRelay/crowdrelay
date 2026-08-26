#!/usr/bin/env bash
set -Eeuo pipefail

base="${CONTROL_PLANE_BASE_URL:-https://control.virya.music}"
base="${base%/}"
[[ "$base" == https://* ]] || { echo 'CONTROL_PLANE_SMOKE=FAIL reason=base-url-must-be-https' >&2; exit 2; }
[[ "$base" != *'@'* ]] || { echo 'CONTROL_PLANE_SMOKE=FAIL reason=credentials-in-url' >&2; exit 2; }

request() {
  curl --silent --show-error --location \
    --connect-timeout 4 --max-time 12 \
    --retry 1 --retry-delay 1 --retry-all-errors \
    "$@"
}

root_headers="$(mktemp)"
root_body="$(mktemp)"
unauth_body="$(mktemp)"
auth_body="$(mktemp)"
cleanup() { rm -f "$root_headers" "$root_body" "$unauth_body" "$auth_body"; }
trap cleanup EXIT

# The edge puts Basic Auth in front of the whole plane, the SPA shell
# included, so the root probe has to authenticate. The proof that the edge
# actually rejects anonymous callers is the unauthenticated API probe below.
[[ -n "${CONTROL_PLANE_SMOKE_BASIC_AUTH:-}" ]] || {
  echo 'CONTROL_PLANE_SMOKE=FAIL reason=CONTROL_PLANE_SMOKE_BASIC_AUTH-required' >&2
  exit 2
}
[[ "$CONTROL_PLANE_SMOKE_BASIC_AUTH" == *:* ]] || {
  echo 'CONTROL_PLANE_SMOKE=FAIL reason=invalid-secret-format' >&2
  exit 2
}
root_status="$(request --user "$CONTROL_PLANE_SMOKE_BASIC_AUTH" \
  --dump-header "$root_headers" --output "$root_body" --write-out '%{http_code}' "$base/")"
[[ "$root_status" == 200 ]] || { echo "CONTROL_PLANE_ROOT=FAIL http=${root_status}" >&2; exit 1; }
grep -Fqi '<!doctype html' "$root_body" || grep -Fqi '<html' "$root_body" || {
  echo 'CONTROL_PLANE_ROOT=FAIL reason=html-missing' >&2
  exit 1
}
for header in strict-transport-security x-content-type-options x-frame-options content-security-policy; do
  tr -d '\r' < "$root_headers" | grep -Eqi "^${header}:" || {
    echo "CONTROL_PLANE_ROOT=FAIL reason=missing-security-header header=${header}" >&2
    exit 1
  }
done
echo 'CONTROL_PLANE_ROOT=PASS security_headers=true'

unauth_status="$(request --output "$unauth_body" --write-out '%{http_code}' "$base/api/v1/overview")"
[[ "$unauth_status" == 401 ]] || {
  echo "CONTROL_PLANE_EDGE_AUTH=FAIL unauth_http=${unauth_status}" >&2
  exit 1
}
echo 'CONTROL_PLANE_EDGE_AUTH=PASS unauthenticated=401'

auth_status="$(request \
  --user "$CONTROL_PLANE_SMOKE_BASIC_AUTH" \
  --header 'accept: application/json' \
  --output "$auth_body" --write-out '%{http_code}' \
  "$base/api/v1/overview")"
[[ "$auth_status" == 200 ]] || {
  echo "CONTROL_PLANE_AUTHENTICATED=FAIL http=${auth_status}" >&2
  exit 1
}
jq -e '
  (.tenants | type == "number")
  and (.healthy | type == "number")
  and (.degraded | type == "number")
  and (.stale | type == "number")
  and (.unknown | type == "number")
  and (.runtimeStaleAfterSeconds | type == "number")
' "$auth_body" >/dev/null
echo 'CONTROL_PLANE_AUTHENTICATED=PASS overview_contract=true'

echo 'CONTROL_PLANE_PRODUCTION_SMOKE=PASS'
