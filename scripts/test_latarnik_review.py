#!/usr/bin/env python3
"""The review tool must never send a letter nobody read and confirmed."""
from __future__ import annotations

import importlib.util
import io
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/latarnik_review.py"
spec = importlib.util.spec_from_file_location("latarnik_review", SCRIPT)
assert spec and spec.loader
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def preview(email="anna@example.test", name="Anna"):
    return {
        "recipient_name": name,
        "recipient_email": email,
        "role": "promoter",
        "city": "Wroclaw",
        "reason": "koncert w Wroclaw",
        "hook_fact": "recenzja plyty X w audycji Y",
        "hook_source_url": "https://example.test/audycje/y",
        "hook_observed_on": "2026-09-20",
        "subject": "Virya: terminy zanim wyjda",
        "body": "Czesc Anna,\n\nGramy w Wroclaw.\n",
    }


def rows(*ids):
    return [{"beacon_id": i, "display_name": f"P{i}", "relationship_score": 70} for i in ids]


class Harness:
    def __init__(self, answers, previews=None):
        self.answers = list(answers)
        self.previews = previews or {}
        self.sent = []
        self.output = []

    def get_preview(self, beacon_id):
        return self.previews.get(beacon_id, preview())

    def send(self, beacon_id):
        self.sent.append(beacon_id)
        return {"outcome": "Queued"}

    def ask(self, prompt):
        return self.answers.pop(0)

    def run(self, ids, limit=10):
        return module.review_loop(
            rows(*ids),
            limit=limit,
            get_preview=self.get_preview,
            send=self.send,
            ask=self.ask,
            out=self.output.append,
        )


class ReviewNeverSendsUnconfirmed(unittest.TestCase):
    def test_sends_only_after_s_and_tak(self):
        h = Harness(["s", "tak"])
        counts = h.run(["a"])
        self.assertEqual(h.sent, ["a"])
        self.assertEqual(counts["sent"], 1)

    def test_s_without_tak_sends_nothing(self):
        for confirm in ("", "nie", "t", "TAK?"):
            h = Harness(["s", confirm])
            counts = h.run(["a"])
            self.assertEqual(h.sent, [], confirm)
            self.assertEqual(counts["skipped"], 1)

    def test_anything_but_s_sends_nothing(self):
        for answer in ("", "n", "y", "yes", "tak", "send", "S "[:0]):
            h = Harness([answer])
            h.run(["a"])
            self.assertEqual(h.sent, [], repr(answer))

    def test_q_stops_the_session(self):
        h = Harness(["q"])
        h.run(["a", "b", "c"])
        self.assertEqual(h.sent, [])
        self.assertEqual(len(h.answers), 0)

    def test_the_letter_is_shown_before_the_question(self):
        h = Harness(["n"])
        h.run(["a"])
        text = "\n".join(h.output)
        self.assertIn("Gramy w Wroclaw.", text)
        self.assertIn("anna@example.test", text)

    def test_a_refused_preview_is_skipped_without_asking(self):
        h = Harness([], previews={"a": {"refused": "asked once already"}})
        counts = h.run(["a"])
        self.assertEqual(h.sent, [])
        self.assertEqual(counts["refused"], 1)
        self.assertIn("asked once already", "\n".join(h.output))

    def test_a_refused_send_is_not_counted_as_sent(self):
        h = Harness(["s", "tak"])
        h.send = lambda beacon_id: {"refused": "recently contacted"}  # type: ignore[method-assign]
        counts = h.run(["a"])
        self.assertEqual(counts["sent"], 0)
        self.assertEqual(counts["refused"], 1)

    def test_a_session_is_capped(self):
        h = Harness(["n"] * 40)
        h.run([str(i) for i in range(40)], limit=3)
        self.assertEqual(len(h.answers), 40 - 3)

    def test_the_cap_cannot_be_raised_past_the_maximum(self):
        h = Harness([])
        for bad in (0, -1, module.MAX_PER_SESSION + 1):
            with self.assertRaises(module.OperatorError):
                h.run(["a"], limit=bad)


class Selection(unittest.TestCase):
    def test_only_invitable_rows_strongest_relationship_first(self):
        review = {
            "contacts": [
                {"beacon_id": "x", "invitable": False, "relationship_score": 99, "display_name": "X"},
                {"beacon_id": "b", "invitable": True, "relationship_score": 60, "display_name": "B"},
                {"beacon_id": "a", "invitable": True, "relationship_score": 80, "display_name": "A"},
            ]
        }
        self.assertEqual([r["beacon_id"] for r in module.invitable(review)], ["a", "b"])

    def test_render_shows_the_whole_letter(self):
        text = module.render_letter(preview())
        for part in ("Anna <anna@example.test>", "koncert w Wroclaw", "Virya: terminy", "Gramy w Wroclaw."):
            self.assertIn(part, text)

    def test_render_shows_what_the_band_read_and_where(self):
        text = module.render_letter(preview())
        for part in ("recenzja plyty X w audycji Y", "https://example.test/audycje/y", "2026-09-20"):
            self.assertIn(part, text)

    def test_the_research_queue_is_warm_unread_people_only(self):
        review = {
            "contacts": [
                {"beacon_id": "ready", "invitable": True, "has_research": True,
                 "relationship_score": 90, "display_name": "R"},
                {"beacon_id": "cold", "invitable": False, "has_research": False,
                 "hold_reason": "no relationship on record yet", "relationship_score": 95,
                 "display_name": "C"},
                {"beacon_id": "busy", "invitable": False, "has_research": False,
                 "hold_reason": "the band wrote to them recently", "relationship_score": 80,
                 "display_name": "B"},
                {"beacon_id": "b", "invitable": False, "has_research": False,
                 "hold_reason": "not read yet - we write to people", "relationship_score": 60,
                 "display_name": "B2"},
                {"beacon_id": "a", "invitable": False, "has_research": False,
                 "hold_reason": "not read yet - we write to people", "relationship_score": 75,
                 "display_name": "A"},
            ]
        }
        # Cold and recently-contacted people are held for those reasons; spending
        # research on them would be spending it on someone who cannot be asked.
        self.assertEqual([r["beacon_id"] for r in module.needs_research(review)], ["a", "b"])

    def test_note_arguments_are_all_required(self):
        parser = module.build_parser()
        with self.assertRaises(SystemExit), redirect_stdout(io.StringIO()), mock.patch(
            "sys.stderr", new=io.StringIO()
        ):
            parser.parse_args(["note", "some-id", "--fact", "x"])
        args = parser.parse_args(
            ["note", "some-id", "--fact", "f", "--source-url", "https://e.test/x", "--observed-on", "2026-09-20"]
        )
        self.assertEqual(args.language, "pl")


class Entry(unittest.TestCase):
    def test_review_refuses_without_a_terminal(self):
        with mock.patch.object(module.sys.stdin, "isatty", return_value=False):
            with self.assertRaises(module.OperatorError):
                module.command_review(module.build_parser().parse_args(["review"]))

    def test_there_is_no_flag_that_skips_the_questions(self):
        parser = module.build_parser()
        for flag in ("--yes", "--force", "--all", "--no-confirm", "-y"):
            with self.assertRaises(SystemExit), redirect_stdout(io.StringIO()), mock.patch(
                "sys.stderr", new=io.StringIO()
            ):
                parser.parse_args(["review", flag])

    def test_the_send_uses_one_stable_key_per_person(self):
        source = SCRIPT.read_text()
        self.assertIn('idempotency_key=f"latarnik-{beacon_id}"', source)

    def test_the_key_is_never_printed(self):
        source = SCRIPT.read_text()
        self.assertNotIn("print(env(", source)


class ResearchLoop(unittest.TestCase):
    def test_it_sends_each_person_up_to_the_limit_and_reports_every_answer(self):
        asked, out = [], []

        def request(beacon_id):
            asked.append(beacon_id)
            return {"queued": "t"} if beacon_id != "b" else {"refused": "read recently"}

        rows = [{"beacon_id": i, "display_name": i.upper()} for i in ["a", "b", "c", "d"]]
        counts = module.research_loop(rows, limit=3, request=request, out=out.append)
        self.assertEqual(asked, ["a", "b", "c"])
        self.assertEqual(counts, {"queued": 2, "refused": 1})
        self.assertIn("read recently", "\n".join(out))

    def test_the_limit_is_bounded(self):
        for bad in (0, -1, module.MAX_PER_SESSION + 1):
            with self.assertRaises(module.OperatorError):
                module.research_loop([], limit=bad, request=lambda _: None, out=lambda _: None)

    def test_research_never_touches_the_send_endpoint(self):
        source = SCRIPT.read_text()
        start = source.index("def command_research")
        body = source[start : source.index("def command_note")]
        self.assertIn("research/request", body)
        self.assertNotIn("latarnik-invite", body)


if __name__ == "__main__":
    unittest.main()

