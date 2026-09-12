#!/usr/bin/env python3
"""Turn the band's contact workbooks into one CSV the importer can read.

The CRM lives in Excel and holds thousands of rows the growth loop has never
seen: radio shows, stations, press, blogs, playlist curators, endorsement
brands. CrowdRelay held 22 press targets, nine with an address, so a press
pitch had almost nobody to reach.

This does the reading and the mapping; it writes nothing to any database. The
Rust side (`crowdrelay-worker import-outreach`) owns the writes, so every row
lands through the same screening an agent-proposed target goes through.

Only rows with a real email survive. A contact without an address is a lead,
not a recipient, and the growth loop cannot use one.
"""
from __future__ import annotations

import argparse
import csv
import re
import sys
import unicodedata
from pathlib import Path

EMAIL = re.compile(r"^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}$")

# `agent_outreach_targets_target_kind_check` accepts exactly these.
KINDS = {"press", "radio", "playlist", "media_patronage", "endorsement", "creator", "community"}

# The workbook's own Polish vocabulary, mapped to that check constraint.
# Anything unmapped is skipped rather than guessed: a booking agency is not a
# press contact, and filing it as one would put a pitch in front of the wrong
# person.
CATEGORY = {
    "radio": "radio",
    "audycja radiowa": "radio",
    "radio internetowe": "radio",
    "podcast / audycja": "radio",
    "audycja / nabor": "radio",
    "prasa": "press",
    "portal informacyjny": "press",
    "portal muzyczny": "press",
    "portal rock/metal": "press",
    "magazyn / portal": "press",
    "blog": "press",
    "telewizja": "press",
    "label / media": "media_patronage",
    "wydawnictwo / label": "media_patronage",
    "sklep / merch": "endorsement",
    "grafika pl": "creator",
    "strona / spolecznosc facebook": "community",
}


def fold(value: str) -> str:
    """Lowercase and strip Polish diacritics so the lookup is stable."""
    text = unicodedata.normalize("NFKD", str(value)).encode("ascii", "ignore").decode()
    return " ".join(text.lower().split())


def clean(value) -> str:
    return "" if value is None else " ".join(str(value).split())


def first_email(value) -> str:
    """The first address in a cell. Several rows carry two, comma separated."""
    for part in re.split(r"[,;/\s]+", clean(value)):
        if EMAIL.match(part):
            return part.lower()
    return ""


def country(value) -> str:
    text = fold(value)
    if not text:
        return ""
    if text.startswith("pol") or text == "pl":
        return "PL"
    return text.upper()[:2] if len(text) >= 2 else ""


def rows_of(worksheet):
    header = None
    for row in worksheet.iter_rows(values_only=True):
        if header is None:
            header = [clean(c) for c in row]
            continue
        yield dict(zip(header, row))


def harvest(path: Path, sheet: str, mapping: dict[str, str], source: str):
    import openpyxl

    book = openpyxl.load_workbook(path, read_only=True, data_only=True)
    if sheet not in book.sheetnames:
        book.close()
        return
    for record in rows_of(book[sheet]):
        email = first_email(record.get(mapping["email"]))
        if not email:
            continue
        # A sheet whose every row is one kind declares it directly; the rest
        # carry a category column in the band's own Polish vocabulary.
        fixed = mapping.get("fixed_kind")
        kind = fixed or CATEGORY.get(fold(record.get(mapping["kind"], "")), "")
        if kind not in KINDS:
            continue
        name = clean(record.get(mapping["name"]))
        # Excel coerces a station called "Radio 7" in a person column to the
        # number 7.0. A bare number is not a name; fall back to the
        # organisation, which is the row's real identity anyway.
        if not name or re.fullmatch(r"[\d.,]+", name):
            name = clean(record.get(mapping.get("org", ""))) or name
        if not name or re.fullmatch(r"[\d.,]+", name):
            continue
        notes = " · ".join(
            clean(record.get(field))
            for field in mapping.get("notes", [])
            if clean(record.get(field))
        )
        yield {
            "display_name": name[:200],
            "contact_email": email[:320],
            "target_kind": kind,
            "evidence_url": clean(record.get(mapping.get("url", ""))),
            "country_code": country(record.get(mapping.get("country", ""))),
            "fit_score": clean(record.get(mapping.get("fit", ""))),
            "notes": notes[:500],
            "source": source,
        }
    book.close()


SHEETS = [
    (
        "VIRYA — CRM _ KONTAKTY — EDYTUJ TU.xlsx",
        "Kontakty",
        {
            "name": "Osoba",
            "org": "Organizacja",
            "email": "Email",
            "kind": "Kategoria",
            "url": "Profil_WWW",
            "country": "Kraj",
            "fit": "Dopasowanie_Virya",
            "notes": ["Funkcja", "Organizacja", "Miasto"],
        },
        "crm:kontakty",
    ),
    (
        "VIRYA — CRM _ KONTAKTY — EDYTUJ TU.xlsx",
        "Media_Radio_Audycje",
        {
            "name": "Nazwa",
            "email": "Email",
            "kind": "Typ",
            "url": "WWW",
            "country": "Kraj",
            "fit": "Dopasowanie_Virya",
            # The submission route is the most useful thing in the sheet: it is
            # how a pitch actually reaches this outlet.
            "notes": ["Ścieżka_zgłoszenia", "Gatunki", "Miasto"],
        },
        "crm:media",
    ),
    (
        "VIRYA — CRM _ KONTAKTY — EDYTUJ TU.xlsx",
        "Podmioty",
        {
            "name": "Nazwa",
            "email": "Kontakt_surowy",
            "kind": "Typ",
            "url": "Facebook",
            "country": "Kraj",
            "fit": "Dopasowanie_Virya",
            "notes": ["Typ", "Miasto"],
        },
        "crm:podmioty",
    ),
    (
        "PLAYLIST_PITCHING — VIRYA.xlsx",
        "CURATOR_CONTACTS",
        {
            "name": "Curator_or_Playlist",
            "email": "Email",
            "kind": "__playlist__",
            "url": "Playlist_URLs",
            "notes": ["Genres", "Platform"],
        },
        "playlist:curators",
    ),
]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", default=str(Path.home() / "dev" / "contacts"))
    parser.add_argument("--out", default="outreach_targets.csv")
    args = parser.parse_args()

    source_dir = Path(args.source_dir)
    seen: set[tuple[str, str]] = set()
    harvested: list[dict] = []
    for filename, sheet, mapping, source in SHEETS:
        path = source_dir / filename
        if not path.is_file():
            print(f"skip (missing): {filename}", file=sys.stderr)
            continue
        # The playlist sheet has no category column — every row is a curator.
        if mapping["kind"] == "__playlist__":
            mapping = dict(mapping, fixed_kind="playlist")
        for row in harvest(path, sheet, mapping, source):
            key = (row["contact_email"], row["target_kind"])
            if key in seen:
                continue
            seen.add(key)
            harvested.append(row)

    fields = [
        "display_name",
        "contact_email",
        "target_kind",
        "evidence_url",
        "country_code",
        "fit_score",
        "notes",
        "source",
    ]
    with open(args.out, "w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields)
        writer.writeheader()
        writer.writerows(harvested)

    by_kind: dict[str, int] = {}
    for row in harvested:
        by_kind[row["target_kind"]] = by_kind.get(row["target_kind"], 0) + 1
    print(f"wrote {len(harvested)} unique targets to {args.out}")
    for kind, count in sorted(by_kind.items(), key=lambda item: -item[1]):
        print(f"  {count:5}  {kind}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
