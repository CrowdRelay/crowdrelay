#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

command -v cargo >/dev/null 2>&1 || {
  echo "LATARNIK_RELEASE_GATE=FAIL reason=cargo-missing" >&2
  exit 2
}
command -v node >/dev/null 2>&1 || {
  echo "LATARNIK_RELEASE_GATE=FAIL reason=node-missing" >&2
  exit 2
}
command -v just >/dev/null 2>&1 || {
  echo "LATARNIK_RELEASE_GATE=FAIL reason=just-missing" >&2
  exit 2
}

echo "==> Rust, contract assets, security and schema checks"
just ci

echo "LATARNIK_RELEASE_GATE=PASS schema=64 openapi_paths=242"
