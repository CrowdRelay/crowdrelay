#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
CANONICAL="$ROOT_DIR/scripts/deploy-production-exact.sh"
DEST="$HOME/.config/fish/functions/deploy-crowdrelay.fish"

[[ -x "$CANONICAL" ]] || {
  echo "ERROR: canonical deploy script is missing or not executable: $CANONICAL" >&2
  exit 1
}

mkdir -p "$(dirname "$DEST")"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
if [[ -f "$DEST" ]]; then
  backup="${DEST}.backup-canonical-${stamp}"
  cp -p "$DEST" "$backup"
  echo "BACKUP=$backup"
fi

repo_escaped="${ROOT_DIR//\\/\\\\}"
repo_escaped="${repo_escaped//\"/\\\"}"

cat > "$DEST" <<EOF
function deploy-crowdrelay --description 'Deploy CrowdRelay via canonical tracked orchestrator'
    set -l repo "$repo_escaped"

    if not test -x "\$repo/scripts/deploy-production-exact.sh"
        echo "ERROR: canonical CrowdRelay deploy script missing: \$repo/scripts/deploy-production-exact.sh" >&2
        return 1
    end

    command bash "\$repo/scripts/deploy-production-exact.sh" \$argv
end
EOF

if command -v fish >/dev/null 2>&1; then
  fish -n "$DEST"
  echo "FISH_SYNTAX=PASS"
else
  echo "FISH_SYNTAX=SKIPPED fish=missing"
fi

if grep -Fq '.local/libexec' "$DEST"; then
  echo "ERROR: installed wrapper still references legacy libexec helpers" >&2
  exit 1
fi

echo "INSTALLED=$DEST"
echo "LEGACY_HELPERS=UNREFERENCED preserved=true"
echo "CROWDRELAY_CANONICAL_WRAPPER=PASS tracked-orchestrator=true"
echo "NEXT=source $DEST"
