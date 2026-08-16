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
command -v python3 >/dev/null 2>&1 || {
  echo "LATARNIK_RELEASE_GATE=FAIL reason=python3-missing" >&2
  exit 2
}

echo "==> Rust format / clippy -D warnings / tests"
make check

echo "==> OpenAPI + bootstrap asset contract"
make validate-contract-assets

echo "==> Source/domain contracts"
make contract-tests

echo "==> Runtime/release contracts"
make runtime-contracts

echo "LATARNIK_RELEASE_GATE=PASS schema=57 openapi_paths=228"
