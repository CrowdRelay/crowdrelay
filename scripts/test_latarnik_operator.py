#!/usr/bin/env python3
from __future__ import annotations

import contextlib
import importlib.util
import io
import os
import stat
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/latarnik_operator.py"
spec = importlib.util.spec_from_file_location("latarnik_operator", SCRIPT)
assert spec and spec.loader
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class LatarnikOperatorContract(unittest.TestCase):
    def test_default_endpoint_and_security_names_are_explicit(self) -> None:
        source = SCRIPT.read_text()
        self.assertIn("https://signal-api.virya.music/v1", source)
        self.assertIn('CROWDRELAY_ADMIN_API_KEY', source)
        self.assertIn('CROWDRELAY_COMMERCE_API_KEY', source)
        self.assertNotIn("CROWDRELAY_ADMIN_API_KEY=", source)

    def test_sensitive_output_is_owner_only(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "invites.json"
            module.secure_dump({"inviteUrl": "https://example.invalid/secret"}, str(path))
            mode = stat.S_IMODE(os.stat(path).st_mode)
            self.assertEqual(mode, 0o600)
            self.assertIn("inviteUrl", path.read_text())

    def test_batch_limit_is_200(self) -> None:
        parser = module.build_parser()
        args = parser.parse_args(["invite-batch", "--top", "200"])
        self.assertEqual(args.top, 200)
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                parser.parse_args(["invite-batch", "--top", "201"])

    def test_emission_is_separate_commerce_namespace(self) -> None:
        source = SCRIPT.read_text()
        self.assertIn('internal_request(', source)
        self.assertIn('"internal/beacon/notifications/emit-due"', source)


if __name__ == "__main__":
    unittest.main()
