from pathlib import Path
import unittest
ROOT=Path(__file__).resolve().parents[1]
ECOSYSTEM=ROOT.parent
class ReleaseProvenanceContract(unittest.TestCase):
 def test_release_ledger_exposes_content_roots(self):
  app=(ROOT/'crates/crowdrelay-application/src/autopilot/control.rs').read_text()
  runtime=(ROOT/'crates/crowdrelay-infra/src/autopilot/runtime.rs').read_text()
  for token in ('dependency_lock_sha256','artifact_manifest_sha256'):
   self.assertIn(token,app); self.assertIn(token,runtime)
  reporter=(ROOT/'scripts/report-release-component.sh').read_text()
  self.assertIn('DEPENDENCY_LOCK_SHA256',reporter)
  self.assertIn('ARTIFACT_MANIFEST_SHA256',reporter)
 def test_every_deployable_surface_reports_dependency_provenance(self):
  files=[
   ECOSYSTEM/'virya/.github/workflows/build.yml',
   ECOSYSTEM/'synesthesia/.github/workflows/deploy-web.yml',
   ECOSYSTEM/'virya-signal/.github/workflows/android-release-apk.yml',
   ECOSYSTEM/'virya-signal/.github/workflows/mobile-release.yml',
   ECOSYSTEM/'virya-signal/.github/workflows/android-play.yml',
  ]
  for path in files:
   text=path.read_text(); self.assertIn('dependency_lock_sha256',text,str(path)); self.assertIn('artifact_manifest_sha256',text,str(path))
 def test_deploy_package_carries_the_reported_lockfile(self):
  ctl=(ROOT/'crowdrelayctl').read_text()
  package=ctl[ctl.index('package_deploy()'):ctl.index('\nship()')]
  self.assertIn('cp Cargo.lock "$staging/Cargo.lock"',package)
 def test_readiness_is_full_system_not_team_email_only(self):
  verifier=(ROOT/'scripts/verify-production-readiness.py').read_text()
  for token in ('backend-sha-drift','release-components-missing','dependency-lock-missing','artifact-manifest-missing','virya-os-release-receipt.json'):
   self.assertIn(token,verifier)
if __name__=='__main__': unittest.main()
