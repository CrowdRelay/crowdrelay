#!/usr/bin/env bash
# Provision the Ampere production host in OCI and register its local SSH alias.
#
# Repeatable: resolves the availability domain, subnet and Ubuntu image by name
# instead of pinning OCIDs, refuses to launch a duplicate, and re-prints the SSH
# alias for an instance that already exists.
set -Eeuo pipefail
umask 077

DISPLAY_NAME="${CROWDRELAY_INSTANCE_NAME:-virya-crowdrelay}"
SHAPE="${CROWDRELAY_INSTANCE_SHAPE:-VM.Standard.A1.Flex}"
OCPUS="${CROWDRELAY_INSTANCE_OCPUS:-2}"
MEMORY_GB="${CROWDRELAY_INSTANCE_MEMORY_GB:-12}"
# Comma-separated, tried in order. Always-free Ampere capacity is scarce and
# appears in whichever availability domain happens to have a free host.
AD_SUFFIXES="${CROWDRELAY_INSTANCE_AD_SUFFIX:-AD-2,AD-1,AD-3}"
SUBNET_NAME="${CROWDRELAY_INSTANCE_SUBNET:-subnet-20260724-2110}"
OS_NAME="${CROWDRELAY_INSTANCE_OS:-Canonical Ubuntu}"
OS_VERSION="${CROWDRELAY_INSTANCE_OS_VERSION:-24.04}"
IMAGE_NAME="${CROWDRELAY_INSTANCE_IMAGE_NAME:-}"
SSH_KEY="${CROWDRELAY_INSTANCE_SSH_KEY:-$HOME/.ssh/ssh-key-2026-07-24.key}"
SSH_CONFIG="${CROWDRELAY_SSH_CONFIG:-$HOME/.ssh/config}"
RETRIES="${CROWDRELAY_PROVISION_RETRIES:-0}"
RETRY_SLEEP="${CROWDRELAY_PROVISION_RETRY_SECONDS:-60}"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

note() {
  # stderr: launch_instance runs inside a command substitution that captures stdout.
  printf '==> %s\n' "$*" >&2
}

debug() {
  # Per-attempt resolution chatter. Off by default so a long capacity wait reads
  # as one line per availability domain.
  [[ "${CROWDRELAY_PROVISION_VERBOSE:-0}" == "1" ]] || return 0
  printf '==> %s\n' "$*" >&2
}

require() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

for command in oci jq ssh-keygen; do require "$command"; done
[[ "$OCPUS" =~ ^[1-9][0-9]*$ ]] || fail 'CROWDRELAY_INSTANCE_OCPUS must be a positive integer'
[[ "$MEMORY_GB" =~ ^[1-9][0-9]*$ ]] || fail 'CROWDRELAY_INSTANCE_MEMORY_GB must be a positive integer'
[[ "$RETRIES" =~ ^[0-9]+$ ]] || fail 'CROWDRELAY_PROVISION_RETRIES must be a non-negative integer'
[[ -r "$SSH_KEY.pub" ]] || fail "missing SSH public key: $SSH_KEY.pub"

OCI_CONFIG="${OCI_CLI_CONFIG_FILE:-$HOME/.oci/config}"
[[ -r "$OCI_CONFIG" ]] || fail "missing OCI config: $OCI_CONFIG (run: oci setup config)"
TENANCY="${CROWDRELAY_TENANCY_OCID:-$(
  awk -F= '/^tenancy[[:space:]]*=/ {gsub(/[[:space:]]/, "", $2); print $2; exit}' "$OCI_CONFIG"
)}"
[[ "$TENANCY" == ocid1.tenancy.* ]] || fail "cannot resolve tenancy OCID (got: ${TENANCY:-empty}); run: oci setup config"
COMPARTMENT="${CROWDRELAY_INSTANCE_COMPARTMENT:-$TENANCY}"
REGION="${OCI_CLI_REGION:-$(
  awk -F= '/^region[[:space:]]*=/ {gsub(/[[:space:]]/, "", $2); print $2; exit}' "$OCI_CONFIG"
)}"
[[ -n "$REGION" ]] || fail "cannot resolve region from $OCI_CONFIG"

# launch_instance runs inside a command substitution, so it cannot hand a
# failure reason back through a variable. It leaves the reason here instead.
LAUNCH_ERROR_FILE="$(mktemp)"
trap 'rm -f "$LAUNCH_ERROR_FILE"' EXIT

existing="$(oci compute instance list --compartment-id "$COMPARTMENT" --display-name "$DISPLAY_NAME" \
  --query 'data[?"lifecycle-state"!=`TERMINATED` && "lifecycle-state"!=`TERMINATING`].id | [0]' --raw-output)"

launch_instance() {
  local ad_suffix="$1"
  debug "resolving availability domain ending in $ad_suffix"
  local availability_domain
  availability_domain="$(oci iam availability-domain list --compartment-id "$COMPARTMENT" \
    --query "data[?ends_with(name, \`$ad_suffix\`)].name | [0]" --raw-output)"
  [[ -n "$availability_domain" && "$availability_domain" != null ]] || fail "no availability domain matching $ad_suffix"

  debug "resolving subnet $SUBNET_NAME"
  local subnet_id
  subnet_id="$(oci network subnet list --compartment-id "$COMPARTMENT" --display-name "$SUBNET_NAME" \
    --query 'data[?"lifecycle-state"==`AVAILABLE`].id | [0]' --raw-output)"
  [[ -n "$subnet_id" && "$subnet_id" != null ]] || fail "subnet not found: $SUBNET_NAME"

  debug "resolving $OS_NAME $OS_VERSION image for $SHAPE"
  local image_query='data[0].id' image_id
  if [[ -n "$IMAGE_NAME" ]]; then
    image_query="data[?\"display-name\"==\`$IMAGE_NAME\`].id | [0]"
  fi
  image_id="$(oci compute image list --compartment-id "$COMPARTMENT" \
    --operating-system "$OS_NAME" --operating-system-version "$OS_VERSION" --shape "$SHAPE" \
    --sort-by TIMECREATED --sort-order DESC --query "$image_query" --raw-output)"
  [[ -n "$image_id" && "$image_id" != null ]] || fail "no image found for $OS_NAME $OS_VERSION on $SHAPE"

  # Console equivalents: Authorization header Enabled (IMDS v2 only), restart after
  # infrastructure maintenance, and the non-default Oracle Cloud Agent plugin set.
  local agent_config instance_options availability_config metadata
  agent_config="$(jq -nc '{
    isMonitoringDisabled: false,
    isManagementDisabled: true,
    pluginsConfig: [
      {name: "Vulnerability Scanning", desiredState: "DISABLED"},
      {name: "OS Management Hub Agent", desiredState: "DISABLED"},
      {name: "Management Agent", desiredState: "DISABLED"},
      {name: "Custom Logs Monitoring", desiredState: "ENABLED"},
      {name: "Compute RDMA GPU Monitoring", desiredState: "DISABLED"},
      {name: "Compute Instance Monitoring", desiredState: "ENABLED"},
      {name: "Compute HPC RDMA Auto-Configuration", desiredState: "DISABLED"},
      {name: "Compute HPC RDMA Authentication", desiredState: "DISABLED"},
      {name: "Cloud Guard Workload Protection", desiredState: "ENABLED"},
      {name: "Block Volume Management", desiredState: "DISABLED"},
      {name: "Bastion", desiredState: "DISABLED"}
    ]
  }')"
  instance_options="$(jq -nc '{areLegacyImdsEndpointsDisabled: true}')"
  availability_config="$(jq -nc '{recoveryAction: "RESTORE_INSTANCE"}')"
  metadata="$(jq -nc --rawfile key "$SSH_KEY.pub" '{ssh_authorized_keys: $key}')"

  debug "launching $DISPLAY_NAME ($SHAPE, ${OCPUS} OCPU, ${MEMORY_GB} GB) in $availability_domain"
  local launch_stderr instance_id reason
  launch_stderr="$(mktemp)"
  if instance_id="$(oci compute instance launch \
    --compartment-id "$COMPARTMENT" \
    --availability-domain "$availability_domain" \
    --display-name "$DISPLAY_NAME" \
    --shape "$SHAPE" \
    --shape-config "$(jq -nc --argjson o "$OCPUS" --argjson m "$MEMORY_GB" '{ocpus: $o, memoryInGBs: $m}')" \
    --image-id "$image_id" \
    --subnet-id "$subnet_id" \
    --vnic-display-name "$DISPLAY_NAME" \
    --hostname-label "$DISPLAY_NAME" \
    --assign-public-ip true \
    --metadata "$metadata" \
    --agent-config "$agent_config" \
    --instance-options "$instance_options" \
    --availability-config "$availability_config" \
    --is-pv-encryption-in-transit-enabled true \
    --wait-for-state RUNNING \
    --query 'data.id' --raw-output 2>"$launch_stderr")"; then
    rm -f "$launch_stderr"
    printf '%s' "$instance_id"
    return 0
  fi

  # An OCI ServiceError is a one-line banner followed by a JSON body. Keep just
  # the message so a capacity miss is one readable line instead of 15.
  reason="$(sed -n '/^{/,$p' "$launch_stderr" | jq -r '.message // empty' 2>/dev/null || true)"
  [[ -n "$reason" ]] || reason="$(tr -d '\n' <"$launch_stderr" | tail -c 200)"
  [[ -n "$reason" ]] || reason='launch failed without an error message'
  printf '%s' "$reason" >"$LAUNCH_ERROR_FILE"
  rm -f "$launch_stderr"
  return 1
}

if [[ -n "$existing" && "$existing" != null ]]; then
  note "instance already exists, reusing: $existing"
  INSTANCE_ID="$existing"
else
  IFS=',' read -r -a ad_list <<<"$AD_SUFFIXES"
  (( ${#ad_list[@]} > 0 )) || fail 'CROWDRELAY_INSTANCE_AD_SUFFIX must name at least one availability domain'
  INSTANCE_ID=''
  attempt=0
  while :; do
    for ad in "${ad_list[@]}"; do
      # Status must come from the script, not from a pipeline: piping this into
      # a filter would report the filter's exit code and fake a success.
      if INSTANCE_ID="$(launch_instance "$ad")"; then
        break 2
      fi
      INSTANCE_ID=''
      # "Out of host capacity" is the normal failure for always-free Ampere shapes.
      printf '[%s / %s] %s\n' "$REGION" "$ad" "$(<"$LAUNCH_ERROR_FILE")" >&2
    done
    (( attempt++ < RETRIES )) || fail "launch failed after $attempt round(s) over ${AD_SUFFIXES}"
    printf 'All %s ADs exhausted; retrying in %ss... (round %s/%s)\n' \
      "$REGION" "$RETRY_SLEEP" "$attempt" "$RETRIES" >&2
    sleep "$RETRY_SLEEP"
  done
  [[ -n "$INSTANCE_ID" ]] || fail 'launch reported success without an instance OCID'
fi

PUBLIC_IP="$(oci compute instance list-vnics --instance-id "$INSTANCE_ID" \
  --query 'data[0]."public-ip"' --raw-output)"
[[ -n "$PUBLIC_IP" && "$PUBLIC_IP" != null ]] || fail "instance $INSTANCE_ID has no public IP"

note "instance $INSTANCE_ID is RUNNING at $PUBLIC_IP"

if grep -qiE "^[[:space:]]*Host[[:space:]]+$DISPLAY_NAME[[:space:]]*$" "$SSH_CONFIG" 2>/dev/null; then
  note "SSH alias $DISPLAY_NAME already present in $SSH_CONFIG, leaving it untouched"
  note "verify HostName matches $PUBLIC_IP before deploying"
else
  note "appending SSH alias $DISPLAY_NAME to $SSH_CONFIG"
  cat >>"$SSH_CONFIG" <<EOF

Host $DISPLAY_NAME
    HostName $PUBLIC_IP
    User ubuntu
    IdentityFile $SSH_KEY
    IdentitiesOnly yes
    ServerAliveInterval 30
    ServerAliveCountMax 3
EOF
fi

cat <<EOF

Instance : $DISPLAY_NAME
OCID     : $INSTANCE_ID
Public IP: $PUBLIC_IP
Connect  : ssh $DISPLAY_NAME
Deploy   : CROWDRELAY_DEPLOY_HOST=$DISPLAY_NAME make deploy
EOF
