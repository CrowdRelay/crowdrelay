import unittest
from pathlib import Path
from rust_source_tree import read_rust_module
ROOT = Path(__file__).resolve().parents[1]
class SourceLayoutContracts(unittest.TestCase):
    def test_modern_layout(self):
        self.assertFalse(any((ROOT / 'crates').rglob('mod.rs')))
        self.assertTrue((ROOT / 'crates/crowdrelay-worker/src/outbox.rs').is_file())
    def test_partitioned_contracts(self):
        specs={
          'crates/crowdrelay-api/src/area.rs':('public_drops',5),
          'crates/crowdrelay-api/src/ticketing.rs':('reserve_order',6),
          'crates/crowdrelay-api/src/commerce.rs':('reserve_inventory',5),
          'crates/crowdrelay-worker/src/bootstrap.rs':('bootstrap_admission_access',5),
        }
        for rel,(contract,count) in specs.items():
            entry=(ROOT/rel).read_text()
            self.assertEqual(entry.count('include!("'),count,rel)
            self.assertIn(contract,read_rust_module(ROOT,rel),rel)
if __name__=='__main__': unittest.main()
