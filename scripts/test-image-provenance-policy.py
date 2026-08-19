#!/usr/bin/env python3
from pathlib import Path
import sys
ROOT=Path(__file__).resolve().parents[1]
workflow=(ROOT/'.github/workflows/publish-images.yml').read_text()
resolver=(ROOT/'ops/resolve-image-digests.sh').read_text()
override=(ROOT/'compose.production.digest.yaml').read_text()
bake=(ROOT/'docker-bake.hcl').read_text()
checks={
 # API and worker are baked together in one invocation, so their shared
 # attestation settings are declared once and apply to both targets; the Rekor
 # relayer is still built on its own. Both builders must attest.
 'provenance': workflow.count('provenance: mode=max') >= 2,
 'sbom': workflow.count('sbom: true') >= 2,
 'baked targets attested': 'targets: api,worker' in workflow,
 # Production is Oracle/Ampere. CI already exercises the runtime images as
 # arm64; publication must not silently fall back to the x86_64 runner host.
 'baked arm64 publication': '*.platform=linux/arm64' in workflow,
 'rekor arm64 publication': 'platforms: linux/arm64' in workflow,
 # Digests are recorded per published image either way: bake reports both of
 # its targets in one metadata document, the single build keeps a step output.
 'digest outputs': all(x in workflow for x in ['.api["containerimage.digest"]','.worker["containerimage.digest"]','steps.rekor-image.outputs.digest']),
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
 # The Rekor build labels inline; the baked targets inherit the label from
 # `_common`, whose value is the CROWDRELAY_GIT_SHA the workflow feeds from
 # IMAGE_SHA. Both routes must end at the same build SHA.
 'revision label': workflow.count('org.opencontainers.image.revision=${{ env.IMAGE_SHA }}') >= 1,
 'baked revision label': '"org.opencontainers.image.revision" = CROWDRELAY_GIT_SHA' in bake,
 'baked revision is the build sha': 'CROWDRELAY_GIT_SHA: ${{ env.IMAGE_SHA }}' in workflow,
 'revision is the build sha': 'IMAGE_SHA: ${{ github.event.workflow_run.head_sha }}' in workflow,
}
failed=[k for k,v in checks.items() if not v]
if failed:
 print('IMAGE_PROVENANCE_POLICY=FAIL '+','.join(failed), file=sys.stderr); sys.exit(1)
print(f'IMAGE_PROVENANCE_POLICY=PASS checks={len(checks)} provenance=mode-max sbom=true digest-deploy=available')
