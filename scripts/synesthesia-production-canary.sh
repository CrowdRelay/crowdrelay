#!/usr/bin/env bash
set -Eeuo pipefail

: "${CROWDRELAY_BASE_URL:?CROWDRELAY_BASE_URL is required}"
base="${CROWDRELAY_BASE_URL%/}"

rooms=(
  wave-of-uncertainty
  party-time
  unmasked
  the-calling
  seed-of-doubt
  hybrid
  technophobia
  invaluable
  from-the-ashes
  waves
  rise
)

request_id="$(printf 'synesthesia-production-canary:%s:%s:%s' "$(hostname)" "$(date -u +%Y%m%dT%H%M%SZ)" "$$" | sha256sum | awk '{print $1}')"
attempt_id="canary_$(date -u +%Y%m%dT%H%M%SZ)_$$"
start_payload="$(jq -nc \
  --arg install_id "$request_id" \
  --arg attempt_id "$attempt_id" \
  '{campaign_slug:"virya-synesthesia-album-v1",install_id:$install_id,app_version:"production-canary-v1",attempt_id:$attempt_id,locale:"pl-PL",synthetic:true}')"

start_response="$(curl --fail-with-body --silent --show-error \
  --connect-timeout 4 --max-time 12 \
  --retry 1 --retry-delay 1 --retry-all-errors \
  --header 'content-type: application/json' \
  --header 'accept: application/json' \
  --data "$start_payload" \
  "$base/v1/public/synesthesia/runs")"

run_id="$(jq -er '.run_id | select(type == "string" and length > 0)' <<<"$start_response")"
run_token="$(jq -er '.run_token | select(type == "string" and test("^[0-9a-f]{64}$"))' <<<"$start_response")"
next_room="$(jq -er '.next_room_index | select(type == "number")' <<<"$start_response")"
[[ "$next_room" == 0 ]] || {
  printf 'synesthesia canary expected a fresh run, got next_room_index=%s\n' "$next_room" >&2
  exit 1
}

for index in "${!rooms[@]}"; do
  room="${rooms[$index]}"
  response="$(curl --fail-with-body --silent --show-error \
    --connect-timeout 4 --max-time 12 \
    --retry 1 --retry-delay 1 --retry-all-errors \
    --header 'content-type: application/json' \
    --header 'accept: application/json' \
    --header "Authorization: Bearer ${run_token}" \
    --data "$(jq -nc --argjson room_index "$index" '{room_index:$room_index,client_elapsed_ms:1000}')" \
    "$base/v1/public/synesthesia/runs/$run_id/rooms/$room")"
  expected=$((index + 1))
  actual="$(jq -er '.next_room_index | select(type == "number")' <<<"$response")"
  [[ "$actual" == "$expected" ]] || {
    printf 'synesthesia canary room=%s expected next=%s got=%s\n' "$room" "$expected" "$actual" >&2
    exit 1
  }
done

complete_response="$(curl --fail-with-body --silent --show-error \
  --connect-timeout 4 --max-time 12 \
  --retry 1 --retry-delay 1 --retry-all-errors \
  --header 'content-type: application/json' \
  --header 'accept: application/json' \
  --header "Authorization: Bearer ${run_token}" \
  --data '{"client_total_elapsed_ms":11000}' \
  "$base/v1/public/synesthesia/runs/$run_id/complete")"

jq -e '
  .completed == true
  and .linked_to_fan == false
  and (.handoff_code == null)
  and (.handoff_expires_at == null)
' <<<"$complete_response" >/dev/null

printf 'synesthesia_synthetic_lifecycle=ok run_id=%s rooms=%s handoff=disabled\n' "$run_id" "${#rooms[@]}"
