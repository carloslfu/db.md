#!/usr/bin/env python3
"""Deterministic generator for the corpus-a near-cap expense folder.

Intent (derived from SPEC.md, not from tool output):
  - A near-cap `records/expenses/` type-folder: 490 records (< the 500
    browse-view cap) so the corpus exercises the borderline WITHOUT
    triggering `## More` overflow. md == jsonl just under the cap.
  - Event type → date-sharded: split across `2026/04/` and `2026/05/`.
  - Every record satisfies the corpus `DB.md` `### expense` schema:
        date (required, date), amount (required, currency),
        currency (default USD), category (string),
        vendor (required, link to records/companies/).
  - Every `vendor` is a full-path wiki-link to an existing company
    record (one of the three vendor companies), so no WIKI_LINK_BROKEN.
  - Every `(date, amount, vendor)` tuple is UNIQUE, and corpus-a's
    `### expense` schema declares no `unique:` key, so `dbmd validate
    --all` reports no `DUP_UNIQUE_KEY` collision — the store is clean
    under both the working-set and the full sweep.
  - Every record carries a hand-written `summary` in the
    `<date> — <amount> <currency> — <vendor-leaf>` style (the shape the
    pre-v0.2 built-in expense composer produced) — this is
    what a bulk-ingested ledger realistically looks like, and it lets
    the later index goldens be derived by the same rule.

Run from the corpus root:  python3 .gen-expenses.py
Idempotent: clears and rewrites the two expense shards.
"""

import os
import shutil

ROOT = os.path.dirname(os.path.abspath(__file__))
EXP = os.path.join(ROOT, "records", "expenses")

# Three vendor companies that exist as flat records/companies/ files.
# (company-record-stem, leaf-used-by-default-summary)
VENDORS = ["github", "aws", "figma"]

CATEGORIES = {
    "github": "software",
    "aws": "infrastructure",
    "figma": "software",
}

# Per-month plan: (year, month, day-count, records). 245 + 245 = 490.
MONTHS = [("2026", "04", 30, 245), ("2026", "05", 31, 245)]


def two(n: int) -> str:
    return f"{n:02d}"


def gen_month(year: str, month: str, days: int, count: int):
    shard = os.path.join(EXP, year, month)
    if os.path.isdir(shard):
        shutil.rmtree(shard)
    os.makedirs(shard, exist_ok=True)

    files = 0
    seq = 0
    # Walk days 1..days repeatedly until `count` records are emitted, so
    # the folder is densely populated and shard order is by date then seq.
    while files < count:
        for day in range(1, days + 1):
            if files >= count:
                break
            seq += 1
            vendor = VENDORS[seq % len(VENDORS)]
            category = CATEGORIES[vendor]
            # Unique amount per record (monotonic cents) → unique
            # (date, amount, vendor) tuple across the whole ledger.
            amount = f"{(10000 + seq * 7) / 100:.2f}"  # e.g. 100.07, 100.14, ...
            date = f"{year}-{month}-{two(day)}"
            ts = f"{date}T09:{two(seq % 60)}:00-07:00"
            stem = f"{date}-{vendor}-{seq:04d}"
            summary = f"{date} — {amount} USD — {vendor}"
            # No explicit `id:` — it is path-derived per SPEC when absent, and an
            # explicit id equal to the path stem only invites DUP_ID false alarms
            # when an entity also has a wiki page. Path-derived ids are unique by
            # construction (the path is unique).
            body = (
                f"---\n"
                f"type: expense\n"
                f"created: {ts}\n"
                f"updated: {ts}\n"
                f'summary: "{summary}"\n'
                f"date: {date}\n"
                f"amount: {amount}\n"
                f"currency: USD\n"
                f"category: {category}\n"
                # Quoted scalar wiki-link: the canonical scalar form that the
                # summary composer resolves (an UNQUOTED `[[x]]` parses as a YAML
                # nested sequence the composer drops). Validation accepts both;
                # the quoted form is what makes the default `summary` carry the
                # vendor, matching the SPEC expense template.
                f'vendor: "[[records/companies/{vendor}]]"\n'
                f"tags: [vendor, {category}]\n"
                f"status: reconciled\n"
                f"---\n"
                f"\n"
                f"Recurring {vendor} charge, {date}. Reconciled against the "
                f"monthly vendor invoice.\n"
            )
            with open(os.path.join(shard, f"{stem}.md"), "w") as fh:
                fh.write(body)
            files += 1
    return files


def main():
    total = 0
    for (y, m, days, count) in MONTHS:
        n = gen_month(y, m, days, count)
        print(f"{y}/{m}: {n} expense records")
        total += n
    print(f"total expense records: {total}")
    assert total < 500, "near-cap folder must stay under the 500 browse cap"


if __name__ == "__main__":
    main()
