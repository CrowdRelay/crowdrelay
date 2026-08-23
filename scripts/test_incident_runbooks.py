from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
RUNBOOK = ROOT / 'docs/operations/VIRYA_OS_INCIDENT_RUNBOOKS.md'


class RunbookContract(unittest.TestCase):
    def test_critical_failure_modes_and_invariants_are_documented(self):
        if not RUNBOOK.is_file():
            self.skipTest('incident runbook is intentionally not tracked in docs')
        text = RUNBOOK.read_text(encoding='utf-8')
        for token in ['team.email', 'claim is `claimed`', 'delayed `failed`', 'idempotency', 'n8n attestation', 'Accounting finalization', 'Synesthesia', 'production readiness', 'release receipt']:
            self.assertIn(token, text)
        self.assertIn('must never downgrade', text)


if __name__ == '__main__':
    unittest.main()
