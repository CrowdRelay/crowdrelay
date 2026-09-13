#!/usr/bin/env python3
"""Registering a hand-published post must say what actually went wrong.

Reddit is read-only by policy and the other three channels default to manual, so
`POST /v1/control-plane/{community,social,telegram,discord}-posts/{id}/register-manual`
is the path every real post this tenant makes actually takes. The operator
publishes by hand — an irreversible public act — and then registers it here.

All four handlers answered every failure with 400, "The request could not be
parsed or validated". That is the wrong thing to say to somebody holding a URL
they know is right, and it made three different situations indistinguishable:

  * the post id is wrong — 404;
  * the post is already registered — 409, and the operator's natural response to
    an ambiguous failure is to retry, or worse, to publish to Reddit a second
    time under the band's name;
  * the database is down — 503, and the operator should retry rather than debug
    their URL.

The infra layer already distinguished them; only the URL-extraction failure is
genuinely a 400. The repository's `NotFound` also conflated "no such row" with
"row is not awaiting publication", so the split goes there too and the status
travels with it — an answer that can say "already registered" is worth more than
one that says "not found".

`Problem::conflict_because` exists for exactly this: its own doc says the generic
409 detail "is true of every 409 and actionable for none".
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
API = ROOT / "crates/crowdrelay-api/src/fanbase.rs"
INFRA = ROOT / "crates/crowdrelay-infra/src/fanbase/manual_publication.rs"

HANDLERS = (
    "register_manual_community_post",
    "register_manual_social_post",
    "register_manual_telegram_post",
    "register_manual_discord_post",
)


def handler_body(name: str) -> str:
    source = API.read_text()
    start = source.index(f"pub async fn {name}(")
    rest = source[start:]
    end = re.search(r"\n\}\n", rest)
    if end is None:
        raise AssertionError(f"could not find the end of {name}")
    return rest[: end.end()]


class ManualPublicationAnswers(unittest.TestCase):
    def test_all_four_handlers_are_still_here(self):
        source = API.read_text()
        for name in HANDLERS:
            self.assertIn(
                f"pub async fn {name}(",
                source,
                "a manual-registration handler moved; this gate is checking the "
                "wrong set",
            )

    def test_no_handler_answers_every_failure_the_same_way(self):
        for name in HANDLERS:
            body = handler_body(name)
            failure = body[body.index("Err(error)") :]
            statuses = set(re.findall(r"Problem::(\w+)\(", failure))
            self.assertGreaterEqual(
                len(statuses),
                3,
                f"{name} answers {sorted(statuses) or ['nothing']} for every "
                "failure. A wrong id, an already-registered post and a database "
                "outage need different answers; this is the path every real post "
                "takes.",
            )

    def test_an_outage_is_not_reported_as_a_bad_request(self):
        for name in HANDLERS:
            body = handler_body(name)
            self.assertIn(
                "Database(_) => Problem::service_unavailable",
                body,
                f"{name} must answer 503 for a database error, so the operator "
                "retries instead of debugging a URL that is correct",
            )

    def test_an_already_registered_post_is_a_named_conflict(self):
        for name in HANDLERS:
            body = handler_body(name)
            self.assertIn(
                "NotAwaitingPublication { .. } => Problem::conflict_because",
                body,
                f"{name} must answer a named 409: the generic detail is true of "
                "every conflict and actionable for none",
            )
            self.assertIn(
                "registered already",
                body,
                f"{name}'s conflict must name the likely cause, not restate the "
                "status code",
            )

    def test_only_an_unreadable_url_is_a_bad_request(self):
        """The one failure 400 actually describes."""
        reddit = handler_body("register_manual_community_post")
        self.assertIn("InvalidUrl(_) => Problem::bad_request", reddit)
        # The other three take no URL to parse, so none of them should have a
        # 400 path at all.
        for name in HANDLERS[1:]:
            body = handler_body(name)
            failure = body[body.index("Err(error)") :]
            self.assertNotIn(
                "Problem::bad_request",
                failure,
                f"{name} has no URL to fail extraction on, so it has no 400 case",
            )

    def test_the_repository_still_separates_the_two_zero_row_reasons(self):
        infra = INFRA.read_text()
        # Both error enums, not one. `assertIn` passed while a mutation removed
        # the variant from `ManualRedditPostError` alone, because
        # `ManualContentPostError` still had it — the single most likely way for
        # this to regress is one of the two being changed.
        self.assertEqual(
            infra.count("NotAwaitingPublication { status: String }"),
            2,
            "both ManualRedditPostError and ManualContentPostError must "
            "distinguish 'no such row' from 'wrong status', and carry the status "
            "so the answer can name it",
        )
        # Four failure paths, one per channel, each asking which reason it was.
        self.assertEqual(
            len(re.findall(r"publication_failure\(", infra)),
            5,
            "each of the four registrations must classify its own zero-row "
            "update (four call sites plus the function itself)",
        )

    def test_the_classifier_never_takes_a_table_name_from_input(self):
        """It interpolates a table name, so the source of that name matters."""
        infra = INFRA.read_text()
        signature = infra[infra.index("async fn publication_failure(") :]
        signature = signature[: signature.index(")")]
        self.assertIn(
            "table: &'static str",
            signature,
            "the interpolated table name must be a 'static literal, never a "
            "runtime string",
        )
        for call in re.findall(r'publication_failure\(\s*&mut transaction,\s*("[^"]+")', infra):
            self.assertRegex(call, r'^"[a-z_]+"$', "table names must be literals")


if __name__ == "__main__":
    unittest.main()
