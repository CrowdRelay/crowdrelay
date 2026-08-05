#!/usr/bin/env bash
set -euo pipefail

container="${CROWDRELAY_API_CONTAINER:-crowdrelay-api}"
actor_id="${CROWDRELAY_INVENTORY_ACTOR_ID:-virya-ops}"

command -v docker >/dev/null 2>&1 || { echo "ERROR: docker is required" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "ERROR: python3 is required" >&2; exit 1; }

docker inspect "$container" >/dev/null 2>&1 || {
  echo "ERROR: API container '$container' is not running or does not exist" >&2
  exit 1
}

api_request() {
  local method="$1"
  local path="$2"
  local body="${3:-}"
  docker exec \
    -e REQUEST_METHOD="$method" \
    -e REQUEST_PATH="$path" \
    -e REQUEST_BODY="$body" \
    "$container" sh -eu -c '
      : "${CROWDRELAY_STAFF_API_KEY:?CROWDRELAY_STAFF_API_KEY missing in API container}"
      if [ -n "${REQUEST_BODY}" ]; then
        curl -fsS --connect-timeout 3 --max-time 15 \
          -X "${REQUEST_METHOD}" \
          -H "Accept: application/json" \
          -H "Authorization: Bearer ${CROWDRELAY_STAFF_API_KEY}" \
          -H "Content-Type: application/json" \
          --data "${REQUEST_BODY}" \
          "http://127.0.0.1:8080${REQUEST_PATH}"
      else
        curl -fsS --connect-timeout 3 --max-time 15 \
          -X "${REQUEST_METHOD}" \
          -H "Accept: application/json" \
          -H "Authorization: Bearer ${CROWDRELAY_STAFF_API_KEY}" \
          "http://127.0.0.1:8080${REQUEST_PATH}"
      fi
    '
}

print_state() {
  python3 -c '
import json, sys
state = json.load(sys.stdin)
print(f"status={state.get('"'"'status'"'"')} ready={state.get('"'"'ready'"'"')} fully_enabled={state.get('"'"'fully_enabled'"'"')}")
print(f"counted={state.get('"'"'counted_active_variants'"'"')}/{state.get('"'"'total_active_variants'"'"')}")
if state.get("blockers"):
    print("blockers=" + ",".join(state["blockers"]))
if state.get("missing_skus"):
    print("missing_skus=" + ",".join(state["missing_skus"]))
'
}

state="$(api_request GET /v1/staff/merch/inventory/activation)"
printf '%s\n' "$state" | print_state

read -r ready fully_enabled can_mark_ready < <(
  printf '%s\n' "$state" | python3 -c '
import json, sys
state = json.load(sys.stdin)
print(
    str(bool(state.get("ready"))).lower(),
    str(bool(state.get("fully_enabled"))).lower(),
    str(bool(state.get("can_mark_ready"))).lower(),
)
'
)

if [[ "$ready" == "true" && "$fully_enabled" == "true" ]]; then
  echo "Inventory is already active. Verifying public catalog..."
elif [[ "$can_mark_ready" != "true" ]]; then
  echo "NOT ACTIVATED: complete the exact stocktake for every missing SKU, then rerun this script." >&2
  exit 2
else
  payload="$(ACTOR_ID="$actor_id" python3 -c '
import json, os
print(json.dumps({"actor_id": os.environ["ACTOR_ID"]}, separators=(",", ":")))
')"
  state="$(api_request POST /v1/staff/merch/inventory/ready "$payload")"
  printf '%s\n' "$state" | print_state
  printf '%s\n' "$state" | python3 -c '
import json, sys
state = json.load(sys.stdin)
if not (state.get("ready") and state.get("fully_enabled")):
    raise SystemExit("ERROR: READY endpoint did not fully enable inventory")
'
fi

catalog="$(api_request GET /v1/public/merch/catalog)"
printf '%s\n' "$catalog" | python3 -c '
import json, sys
catalog = json.load(sys.stdin)
products = catalog.get("products")
if not isinstance(products, list) or not products:
    raise SystemExit("ERROR: public merch catalog is empty after activation")
variants = sum(len(product.get("variants") or []) for product in products)
print(f"OK: public merch catalog active — products={len(products)} variants={variants}")
'
