#!/usr/bin/env python3
"""Verify CrowdRelay Merkle inclusion paths and public draw receipts offline."""
from __future__ import annotations

import argparse
import hashlib
import json
import sys
import uuid
from pathlib import Path
from typing import Any


def sha256(*parts: bytes) -> bytes:
    digest = hashlib.sha256()
    for part in parts:
        digest.update(part)
    return digest.digest()


def decode_hash(value: str) -> bytes:
    if len(value) != 64:
        raise ValueError("SHA-256 value must contain exactly 64 hex characters")
    decoded = bytes.fromhex(value)
    if len(decoded) != 32:
        raise ValueError("SHA-256 value must decode to 32 bytes")
    return decoded


def node_hash(left: bytes, right: bytes) -> bytes:
    return sha256(b"\x01", left, right)


def verify_inclusion(document: dict[str, Any]) -> dict[str, Any]:
    current = decode_hash(str(document["leaf_sha256"]))
    for step in document.get("proof", []):
        sibling = decode_hash(str(step["sha256"]))
        side = step.get("side")
        if side == "left":
            current = node_hash(sibling, current)
        elif side == "right":
            current = node_hash(current, sibling)
        else:
            raise ValueError("Merkle step side must be left or right")
    expected = decode_hash(str(document["batch"]["root_sha256"]))
    return {
        "kind": "merkle_inclusion",
        "valid": current == expected,
        "computed_root_sha256": current.hex(),
        "expected_root_sha256": expected.hex(),
        "batch_id": document["batch"]["id"],
        "source_kind": document["source_kind"],
        "source_id": document["source_id"],
    }


def length_prefixed(value: bytes) -> bytes:
    return len(value).to_bytes(8, "big") + value


def signed(value: Any, size: int) -> bytes:
    return int(value).to_bytes(size, "big", signed=True)


def verify_draw(document: dict[str, Any]) -> dict[str, Any]:
    run_id = uuid.UUID(str(document["run_id"]))
    algorithm = str(document["algorithm_version"]).encode()
    revealed_seed = str(document["revealed_seed_hex"]).encode()
    receipt = sha256(
        b"crowdrelay/draw-receipt/v1\x00",
        run_id.bytes,
        length_prefixed(algorithm),
        decode_hash(str(document["seed_hash_sha256"])),
        length_prefixed(revealed_seed),
        signed(document["eligible_count"], 4),
        signed(document["total_entries"], 8),
        signed(document["requested_winners"], 4),
        signed(document["selected_winners"], 4),
        decode_hash(str(document["candidate_snapshot_sha256"])),
        decode_hash(str(document["winner_snapshot_sha256"])),
    )
    expected = decode_hash(str(document["receipt_sha256"]))
    anchor = document.get("anchor") or {}
    return {
        "kind": "draw_receipt",
        "valid": receipt == expected,
        "computed_receipt_sha256": receipt.hex(),
        "expected_receipt_sha256": expected.hex(),
        "draw_slug": document.get("draw_slug"),
        "run_id": str(run_id),
        "anchor_status": anchor.get("status"),
        "transaction_hash": anchor.get("transaction_hash"),
    }


def batch_key(value: str) -> str:
    identifier = uuid.UUID(value)
    return "0x" + (b"\x00" * 16 + identifier.bytes).hex()


def load(path: str) -> dict[str, Any]:
    if path == "-":
        return json.load(sys.stdin)
    return json.loads(Path(path).read_text(encoding="utf-8"))


def self_test() -> None:
    left = sha256(b"\x00", (1).to_bytes(8, "big"), b"a")
    right = sha256(b"\x00", (1).to_bytes(8, "big"), b"b")
    root = node_hash(left, right)
    result = verify_inclusion({
        "leaf_sha256": left.hex(),
        "proof": [{"side": "right", "sha256": right.hex()}],
        "batch": {"root_sha256": root.hex(), "id": str(uuid.uuid4())},
        "source_kind": "audit_event",
        "source_id": str(uuid.uuid4()),
    })
    if not result["valid"]:
        raise RuntimeError("Merkle self-test failed")


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    for command in ("inclusion", "draw"):
        child = sub.add_parser(command)
        child.add_argument("document")
    key = sub.add_parser("batch-key")
    key.add_argument("batch_id")
    sub.add_parser("self-test")
    args = parser.parse_args()
    if args.command == "self-test":
        self_test()
        print(json.dumps({"valid": True, "kind": "self_test"}))
        return 0
    if args.command == "batch-key":
        print(batch_key(args.batch_id))
        return 0
    document = load(args.document)
    result = verify_inclusion(document) if args.command == "inclusion" else verify_draw(document)
    print(json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True))
    return 0 if result["valid"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
