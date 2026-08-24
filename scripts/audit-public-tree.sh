#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

fail() {
  echo "$*" >&2
  exit 1
}

# A tracked file must never also match public ignore rules. This catches files
# that were added before an ignore rule was introduced.
ignored_tracked="$(git ls-files -ci --exclude-standard || true)"
if [[ -n "$ignored_tracked" ]]; then
  echo "Tracked files match public ignore rules:" >&2
  printf '%s\n' "$ignored_tracked" >&2
  exit 1
fi

# The n8n directory is deny-by-default. Only documentation and explicitly
# anonymized examples may be tracked.
unexpected_n8n="$(
  git ls-files 'n8n/**' |
    grep -Ev '^n8n/(README\.md|viryaos-executor-contract\.md|examples/.*\.example\.json)$' || true
)"
if [[ -n "$unexpected_n8n" ]]; then
  echo "Unexpected public n8n files:" >&2
  printf '%s\n' "$unexpected_n8n" >&2
  exit 1
fi

# Even anonymized examples must not contain provider-specific production
# metadata or credential bindings.
if git grep -n -I -E \
  '(discord\.com/api/v[0-9]+/(channels|webhooks)/[0-9]+|graph\.facebook\.com/v[0-9.]+|templateCredsSetupCompleted|"credentials"[[:space:]]*:)' \
  -- 'n8n/examples/*.example.json'
then
  fail "Public n8n examples contain deployment metadata."
fi

# Reject common workstation paths without publishing any operator-specific
# usernames, hostnames, providers, or directory layouts in this audit script.
if git grep -n -I -E \
  '(/Users/[^/[:space:]]+/(Desktop|Documents|Downloads|dev|repos|src)/|/home/[^/[:space:]]+/(Desktop|Documents|Downloads|dev|repos|src)/|[A-Za-z]:\\Users\\[^\\[:space:]]+\\)' \
  -- . ':!scripts/audit-public-tree.sh'
then
  fail "Tracked files contain a workstation-specific absolute path."
fi


# Generic deployment-data checks.
if git grep -n -I -E \
  'DEFAULT_(SELLER|SIGNAL_CITY)_' \
  -- . ':!scripts/audit-public-tree.sh'
then
  fail "Tracked source contains deployment-specific defaults."
fi

if git grep -n -I -E \
  'INSERT[[:space:]]+INTO[[:space:]]+ticket_accounting_profiles' \
  -- 'migrations/*.sql'
then
  fail "A migration contains an operator accounting profile."
fi

if git grep -n -I -E \
  '(ORACLE_(SSH_TARGET|PUBLIC_IPV4|INSTALL_DIR|REMOTE_ARCHIVE)|VIRYA_(DOCKER_NETWORK|POSTGRES_CONTAINER|POSTGRES_ADMIN_USER|CADDY_CONTAINER))' \
  -- . ':!scripts/audit-public-tree.sh'
then
  fail "Tracked files contain deployment-specific infrastructure identifiers."
fi

ipv4_hits="$(
  git grep -n -I -E \
    '(^|[^0-9])([0-9]{1,3}\.){3}[0-9]{1,3}([^0-9]|$)' \
    -- . ':!scripts/audit-public-tree.sh' || true
)"
ipv4_hits="$(
  printf '%s\n' "$ipv4_hits" |
    grep -Ev \
      '(127\.0\.0\.1|0\.0\.0\.0|192\.0\.2\.|198\.51\.100\.|203\.0\.113\.)' || true
)"
if [[ -n "$ipv4_hits" ]]; then
  echo "Tracked files contain a non-example IPv4 address:" >&2
  printf '%s\n' "$ipv4_hits" >&2
  exit 1
fi
