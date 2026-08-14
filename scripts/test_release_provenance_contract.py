from pathlib import Path
import unittest
ROOT=Path(__file__).resolve().parents[1]
class ReleaseProvenanceContract(unittest.TestCase):
 def test_release_ledger_exposes_content_roots(self):
  control=ROOT/'crates/crowdrelay-application/src/autopilot/control.rs'
  app='\n'.join(path.read_text() for path in (control, control.parent/'control/state_ports.rs', control.parent/'control/runtime_ports.rs'))
  runtime=(ROOT/'crates/crowdrelay-infra/src/autopilot/runtime.rs').read_text()
  for token in ('dependency_lock_sha256','artifact_manifest_sha256'):
   self.assertIn(token,app); self.assertIn(token,runtime)
  reporter=(ROOT/'scripts/report-release-component.sh').read_text()
  self.assertIn('DEPENDENCY_LOCK_SHA256',reporter)
  self.assertIn('ARTIFACT_MANIFEST_SHA256',reporter)
 def test_readiness_requires_provenance_for_every_deployable_surface(self):
  verifier=(ROOT/'scripts/verify-production-readiness.py').read_text()
  self.assertIn('CODE_COMPONENTS = ("crowdrelay-api", "crowdrelay-worker", "virya-www", "synesthesia", "virya-signal")',verifier)
  self.assertIn('MANIFEST_COMPONENTS = ("virya-www", "synesthesia", "virya-signal")',verifier)
  self.assertIn('for key in CODE_COMPONENTS:',verifier)
  self.assertIn('dependency_lock_sha256',verifier)
  self.assertIn('artifact_manifest_sha256',verifier)
 def test_deploy_package_carries_the_reported_lockfile(self):
  ctl=(ROOT/'crowdrelayctl').read_text()
  package=ctl[ctl.index('package_deploy()'):ctl.index('\nship()')]
  self.assertIn('cp Cargo.lock "$staging/Cargo.lock"',package)
 def test_readiness_is_full_system_not_team_email_only(self):
  verifier=(ROOT/'scripts/verify-production-readiness.py').read_text()
  for token in ('backend-sha-drift','release-components-missing','dependency-lock-missing','artifact-manifest-missing','virya-os-release-receipt.json'):
   self.assertIn(token,verifier)
if __name__=='__main__': unittest.main()
