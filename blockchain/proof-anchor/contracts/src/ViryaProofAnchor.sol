// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {Ownable2Step} from "@openzeppelin/contracts/access/Ownable2Step.sol";

/// @notice Minimal external timestamp/notarization layer for CrowdRelay proofs.
/// @dev CrowdRelay remains authoritative. The contract stores only opaque
///      commitments, never fan data, e-mail addresses, ticket IDs or payloads.
contract ViryaProofAnchor is Ownable2Step {
    uint256 public constant MAX_BATCH_SIZE = 32;

    error UnauthorizedAnchorSigner();
    error InvalidAnchorSigner();
    error InvalidProof();
    error InvalidBatchLength();
    error BatchCommitmentConflict(bytes32 batchKey, bytes32 existingCommitment, bytes32 suppliedCommitment);

    address public anchorSigner;
    // One storage word binds root, leaf count and proof schema together.
    mapping(bytes32 batchKey => bytes32 commitment) public batchCommitments;

    event AnchorSignerChanged(address indexed previousSigner, address indexed newSigner);
    event ProofAnchored(
        bytes32 indexed batchKey, bytes32 indexed root, uint32 leafCount, bytes32 indexed schemaHash
    );
    event ProofReaffirmed(bytes32 indexed batchKey, bytes32 indexed commitment);

    constructor(address initialOwner, address initialAnchorSigner) Ownable(initialOwner) {
        if (initialAnchorSigner == address(0)) revert InvalidAnchorSigner();
        anchorSigner = initialAnchorSigner;
        emit AnchorSignerChanged(address(0), initialAnchorSigner);
    }

    modifier onlyAnchorSigner() {
        if (msg.sender != anchorSigner) revert UnauthorizedAnchorSigner();
        _;
    }

    function setAnchorSigner(address newSigner) external onlyOwner {
        if (newSigner == address(0)) revert InvalidAnchorSigner();
        address previous = anchorSigner;
        anchorSigner = newSigner;
        emit AnchorSignerChanged(previous, newSigner);
    }

    /// @notice Anchors one CrowdRelay proof idempotently.
    function anchor(bytes32 batchKey, bytes32 root, uint32 leafCount, bytes32 schemaHash)
        external
        onlyAnchorSigner
    {
        _anchor(batchKey, root, leafCount, schemaHash);
    }

    /// @notice Anchors multiple proofs in one transaction to minimize RPC,
    ///         nonce management and gas overhead. Arrays stay bounded.
    function anchorMany(
        bytes32[] calldata batchKeys,
        bytes32[] calldata roots,
        uint32[] calldata leafCounts,
        bytes32[] calldata schemaHashes
    ) external onlyAnchorSigner {
        uint256 length = batchKeys.length;
        if (
            length == 0 || length > MAX_BATCH_SIZE || roots.length != length || leafCounts.length != length
                || schemaHashes.length != length
        ) revert InvalidBatchLength();

        for (uint256 index; index < length;) {
            _anchor(batchKeys[index], roots[index], leafCounts[index], schemaHashes[index]);
            unchecked {
                ++index;
            }
        }
    }

    /// @notice Computes the exact single-word value stored for a proof.
    function commitmentFor(bytes32 root, uint32 leafCount, bytes32 schemaHash) public pure returns (bytes32) {
        if (root == bytes32(0) || leafCount == 0 || schemaHash == bytes32(0)) {
            revert InvalidProof();
        }
        return keccak256(abi.encodePacked(root, leafCount, schemaHash));
    }

    function verify(bytes32 batchKey, bytes32 root, uint32 leafCount, bytes32 schemaHash)
        external
        view
        returns (bool)
    {
        if (batchKey == bytes32(0)) return false;
        return batchCommitments[batchKey] == commitmentFor(root, leafCount, schemaHash);
    }

    function _anchor(bytes32 batchKey, bytes32 root, uint32 leafCount, bytes32 schemaHash) private {
        if (batchKey == bytes32(0)) revert InvalidProof();
        bytes32 supplied = commitmentFor(root, leafCount, schemaHash);
        bytes32 existing = batchCommitments[batchKey];
        if (existing == bytes32(0)) {
            batchCommitments[batchKey] = supplied;
            emit ProofAnchored(batchKey, root, leafCount, schemaHash);
            return;
        }
        if (existing != supplied) {
            revert BatchCommitmentConflict(batchKey, existing, supplied);
        }
        emit ProofReaffirmed(batchKey, supplied);
    }
}
