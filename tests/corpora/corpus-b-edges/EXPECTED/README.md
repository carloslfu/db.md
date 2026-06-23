# corpus-b-edges — EXPECTED contract

This directory is the **hand-derived** golden for the designed-to-fail
store. Every value here is derived from **SPEC.md § Validation** and
from **exactly what the corpus breaks** — never copied from whatever
`dbmd` happens to emit. A test whose expected value is the
code-under-test's own output is vacuous; these files are the
independent source of truth the tool is measured against.

## Files

| File | What it asserts | How to produce |
|------|-----------------|----------------|
| `validate.json` | The complete issue array for the store sweep. | `dbmd validate --all --json tests/corpora/corpus-b-edges` |
| `not-a-store.json` | The single `NOT_A_STORE` error for the no-`DB.md` sibling. | `dbmd validate --json tests/corpora/corpus-b-edges/not-a-store` |
| `bad-db-md.json` | The three `DB_MD_*` identity-contract errors for the `bad-db-md/` sub-store (separate invocation). | `dbmd validate --all --json tests/corpora/corpus-b-edges/bad-db-md` |
| `policy-refusal/*.json` | Write-time `POLICY_FROZEN_PAGE` refusals (one per write surface). | `dbmd write` / `fm set` / `rename` / `link` against a frozen path |
| `coverage.json` | Maps **every** SPEC code → the fixture(s) that seed it. The e2e test asserts the map contains only real SPEC codes, that every code the golden emits is mapped, and that the bookkeeping (`spec_code_count` / `all_spec_codes_covered` / `uncovered_spec_codes`) agrees with the live SPEC table — so it can't drift or over-claim. | derived from the SPEC table |

`validate.json` issues are sorted by `(file, line, code)` for stable
diffing. The runner should compare as a **set** of issue objects (order
independent) OR sort the tool output the same way before diffing.

## Coverage — 38 of the SPEC § Validation codes are seeded

The SPEC § Validation table defines **48** codes. This corpus seeds
**38** of them (the seeding table below has 38 rows) and deliberately
leaves the rest uncovered — the five `ASSET_*` asset-manifest codes,
`DB_MD_SCHEMA_FIELD`, `FM_UNREADABLE`, `FM_MISSING_CREATED`,
`FM_MISSING_UPDATED`, and `FM_BAD_META_TYPE` (every seeded records file
carries a valid, or absent-defaulting-to-`fact`, `meta-type`). One of the
38, `INDEX_JSONL_DESYNC`, is also plan-mandated (db-md-rust-toolkit.md
line 494) and is grouped under `plan_extensions` in `coverage.json` for
provenance — it still counts as a seeded SPEC code. `coverage.json`
therefore records `all_spec_codes_covered: false` and lists those ten
codes under `uncovered_spec_codes`.

This is enforced, not asserted by hand: the e2e test computes
SPEC-minus-mapped from the live SPEC table and `coverage.json`, and fails
CI unless `uncovered_spec_codes` equals that gap exactly (both
directions) and `all_spec_codes_covered` agrees. So if a future SPEC code
is added without a seeding fixture — or a fixture is removed — the
bookkeeping forces CI red. 33 distinct codes fire in the `--all` sweep
across 56 issue objects (`SCHEMA_SHAPE_MISMATCH` twice — email + date
shape; `INDEX_JSONL_STALE` eighteen times — the contacts (×6), expenses
(×3), invoices (×3), meetings (×2), decisions, notes, processes, and
clients sidecars each kept a single deliberately-stale projected field;
`DUP_UNIQUE_KEY` six times — one per dup-pair fixture) in
`validate.json`; the three `DB_MD_*` identity-contract codes are a
separate invocation on the `bad-db-md/` sub-store (`bad-db-md.json`);
`NOT_A_STORE` is a separate invocation (`not-a-store.json`);
`POLICY_FROZEN_PAGE` is write-time (`policy-refusal/`).

| Code | Severity | Seeded by | Issue site |
|------|----------|-----------|------------|
| `NOT_A_STORE` | error | `not-a-store/` (no DB.md) | dir-level — separate invocation |
| `DB_MD_BAD_TYPE` | error | `bad-db-md/DB.md` (`type: notes`) | `type` line 2 — separate invocation |
| `DB_MD_MISSING_FIELD` | error | `bad-db-md/DB.md` (no `owner`) | block top line 1 — separate invocation |
| `DB_MD_UNKNOWN_SECTION` | warning | `bad-db-md/DB.md` (`## Glossary`) | heading line 18 — separate invocation |
| `FM_MISSING_TYPE` | error | `records/misc/no-type.md` | no `type:` key (line 1) |
| `FM_MALFORMED_YAML` | error | `records/misc/malformed-yaml.md` | unparseable block (line 1) |
| `FM_BAD_TIMESTAMP` | error | `sources/emails/2026/05/bad-timestamp.md` | `created` line 4 |
| `SUMMARY_MISSING` | error | `records/misc/summary-absent.md` | no `summary` key (line 1) |
| `SUMMARY_EMPTY` | error | `records/misc/summary-blank.md` | `summary` line 6 |
| `SUMMARY_MULTILINE` | error | `records/misc/summary-multiline.md` | `summary` line 6 |
| `SUMMARY_TOO_LONG` | warning | `records/misc/summary-overlong.md` | `summary` line 6 (247 chars) |
| `WIKI_LINK_SHORT_FORM` | error | `records/contacts/sarah-chen.md` | `[[acme-co]]` line 19 |
| `WIKI_LINK_HAS_EXTENSION` | warning | `records/contacts/sarah-chen.md` | `[[…northstar.md]]` line 20 |
| `WIKI_LINK_BROKEN` | error | `records/misc/broken-link.md` | `[[…/ghost]]` line 19 |
| `WIKI_LINK_AMBIGUOUS` | error | `records/misc/ambiguous-link.md` | `[[northstar]]` line 19 |
| `WIKI_LINK_FLOW_FORM_LIST` | error | `records/synthesis/flow-form-list.md` | `derived_from` line 9 |
| `DUP_ID` | error | `records/contacts/dup-id-{one,two}.md` | `id` line 3 |
| `DUP_UNIQUE_KEY` | warning (×6) | `contacts/duplicate-email-{a,b}` (`unique: email`), `companies/dup-domain-{a,b}` (`domain`), `expenses/…/2026-05-05-globex-{a,b}` (`date, amount, vendor`), `invoices/…/2026-04-01-northstar-{a,b}` (`vendor, date, amount`), `emails/…/2026-05-22-renewal-{a,b}` (`from, subject, date`), `meetings/…/2026-05-22-sync-{a,b}` (`date, attendees`) | one per `unique:` key in DB.md § Schemas; single-field → field line, compound → line 1 |
| `SCHEMA_MISSING_REQUIRED` | error | `records/contacts/missing-company.md` | absent `company` |
| `SCHEMA_SHAPE_MISMATCH` | error (×2) | `records/contacts/bad-email-shape.md` (`email` line 8) + `records/expenses/2026/05/bad-date-shape.md` (`date` line 7) | two fixtures: email shape + date shape |
| `SCHEMA_LINK_PREFIX_MISMATCH` | error | `records/contacts/plain-company.md` | `company` line 9 |
| `SCHEMA_ENUM_VIOLATION` | error | `records/invoices/2026/04/bad-status-enum.md` | `status` line 10 |
| `POLICY_FROZEN_PAGE` | error | `policy-refusal/*.json` (write-time) | frozen path |
| `POLICY_IGNORED_TYPE_PRESENT` | info | `records/scratch/throwaway.md` | `type: test` line 2 |
| `POLICY_IGNORED_TYPE_DERIVED` | warning | `records/synthesis/derived-from-ignored.md` | `derived_from` line 10 |
| `LOG_BAD_TIMESTAMP` | error | `log.md` | entry line 13 |
| `LOG_UNKNOWN_KIND` | warning | `log.md` | entry line 17 (`frobnicate`) |
| `LOG_OUT_OF_ORDER` | warning | `log.md` | entry line 23 |
| `INDEX_MISSING` | error | `records/misc/` (no index.md) | folder-level |
| `INDEX_STALE_ENTRY` | error | `records/companies/index.md` | line 13 (`ghost-corp`) |
| `INDEX_MISSING_ENTRY` | error | `records/contacts/index.md` | `sarah-chen` omitted |
| `INDEX_ORPHAN` | warning | `records/people/index.md` | empty folder |
| `INDEX_WRONG_SCOPE` | warning | `records/decisions/index.md` | `scope` line 3 |
| `INDEX_SUMMARY_MISMATCH` | error | `records/contacts/index.md` | line 12 (`bad-email-shape`) |
| `INDEX_JSONL_DESYNC` | error | `sources/emails/index.jsonl` | 2 entries / 3 files |
| `INDEX_JSONL_MISSING` | error | `records/playbooks/` (index.md present, no index.jsonl) | folder-level |
| `INDEX_JSONL_STALE` | error (×18) | `records/contacts/index.jsonl` (`company` ×6), `records/expenses/index.jsonl` (`vendor` ×3), `records/invoices/index.jsonl` (`vendor` ×3), `records/meetings/index.jsonl` (`attendees` ×2), `records/decisions/index.jsonl` (`affects`), `records/notes/index.jsonl` (`tags` vs `malformed-tags.md`), `records/processes/index.jsonl` (`summary` vs `invoicing.md`), `records/clients/index.jsonl` (`company` vs `misplaced-contact.md`) | any projected field, not just summary/type: each sidecar carries the injected `meta-type` (so meta-type is fresh) but keeps exactly one other field stale — a link field bent to the wrong target, the nested `tags` a rebuild would normalize away, or an out-of-date summary |
| `TAGS_MALFORMED` | warning | `records/notes/malformed-tags.md` | `tags` nested list, line 7 |

## Issue object shape

```json
{
  "severity": "error|warning|info",
  "code": "WIKI_LINK_SHORT_FORM",
  "file": "records/contacts/sarah-chen.md",   // store-relative; folder path for folder-level issues
  "line": 19,                                  // 1-based; null when there is no single line (missing key/entry, folder/jsonl-level)
  "key": "company",                            // frontmatter key when the issue is field-scoped; else null
  "message": "...",
  "suggestion": "...",                         // deterministic remediation per SPEC
  "related": ["records/contacts/dup-id-two.md"] // other files involved (dup partner, link target, policy source)
}
```

`policy-refusal/*.json` wrap the same object under `error`, plus
`invocation`, `exit_code_nonzero`, `no_write_occurred`, and a
`policy_source` pointer (`DB.md` heading + line).

## Hand-derivation decisions (precedence the corpus pins)

These are deliberate, SPEC-grounded choices made so each code fires on
exactly one fixture without entangling another. They are the corpus's
contract with the validator; the validator should implement them.

1. **Dedup pairs report ONE issue.** `file` = the lexicographically
   smallest store-relative path of the colliding set; `related` =
   the rest. Deterministic because store-relative path is a total
   order. (Applies to `DUP_*`.)
2. **Schema check owns a field's date-shape when an explicit schema
   declares it.** A bad `expense.date` value, with `DB.md ## Schemas`
   declaring `date (required, date)`, is `SCHEMA_SHAPE_MISMATCH` (the
   more specific schema rule), NOT the generic `FM_BAD_TIMESTAMP`.
   `FM_BAD_TIMESTAMP` is reserved for the universal `created`/`updated`
   fields and for date fields on types with no explicit schema. The
   `FM_BAD_TIMESTAMP` fixture therefore breaks `created` (universal),
   and the `SCHEMA_SHAPE_MISMATCH` fixture keeps `created`/`updated`
   valid and breaks only the schema-typed `date`.
3. **Short-form vs ambiguous.** A no-slash wiki-link target is
   `WIKI_LINK_SHORT_FORM` by default; it is `WIKI_LINK_AMBIGUOUS`
   (the defensive code) only when that bare basename matches ≥2 files.
   The ambiguous fixture (`[[northstar]]`) has two basename matches
   (`records/companies/northstar.md` + `records/synthesis/northstar.md`,
   the latter a `synthesis` / `meta-type: conclusion` page that shares the
   basename); the short-form fixture (`[[acme-co]]`) matches zero. This
   lets both codes own a non-overlapping fixture, matching SPEC's wording
   ("ambiguous = matches multiple files"; short-form = "not a full
   path").
4. **A folder with no `index.md` yields only `INDEX_MISSING`.** Its
   individual files are NOT additionally reported as
   `INDEX_MISSING_ENTRY` — the whole index is absent, so per-entry
   completeness is not evaluated. That is why `records/misc/` (8 files,
   no index) produces a single `INDEX_MISSING`, and the
   `INDEX_MISSING_ENTRY` fixture lives in a folder that DOES have an
   index (`records/contacts/`, which omits one existing file).
5. **`index.jsonl` is the COMPLETE twin; `index.md` is the capped
   browse view.** A file omitted from `index.md` is
   `INDEX_MISSING_ENTRY` (md vs disk). A file omitted from
   `index.jsonl` is `INDEX_JSONL_DESYNC` (jsonl must be complete).
   They are seeded in different folders (`records/contacts/` for the
   md omission; `sources/emails/` for the jsonl truncation) so the two
   codes never co-fire on one folder.
6. **Structural index-target wiki-links are not content links.**
   Layer/root indexes point at sub-folder `index` files; a missing
   such target is governed by `INDEX_MISSING`, not `WIKI_LINK_BROKEN`.
   To avoid any overlap this corpus only lets layer/root indexes link
   to type-folder indexes that EXIST (the records layer index does not
   enumerate `records/misc`, whose index is the intentionally-missing
   one). Layer/root index completeness is not a checked invariant here.
7. **The frozen pages are valid files (or absent), not validate
   issues.** `records/decisions/2026-q1-strategy.md` is well-formed and
   appears correctly in its index — being frozen blocks WRITES, not
   listing. `POLICY_FROZEN_PAGE` is therefore only in `policy-refusal/`,
   never in `validate.json`. One refusal fixture
   (`write-nonexistent-frozen.json`) targets a frozen path that does
   NOT exist on disk, proving refusal is keyed on the policy path, not
   file presence.
8. **`not-a-store/` is outside the corpus-b store's validate scope.**
   `dbmd validate --all` on corpus-b validates the store rooted at
   `corpus-b-edges/DB.md` across the two canonical `sources/`/`records/`
   layers (the `wiki/` layer was removed; nothing under a `wiki/` path is
   swept) and does not descend into the non-canonical `not-a-store/`
   sibling. `NOT_A_STORE` is only producible by pointing `dbmd validate`
   directly at `not-a-store/`.
9. **`malformed-yaml.md` yields only `FM_MALFORMED_YAML`.** When the
   frontmatter block fails to parse, no field-level checks (type,
   summary, schema, dedup) run on that file — the block is opaque.
10. **The DB.md-structure codes live in a SEPARATE sub-store, not the
    main sweep.** A store has exactly one `DB.md`, and the corpus-b root
    `DB.md` must stay valid (a broken one would change the parsed
    `Config` — ignored-types, schemas — and ripple into unrelated
    checks). So the three `DB_MD_*` codes are seeded in `bad-db-md/`, a
    sibling sub-store with its own deliberately-broken `DB.md` (wrong
    `type:`, missing `owner`, an unrecognized `## Glossary` section),
    validated by a separate invocation (`bad-db-md.json`). The corpus-b
    `--all` sweep checks only `<root>/DB.md` (clean) and walks
    `sources/`/`records/` under the root, so it never descends into
    `bad-db-md/` — exactly the isolation `not-a-store/` relies on.
11. **`DUP_UNIQUE_KEY` is schema-declared, not built in.** Each dup-pair
    fixture collides only because this store's `DB.md ## Schemas` gives its
    type a `unique:` key (`contact: email`, `company: domain`,
    `expense: date, amount, vendor`, `invoice: vendor, date, amount`,
    `email: from, subject, date`, `meeting: date, attendees`). A
    single-field key anchors to that field's line and carries it as `key`;
    a compound key anchors to line 1 with a null `key`. The
    `records/clients/misplaced-contact.md` fixture — a fully valid
    `contact` filed under a non-canonical records folder
    (`records/clients/`, not `records/contacts/`), which v0.1 flagged only
    for its layer placement — is now a pure clean FILE (the layout is
    convention, not enforcement) and fires nothing on the file itself; its
    sibling `index.jsonl` is deliberately stale on `company` (one of the
    18 `INDEX_JSONL_STALE` sites) but that issue's `file` is the
    `index.jsonl`, never the contact.
12. **The `wiki/` layer was removed; its fixtures moved into `records/`.**
    The redesign dropped the third layer, so the four former `wiki/`
    content files now live under `records/` carrying a real `type` plus
    `meta-type: conclusion` where they were synthesis pages:
    `wiki/companies/northstar.md` → `records/synthesis/northstar.md`
    (`type: synthesis`, the second `[[northstar]]` basename match),
    `wiki/synthesis/flow-form-list.md` → `records/synthesis/flow-form-list.md`
    (keeps the rejected flow-form `derived_from`),
    `wiki/synthesis/derived-from-ignored.md` →
    `records/synthesis/derived-from-ignored.md` (`meta-type: conclusion`
    is what now gates `POLICY_IGNORED_TYPE_DERIVED`, replacing the retired
    `type == wiki-page` gate), `wiki/people/index.md` →
    `records/people/index.md` (empty-folder `INDEX_ORPHAN`), and the
    misplaced contact → `records/clients/` (see #11). The records-layer
    catalog projection injects `meta-type: fact` into every sidecar
    record, so each committed `records/*/index.jsonl` carries it (fresh on
    `meta-type`); the deliberately-stale sidecars keep exactly one OTHER
    field stale.

## What MUST be true (the invariants, restated)

- `dbmd validate --all --json <store>` emits **exactly** the 56 issues
  in `validate.json` (33 distinct codes; `SCHEMA_SHAPE_MISMATCH` twice,
  `INDEX_JSONL_STALE` eighteen times, `DUP_UNIQUE_KEY` six times) — no
  more (no spurious issues on the deliberately clean link targets and
  clean indexes — and no spurious `meta-type` staleness now that every
  sidecar carries the injected `meta-type: fact`), no fewer.
- Exit code is **non-zero** (there are 41 errors).
- Every `error` blocks; `warning`/`info` do not change the exit code on
  their own — but here errors dominate, so exit is non-zero regardless.
- `dbmd validate --all --json <store>/bad-db-md` emits **exactly** the
  three `DB_MD_*` issues in `bad-db-md.json` (2 errors + 1 warning) and
  exits non-zero; the main sweep emits none of them.
- The clean files exist precisely to catch false positives:
  `records/companies/northstar.md`, `records/synthesis/northstar.md`,
  `records/decisions/2026-q1-strategy.md`, `records/clients/misplaced-contact.md`
  (fully clean — schema + links valid, placement no longer warns), every clean
  type-folder `index.md`/`index.jsonl`, and the first two well-formed
  `log.md` entries must produce **no** other issue.
