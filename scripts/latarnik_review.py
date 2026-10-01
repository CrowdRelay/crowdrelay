#!/usr/bin/env python3
"""Read each invitation before it goes.

The "also hear the dates" letter (P.1) is the band's own answer to mailing a
list: one person, one reason, once, in a colleague's register. The write that
sends it, ``POST /v1/control-plane/contacts/{beacon_id}/latarnik-invite``, is
the approval itself: it queues the letter as already approved and the mail
leaves after a short hold window. So the only honest way to use it is to read
the letter first. This tool does exactly that, one person at a time.

    CROWDRELAY_CONTROL_PLANE_API_KEY=... scripts/latarnik_review.py list
    CROWDRELAY_CONTROL_PLANE_API_KEY=... scripts/latarnik_review.py review --limit 10

``review`` needs a terminal. For every person it fetches the preview (the same
composer and the same refusals as the send, nothing queued), prints the whole
letter, and asks. Nothing is sent unless you answer ``s`` *and* then type
``tak``. There is no flag that skips either question, and no batch mode: the
server has none on purpose, and a session is capped at ``MAX_PER_SESSION``.

Nobody is written to unread. A person with no recent, sourced fact on file about
what they did lately is held ("not read yet") and never appears in ``review``:

    scripts/latarnik_review.py needs-research          # the work queue
    scripts/latarnik_review.py research --limit 10      # send the research agent
    scripts/latarnik_review.py note BEACON_ID \\
        --fact "recenzja plyty X w audycji Y" \\
        --source-url https://... --observed-on 2026-09-20 \\
        [--praise "one specific, true sentence"]

The fact must be dated within the last 120 days, sourced to an https page, and in
the band's register (no exclamation marks, hashtags or links). The research agent
writes through the same route and the same checks.

Standard library only. The key is read from the environment and never printed.
"""
from __future__ import annotations

import argparse
import os
import sys
from typing import Any, Callable, Iterable

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from latarnik_operator import OperatorError, env, request_json  # noqa: E402

MAX_PER_SESSION = 25
DEFAULT_PER_SESSION = 10
KEY_NAME = "CROWDRELAY_CONTROL_PLANE_API_KEY"


def cp_request(method: str, path: str, *, idempotency_key: str | None = None) -> Any:
    return request_json(
        method,
        f"control-plane/{path.lstrip('/')}",
        bearer=env(KEY_NAME),
        idempotency_key=idempotency_key,
    )


def invitable(review: dict[str, Any]) -> list[dict[str, Any]]:
    """The people who may be asked right now, strongest relationship first."""
    rows = [row for row in review.get("contacts", []) if row.get("invitable")]
    return sorted(
        rows,
        key=lambda row: (-int(row.get("relationship_score") or 0), str(row.get("display_name"))),
    )


def needs_research(review: dict[str, Any]) -> list[dict[str, Any]]:
    """Warm people who could be asked if only the band had read their recent work.

    This is the research queue, and it is deliberately narrow: it holds only
    people who passed every relationship test, so no research is spent on a cold
    contact or on somebody written to last week.
    """
    rows = [
        row
        for row in review.get("contacts", [])
        if not row.get("invitable")
        and not row.get("has_research")
        and "not read yet" in str(row.get("hold_reason") or "")
    ]
    return sorted(
        rows,
        key=lambda row: (-int(row.get("relationship_score") or 0), str(row.get("display_name"))),
    )


def render_letter(preview: dict[str, Any]) -> str:
    """The whole letter, with who it is going to and why, exactly as sent."""
    rule = "-" * 72
    lines = [
        rule,
        f"Do:      {preview['recipient_name']} <{preview['recipient_email']}>",
        f"Rola:    {preview.get('role', '')}"
        + (f", {preview['city']}" if preview.get("city") else ""),
        f"Powod:   {preview.get('reason', '')}",
        f"Czytane: {preview.get('hook_fact', '')}",
        f"Zrodlo:  {preview.get('hook_source_url', '')}  ({preview.get('hook_observed_on', '')})",
        f"Temat:   {preview['subject']}",
        rule,
        preview["body"].rstrip(),
        rule,
    ]
    return "\n".join(lines)


def review_loop(
    rows: Iterable[dict[str, Any]],
    *,
    limit: int,
    get_preview: Callable[[str], dict[str, Any]],
    send: Callable[[str], Any],
    ask: Callable[[str], str],
    out: Callable[[str], None],
) -> dict[str, int]:
    """One person at a time. Sends only on ``s`` followed by ``tak``."""
    if not 1 <= limit <= MAX_PER_SESSION:
        raise OperatorError(f"--limit must be between 1 and {MAX_PER_SESSION}")
    counts = {"sent": 0, "skipped": 0, "refused": 0}
    shown = 0
    for row in rows:
        if shown >= limit:
            break
        beacon_id = row["beacon_id"]
        preview = get_preview(beacon_id)
        if "refused" in preview:
            # The list is a snapshot; the preview re-reads everything. A person
            # who stopped being askable since is skipped with the server's reason.
            out(f"POMINIETY {row.get('display_name')}: {preview['refused']}")
            counts["refused"] += 1
            continue
        shown += 1
        out(render_letter(preview))
        answer = ask("[s]wyslij  [n]astepny  [q]uit > ").strip().lower()
        if answer == "q":
            break
        if answer != "s":
            counts["skipped"] += 1
            continue
        confirm = ask(f"Wyslac do {preview['recipient_email']}? Wpisz tak > ").strip().lower()
        if confirm != "tak":
            out("Nie wyslano.")
            counts["skipped"] += 1
            continue
        result = send(beacon_id)
        if isinstance(result, dict) and "refused" in result:
            out(f"ODMOWA: {result['refused']}")
            counts["refused"] += 1
        else:
            out("W kolejce. List wyjdzie po krotkim oknie wstrzymania; da sie go jeszcze cofnac.")
            counts["sent"] += 1
    return counts


def command_list(_: argparse.Namespace) -> int:
    review = cp_request("GET", "contacts/dual-role")
    rows = invitable(review)
    print(
        f"Wszyscy: {review.get('total')}  juz w srodku: {review.get('already_hear_the_dates')}  "
        f"do zaproszenia teraz: {review.get('invitable_now')}"
    )
    for row in rows:
        print(
            f"{row['beacon_id']}  {row.get('relationship_score'):>3}  "
            f"{row.get('role', ''):<12} {row.get('city') or '-':<18} {row.get('display_name')}"
        )
    unread = needs_research(review)
    if unread:
        print(f"\nDo zbadania (jeszcze nieprzeczytani): {len(unread)}  - `needs-research`")
    return 0


def command_needs_research(_: argparse.Namespace) -> int:
    review = cp_request("GET", "contacts/dual-role")
    rows = needs_research(review)
    print(f"Nieprzeczytani, a gotowi na list: {len(rows)}")
    for row in rows:
        print(
            f"{row['beacon_id']}  {row.get('relationship_score'):>3}  "
            f"{row.get('role', ''):<12} {row.get('city') or '-':<18} {row.get('display_name')}"
        )
    return 0


def research_loop(
    rows: Iterable[dict[str, Any]],
    *,
    limit: int,
    request: Callable[[str], Any],
    out: Callable[[str], None],
) -> dict[str, int]:
    """Send the research agent after each person, one at a time, up to ``limit``.

    Researching contacts nobody, so there is no confirmation; the server still
    refuses anybody who is not worth researching yet and anybody researched in
    the last week, and this loop reports each answer instead of hiding it.
    """
    if not 1 <= limit <= MAX_PER_SESSION:
        raise OperatorError(f"--limit must be between 1 and {MAX_PER_SESSION}")
    counts = {"queued": 0, "refused": 0}
    for row in list(rows)[:limit]:
        result = request(row["beacon_id"])
        if isinstance(result, dict) and "queued" in result:
            counts["queued"] += 1
            out(f"zlecone  {row.get('display_name')}")
        else:
            counts["refused"] += 1
            reason = result.get("refused") if isinstance(result, dict) else result
            out(f"pominiete {row.get('display_name')}: {reason}")
    return counts


def command_research(args: argparse.Namespace) -> int:
    review = cp_request("GET", "contacts/dual-role")
    rows = needs_research(review)
    print(f"Nieprzeczytani, a gotowi na list: {len(rows)}. Zlecam najwyzej {args.limit}.")
    counts = research_loop(
        rows,
        limit=args.limit,
        request=lambda beacon_id: cp_request("POST", f"contacts/{beacon_id}/research/request"),
        out=print,
    )
    print(f"Zlecone: {counts['queued']}  pominiete: {counts['refused']}")
    print("Wyniki wracaja, gdy agent skonczy; potem `review` pokaze te listy.")
    return 0


def command_note(args: argparse.Namespace) -> int:
    body: dict[str, Any] = {
        "fact": args.fact,
        "source_url": args.source_url,
        "observed_on": args.observed_on,
        "language": args.language,
    }
    if args.praise:
        body["praise"] = args.praise
    result = request_json(
        "PUT",
        f"control-plane/contacts/{args.beacon_id}/research",
        bearer=env(KEY_NAME),
        payload=body,
    )
    if isinstance(result, dict) and "refused" in result:
        print(f"ODMOWA: {result['refused']}")
        return 1
    print(f"Zapisano: {result.get('fact')} ({result.get('observed_on')})")
    return 0


def command_review(args: argparse.Namespace) -> int:
    if not (sys.stdin.isatty() and sys.stdout.isatty()):
        raise OperatorError("review needs a terminal: every letter is read and confirmed by a person")
    review = cp_request("GET", "contacts/dual-role")
    rows = invitable(review)
    print(f"Do zaproszenia teraz: {len(rows)}. W tej sesji najwyzej {args.limit}.")
    counts = review_loop(
        rows,
        limit=args.limit,
        get_preview=lambda beacon_id: cp_request(
            "GET", f"contacts/{beacon_id}/latarnik-invite/preview"
        ),
        send=lambda beacon_id: cp_request(
            "POST",
            f"contacts/{beacon_id}/latarnik-invite",
            # One stable key per person: the server's once-ever rule and the
            # ledger's replay both hang off it, so a re-run can never ask twice.
            idempotency_key=f"latarnik-{beacon_id}",
        ),
        ask=input,
        out=print,
    )
    print(f"Wyslane: {counts['sent']}  pominiete: {counts['skipped']}  odmowy: {counts['refused']}")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("list", help="who may be asked now (read only)").set_defaults(run=command_list)
    sub.add_parser(
        "needs-research", help="warm people the band has not read yet (the research queue)"
    ).set_defaults(run=command_needs_research)
    research = sub.add_parser(
        "research", help="send the research agent to read the people in the queue"
    )
    research.add_argument(
        "--limit",
        type=int,
        default=DEFAULT_PER_SESSION,
        help=f"people to send this session (1-{MAX_PER_SESSION})",
    )
    research.set_defaults(run=command_research)
    note = sub.add_parser("note", help="record one dated, sourced thing a person did lately")
    note.add_argument("beacon_id")
    note.add_argument("--fact", required=True, help="what they did, as a phrase the letter can quote")
    note.add_argument("--source-url", required=True, help="https page where it can be checked")
    note.add_argument("--observed-on", required=True, help="YYYY-MM-DD: when it happened")
    note.add_argument("--praise", help="one specific, true sentence of appreciation")
    note.add_argument("--language", default="pl")
    note.set_defaults(run=command_note)
    review = sub.add_parser("review", help="read each letter, send only what you confirm")
    review.add_argument(
        "--limit",
        type=int,
        default=DEFAULT_PER_SESSION,
        help=f"letters to show this session (1-{MAX_PER_SESSION})",
    )
    review.set_defaults(run=command_review)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.run(args)
    except OperatorError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
