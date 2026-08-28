#!/usr/bin/env python3
"""
Reddit proxy finder sidecar.

Uses the `free-proxy` Python library to discover a working HTTP proxy,
tests it against Reddit's JSON API, and writes the result to the
`reddit_proxy_state` singleton table. The Rust worker reads this table
every 5 minutes and dynamically rebuilds its HTTP client when the proxy
changes.

Runs in an infinite loop: find → test → write → sleep → repeat.
If no working proxy is found, the existing DB row is left untouched
(the worker keeps using the last known-good proxy until it expires).
"""

import os
import sys
import time
import logging
import urllib.request
import urllib.error

import psycopg2
from fp.fp import FreeProxy

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [reddit-proxy-finder] %(levelname)s %(message)s",
)
log = logging.getLogger("reddit-proxy-finder")

DATABASE_URL = os.environ.get("DATABASE_URL", "")
REFRESH_INTERVAL = int(os.environ.get("REDDIT_PROXY_REFRESH_INTERVAL", "900"))  # 15 min
COUNTRY_ID = os.environ.get("REDDIT_PROXY_COUNTRY_ID", "")  # e.g. "US,DE"
REDDIT_TEST_URL = "https://www.reddit.com/search.json?q=test&limit=1"
PROXY_TIMEOUT = 10  # seconds


def find_working_proxy() -> str | None:
    """Use free-proxy to find a proxy that can reach Reddit's JSON API."""
    kwargs = {"timeout": PROXY_TIMEOUT, "rand": True}
    if COUNTRY_ID:
        kwargs["country_id"] = [c.strip() for c in COUNTRY_ID.split(",") if c.strip()]

    try:
        proxy_url = FreeProxy(**kwargs).get()
        if not proxy_url or "There are no working proxies" in str(proxy_url):
            return None
        return str(proxy_url)
    except Exception as exc:
        log.warning("FreeProxy.get() failed: %s", exc)
        return None


def test_proxy_against_reddit(proxy_url: str) -> tuple[bool, int | None]:
    """Test the proxy by making a request to Reddit's JSON endpoint.
    Returns (success, status_code)."""
    # FreeProxy returns URLs like "http://10.0.0.1:8080" or "https://..."
    # urllib uses the proxy via ProxyHandler.
    proxy_handler = urllib.request.ProxyHandler({
        "http": proxy_url,
        "https": proxy_url,
    })
    opener = urllib.request.build_opener(proxy_handler)
    opener.addheaders = [("User-Agent", "CrowdRelay/1.0 proxy-test")]

    try:
        resp = opener.open(REDDIT_TEST_URL, timeout=PROXY_TIMEOUT)
        status = resp.getcode()
        # Read a small chunk to confirm the response is real
        resp.read(512)
        return (200 <= status < 300, status)
    except urllib.error.HTTPError as exc:
        return (False, exc.code)
    except Exception as exc:
        log.debug("proxy test failed for %s: %s", proxy_url, exc)
        return (False, None)


def write_proxy_to_db(proxy_url: str, ok: bool, status_code: int | None) -> bool:
    """Upsert the proxy state into reddit_proxy_state. Returns True on success."""
    if not DATABASE_URL:
        log.error("DATABASE_URL not set")
        return False
    try:
        conn = psycopg2.connect(DATABASE_URL)
        with conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO reddit_proxy_state (id, proxy_url, last_verified_at, last_verified_ok, reddit_status_code)
                    VALUES (1, %s, now(), %s, %s)
                    ON CONFLICT (id) DO UPDATE SET
                        proxy_url = EXCLUDED.proxy_url,
                        last_verified_at = now(),
                        last_verified_ok = EXCLUDED.last_verified_ok,
                        reddit_status_code = EXCLUDED.reddit_status_code
                    """,
                    (proxy_url, ok, status_code),
                )
        conn.close()
        return True
    except Exception as exc:
        log.error("failed to write proxy to DB: %s", exc)
        return False


def run_cycle() -> bool:
    """Find, test, and write a proxy. Returns True if a working proxy was found."""
    log.info("searching for a working proxy...")
    proxy_url = find_working_proxy()
    if not proxy_url:
        log.warning("no working proxy found this cycle")
        return False

    log.info("found proxy %s, testing against Reddit...", proxy_url)
    ok, status = test_proxy_against_reddit(proxy_url)

    if ok:
        log.info("proxy %s works (Reddit returned %s)", proxy_url, status)
    else:
        log.warning("proxy %s failed Reddit test (status=%s)", proxy_url, status)

    write_proxy_to_db(proxy_url, ok, status)
    return ok


def main() -> int:
    if not DATABASE_URL:
        log.error("DATABASE_URL not set — cannot run")
        return 1

    log.info("starting reddit-proxy-finder (refresh every %ds)", REFRESH_INTERVAL)

    # Run once immediately on startup
    run_cycle()

    # Then loop
    while True:
        time.sleep(REFRESH_INTERVAL)
        run_cycle()


if __name__ == "__main__":
    sys.exit(main())
