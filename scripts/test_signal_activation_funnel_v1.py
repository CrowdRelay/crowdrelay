#!/usr/bin/env python3
"""A column built to be measured must have a writer.

Migration 0257 created `signal_installations.fan_id` with the comment "the gap
between the count of rows and the count of non-null values here IS the
activation funnel". The install half shipped and the identify half did not, so
for as long as that lasted the funnel reported 0% activation no matter how many
installs signed in — and reported it in `metrics_snapshot`, which the brain
reads. A metric that is wrong is worse than one that is missing: the missing one
is asked about, the wrong one is believed.

Nothing about that failure was detectable at compile time or by any test. The
endpoint returned 200, the install rows were correct, the funnel query ran, and
every number it produced was a true statement about an empty numerator.

So this gate pins three things:

  * `fan_id` has a writer at all, in the layer allowed to hold SQL;
  * the fan push registration calls it, because that route is the only one
    holding both an authenticated fan and the app's own installation id;
  * the call cannot fail the registration, which is the property that makes it
    safe to put a measurement write on a live request path.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
INFRA = ROOT / "crates/crowdrelay-infra/src/signal_installations.rs"
PUSH = ROOT / "crates/crowdrelay-api/src/push.rs"
MIGRATIONS = ROOT / "migrations"

LINK_FN = "link_installation_to_fan"


def handler_body(source: str, name: str) -> str:
    """One handler's body, from its signature to the next item at column zero."""
    start = source.index(f"pub async fn {name}(")
    rest = source[start:]
    end = re.search(r"\n\}\n", rest)
    if end is None:
        raise AssertionError(f"could not find the end of {name}")
    return rest[: end.end()]


class SignalActivationFunnelContract(unittest.TestCase):
    def test_the_column_exists_and_is_still_described_as_the_funnel(self):
        """If the schema comment goes, the reason for the rest of this goes."""
        created = [
            path
            for path in sorted(MIGRATIONS.glob("*.sql"))
            if "CREATE TABLE signal_installations" in path.read_text()
        ]
        self.assertEqual(
            len(created), 1, "exactly one migration creates signal_installations"
        )
        schema = created[0].read_text()
        self.assertIn("fan_id", schema)
        self.assertIn(
            "activation funnel",
            schema,
            "the column's purpose is recorded in the migration; keep it there",
        )

    def test_fan_id_has_a_writer(self):
        source = INFRA.read_text()
        self.assertIn(
            f"pub async fn {LINK_FN}(",
            source,
            "signal_installations.fan_id must have a writer, or the funnel "
            "numerator is zero by construction",
        )
        write = source[source.index(f"pub async fn {LINK_FN}(") :]
        self.assertIn("UPDATE signal_installations", write)
        self.assertIn("SET fan_id", write)

    def test_the_write_is_scoped_and_only_fires_once(self):
        write = INFRA.read_text()
        write = write[write.index(f"pub async fn {LINK_FN}(") :]
        statement = write[write.index("UPDATE signal_installations") :]
        self.assertIn(
            "workspace_id = $1",
            statement,
            "the whole of tenant isolation is naming the workspace",
        )
        self.assertIn(
            "fan_id IS NULL",
            statement,
            "the column answers 'did this install ever convert', so the first "
            "identification is what it must keep — a device handed to somebody "
            "else is not a second conversion",
        )

    def test_the_launch_upsert_never_erases_the_link(self):
        """`record_installation` runs on every launch; the link runs once."""
        source = INFRA.read_text()
        upsert = source[source.index("pub async fn record_installation(") :]
        # Bounded by the raw string's own terminator. Slicing to the next
        # function instead swept in that function's doc comment, which says
        # `fan_id` for good reasons, and the assertion failed on prose.
        conflict = upsert[upsert.index("ON CONFLICT") : upsert.index('"#')]
        self.assertNotIn(
            "fan_id",
            conflict,
            "the launch upsert must not touch fan_id, or every launch after "
            "identification resets the install to anonymous",
        )

    def test_the_fan_push_registration_links_the_install(self):
        body = handler_body(PUSH.read_text(), "register_endpoint")
        self.assertIn(
            LINK_FN,
            body,
            "/v1/me/push/endpoints is the only route holding both a fan "
            "session and the app's installation id, so it is the only place "
            "the funnel can be closed",
        )

    def test_the_link_cannot_fail_the_registration(self):
        """A measurement write on a live path must not be able to refuse it."""
        body = handler_body(PUSH.read_text(), "register_endpoint")
        after = body[body.index(LINK_FN) :]
        # The handler's own error path returns `PushError::Unavailable`. The link
        # must not reach it, and must not use `?` either — the function returns
        # Response, so a `?` here would not compile, but a future refactor to a
        # Result-returning handler would make it compile and silently start
        # rejecting push registrations over a lost funnel datapoint.
        tail = after[: after.index("StatusCode::OK")]
        self.assertNotIn(
            ".await?",
            tail,
            "the link must not propagate its error: accepting the push "
            "registration is this endpoint's job",
        )
        self.assertIn(
            "Err(error) =>",
            tail,
            "the link's failure must be handled explicitly and logged",
        )
        self.assertNotIn(
            "PushError",
            tail,
            "losing a funnel datapoint is not a reason to refuse a push "
            "registration",
        )


if __name__ == "__main__":
    unittest.main()
