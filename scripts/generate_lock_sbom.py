#!/usr/bin/env python3
"""Generate a deterministic CycloneDX SBOM from Cargo.lock or package-lock.json."""
from __future__ import annotations
import argparse, hashlib, json, re, tomllib
from pathlib import Path
from urllib.parse import quote

HEX64 = re.compile(r"^[0-9a-f]{64}$")

def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()

def cargo_components(path: Path):
    data = tomllib.loads(path.read_text())
    result = []
    for pkg in data.get("package", []):
        name, version = str(pkg["name"]), str(pkg["version"])
        component = {
            "type": "library", "name": name, "version": version,
            "bom-ref": f"pkg:cargo/{quote(name, safe='')}@{quote(version, safe='')}",
            "purl": f"pkg:cargo/{quote(name, safe='')}@{quote(version, safe='')}",
        }
        checksum = pkg.get("checksum")
        if isinstance(checksum, str) and HEX64.fullmatch(checksum):
            component["hashes"] = [{"alg": "SHA-256", "content": checksum}]
        result.append(component)
    return result

def npm_components(path: Path):
    data = json.loads(path.read_text())
    result = []
    seen = set()
    for key, pkg in (data.get("packages") or {}).items():
        if not key or not isinstance(pkg, dict):
            continue
        name = pkg.get("name") or key.rsplit("node_modules/", 1)[-1]
        version = pkg.get("version")
        if not isinstance(name, str) or not isinstance(version, str):
            continue
        identity = (name, version)
        if identity in seen:
            continue
        seen.add(identity)
        escaped = quote(name, safe='@/')
        purl = f"pkg:npm/{escaped}@{quote(version, safe='')}"
        result.append({"type": "library", "name": name, "version": version, "bom-ref": purl, "purl": purl})
    return result

def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("lockfile", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    if not args.lockfile.is_file(): raise SystemExit("lockfile missing")
    if args.lockfile.name == "Cargo.lock":
        components = cargo_components(args.lockfile)
    elif args.lockfile.name == "package-lock.json":
        components = npm_components(args.lockfile)
    else:
        raise SystemExit("supported lockfiles: Cargo.lock, package-lock.json")
    components.sort(key=lambda item: (item["name"], item["version"], item["bom-ref"]))
    sbom = {
        "bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1,
        "metadata": {"properties": [{"name": "virya.lock.sha256", "value": sha256(args.lockfile)}]},
        "components": components,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(sbom, indent=2, sort_keys=True) + "\n")
    print(f"SBOM=PASS components={len(components)} lock={args.lockfile}")
    return 0

if __name__ == "__main__": raise SystemExit(main())
