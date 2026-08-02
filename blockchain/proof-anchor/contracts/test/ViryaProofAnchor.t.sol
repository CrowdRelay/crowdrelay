// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {ViryaProofAnchor} from "../src/ViryaProofAnchor.sol";

contract ViryaProofAnchorTest is Test {
    address private owner = makeAddr("owner");
    address private signer = makeAddr("signer");
    address private outsider = makeAddr("outsider");
    ViryaProofAnchor private anchorContract;

    function setUp() public {
        anchorContract = new ViryaProofAnchor(owner, signer);
    }

    function testAnchorAndIdempotentReplay() public {
        bytes32 batch = keccak256("batch");
        bytes32 root = sha256(bytes("root"));
        vm.prank(signer);
        anchorContract.anchor(batch, root, 12, keccak256("crowdrelay/audit_ledger/v1"));
        assertTrue(anchorContract.verify(batch, root, 12, keccak256("crowdrelay/audit_ledger/v1")));
        vm.prank(signer);
        anchorContract.anchor(batch, root, 12, keccak256("crowdrelay/audit_ledger/v1"));
    }

    function testAnchorManyCommitsEveryRootInOneCall() public {
        bytes32[] memory batches = new bytes32[](3);
        bytes32[] memory roots = new bytes32[](3);
        uint32[] memory counts = new uint32[](3);
        bytes32[] memory schemas = new bytes32[](3);
        for (uint256 index; index < batches.length; ++index) {
            batches[index] = keccak256(abi.encode("batch", index));
            roots[index] = sha256(abi.encode("root", index));
            counts[index] = uint32(index + 1);
            schemas[index] = keccak256("crowdrelay/audit_ledger/v1");
        }
        vm.prank(signer);
        anchorContract.anchorMany(batches, roots, counts, schemas);
        for (uint256 index; index < batches.length; ++index) {
            assertTrue(anchorContract.verify(batches[index], roots[index], counts[index], schemas[index]));
        }
    }

    function testRejectsBadBatchShape() public {
        bytes32[] memory batches = new bytes32[](1);
        bytes32[] memory roots = new bytes32[](0);
        uint32[] memory counts = new uint32[](1);
        bytes32[] memory schemas = new bytes32[](1);
        vm.expectRevert(ViryaProofAnchor.InvalidBatchLength.selector);
        vm.prank(signer);
        anchorContract.anchorMany(batches, roots, counts, schemas);
    }

    function testRejectsRootMutationAndUnauthorizedCaller() public {
        bytes32 batch = keccak256("batch");
        vm.prank(signer);
        anchorContract.anchor(batch, sha256(bytes("root-a")), 1, keccak256("schema"));
        vm.expectRevert(ViryaProofAnchor.BatchCommitmentConflict.selector);
        vm.prank(signer);
        anchorContract.anchor(batch, sha256(bytes("root-b")), 1, keccak256("schema"));
        vm.expectRevert(ViryaProofAnchor.BatchCommitmentConflict.selector);
        vm.prank(signer);
        anchorContract.anchor(batch, sha256(bytes("root-a")), 2, keccak256("schema"));
        vm.expectRevert(ViryaProofAnchor.UnauthorizedAnchorSigner.selector);
        vm.prank(outsider);
        anchorContract.anchor(keccak256("other"), sha256(bytes("root")), 1, keccak256("schema"));
    }

    function testRejectsOversizedBatch() public {
        uint256 length = anchorContract.MAX_BATCH_SIZE() + 1;
        bytes32[] memory batches = new bytes32[](length);
        bytes32[] memory roots = new bytes32[](length);
        uint32[] memory counts = new uint32[](length);
        bytes32[] memory schemas = new bytes32[](length);
        for (uint256 index; index < length; ++index) {
            batches[index] = keccak256(abi.encode("batch", index));
            roots[index] = sha256(abi.encode("root", index));
            counts[index] = 1;
            schemas[index] = keccak256("schema");
        }
        vm.expectRevert(ViryaProofAnchor.InvalidBatchLength.selector);
        vm.prank(signer);
        anchorContract.anchorMany(batches, roots, counts, schemas);
    }

    function testOwnershipTransferRequiresAcceptance() public {
        address nextOwner = makeAddr("next-owner");
        vm.prank(owner);
        anchorContract.transferOwnership(nextOwner);
        assertEq(anchorContract.owner(), owner);
        vm.prank(nextOwner);
        anchorContract.acceptOwnership();
        assertEq(anchorContract.owner(), nextOwner);
    }

    function testOwnerRotatesHotSigner() public {
        address nextSigner = makeAddr("next-signer");
        vm.prank(owner);
        anchorContract.setAnchorSigner(nextSigner);
        assertEq(anchorContract.anchorSigner(), nextSigner);
    }
}
