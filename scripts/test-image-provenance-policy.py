#!/usr/bin/env python3
from pathlib import Path
import sys
ROOT=Path(__file__).resolve().parents[1]
workflow=(ROOT/'.github/workflows/publish-images.yml').read_text()
resolver=(ROOT/'ops/resolve-image-digests.sh').read_text()
override=(ROOT/'compose.production.digest.yaml').read_text()
checks={
 'provenance': workflow.count('provenance: mode=max') >= 3,
 'sbom': workflow.count('sbom: true') >= 3,
 'digest outputs': all(x in workflow for x in ['steps.api-image.outputs.digest','steps.worker-image.outputs.digest','steps.rekor-image.outputs.digest']),
 'digest manifest': 'crowdrelay-image-digests-${{ env.IMAGE_SHA }}' in workflow,
 'full sha gate': '^sha-[0-9a-f]{40}$' in resolver,
 'resolver digest': 'RepoDigests' in resolver and '@sha256:' in resolver,
 'compose api digest': 'CROWDRELAY_API_IMAGE_REF' in override,
 'compose worker digest': 'CROWDRELAY_WORKER_IMAGE_REF' in override,
 # Cross-repo contract with crowdrelay-control-plane/deploy/provisioner.py.
 # An OCI tag is mutable, so the tenant provisioner refuses to start an image
 # whose org.opencontainers.image.revision label does not equal the git SHA of
 # the release it was asked to deploy. Every published image must carry that
 # label, and it must be the build SHA rather than a second version source.
 'revision label': workflow.count('org.opencontainers.image.revision=${{ env.IMAGE_SHA }}') >= 3,
 'revision is the build sha': 'IMAGE_SHA: ${{ github.event.workflow_run.head_sha }}' in workflow,
}
failed=[k for k,v in checks.items() if not v]
if failed:
 print('IMAGE_PROVENANCE_POLICY=FAIL '+','.join(failed), file=sys.stderr); sys.exit(1)
print(f'IMAGE_PROVENANCE_POLICY=PASS checks={len(checks)} provenance=mode-max sbom=true digest-deploy=available')
