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

## Coverage — all 44 SPEC § Validation codes are seeded

The SPEC § Validation table defines **44** codes. This corpus seeds
**all 44** (the seeding table below has 44 rows). One of those 44,
`INDEX_JSONL_DESYNC`, is also plan-mandated (db-md-rust-toolkit.md line
494) and is grouped under `plan_extensions` in `coverage.json` for
provenance — it still counts as a seeded SPEC code. `coverage.json`
records `all_spec_codes_covered: true` and `uncovered_spec_codes: []`.

This is enforced, not asserted by hand: the e2e test computes
SPEC-minus-mapped from the live SPEC table and `coverage.json`, and fails
CI unless `uncovered_spec_codes` equals that gap exactly (both
directions) and `all_spec_codes_covered` agrees. So if a future SPEC code
is added without a seeding fixture — or a fixture is removed — the
bookkeeping forces CI red. 39 distinct codes fire in the `--all` sweep
across 41 issue objects (`SCHEMA_SHAPE_MISMATCH` appears twice — email
shape + date shape) in `validate.json` (the 39th is
`LAYER_TYPE_MISMATCH`, seeded by the misplaced contact under `wiki/`);
the three `DB_MD_*` identity-contract codes are a separate invocation on
the `bad-db-md/` sub-store (`bad-db-md.json`); `NOT_A_STORE` is a
separate invocation (`not-a-store.json`); `POLICY_FROZEN_PAGE` is
write-time (`policy-refusal/`).

| Code | Severity | Seeded by | Issue site |
|------|----------|-----------|------------|
| `NOT_A_STORE` | error | `not-a-store/` (no DB.md) | dir-level — separate invocation |
| `DB_MD_BAD_TYPE` | error | `bad-db-md/DB.md` (`type: notes`) | `type` line 2 — separate invocation |
| `DB_MD_MISSING_FIELD` | error | `bad-db-md/DB.md` (no `owner`) | block top line 1 — separate invocation |
| `DB_MD_UNKNOWN_SECTION` | warning | `bad-db-md/DB.md` (`## Glossary`) | heading line 18 — separate invocation |
| `FM_MISSING_TYPE` | error | `records/misc/no-type.md` | no `type:` key (line 1) |
| `FM_MALFORMED_YAML` | error | `records/misc/malformed-yaml.md` | unparseable block (line 1) |
| `FM_BAD_TIMESTAMP` | error | `sources/emails/2026/05/bad-timestamp.md` | `created` line 4 |
| `LAYER_TYPE_MISMATCH` | warning | `wiki/contacts/misplaced-contact.md` | `type: contact` under `wiki/` (line 2) |
| `SUMMARY_MISSING` | error | `records/misc/summary-absent.md` | no `summary` key (line 1) |
| `SUMMARY_EMPTY` | error | `records/misc/summary-blank.md` | `summary` line 6 |
| `SUMMARY_MULTILINE` | error | `records/misc/summary-multiline.md` | `summary` line 6 |
| `SUMMARY_TOO_LONG` | warning | `records/misc/summary-overlong.md` | `summary` line 6 (247 chars) |
| `WIKI_LINK_SHORT_FORM` | error | `records/contacts/sarah-chen.md` | `[[acme-co]]` line 19 |
| `WIKI_LINK_HAS_EXTENSION` | warning | `records/contacts/sarah-chen.md` | `[[…northstar.md]]` line 20 |
| `WIKI_LINK_BROKEN` | error | `records/misc/broken-link.md` | `[[…/ghost]]` line 19 |
| `WIKI_LINK_AMBIGUOUS` | error | `records/misc/ambiguous-link.md` | `[[northstar]]` line 19 |
| `WIKI_LINK_FLOW_FORM_LIST` | error | `wiki/synthesis/flow-form-list.md` | `derived_from` line 8 |
| `DUP_ID` | error | `records/contacts/dup-id-{one,two}.md` | `id` line 3 |
| `DUP_CONTACT_EMAIL` | warning | `records/contacts/duplicate-email-{a,b}.md` | `email` line 8 |
| `DUP_COMPANY_DOMAIN` | warning | `records/companies/dup-domain-{a,b}.md` | `domain` line 8 |
| `DUP_EXPENSE_TUPLE` | warning | `records/expenses/2026/05/2026-05-05-globex-{a,b}.md` | tuple |
| `DUP_INVOICE_TUPLE` | warning | `records/invoices/2026/04/2026-04-01-northstar-{a,b}.md` | tuple |
| `DUP_EMAIL_REINGEST` | warning | `sources/emails/2026/05/2026-05-22-renewal-{a,b}.md` | tuple |
| `DUP_MEETING_TUPLE` | warning | `records/meetings/2026/05/2026-05-22-sync-{a,b}.md` | tuple |
| `SCHEMA_MISSING_REQUIRED` | error | `records/contacts/missing-company.md` | absent `company` |
| `SCHEMA_SHAPE_MISMATCH` | error (×2) | `records/contacts/bad-email-shape.md` (`email` line 8) + `records/expenses/2026/05/bad-date-shape.md` (`date` line 7) | two fixtures: email shape + date shape |
| `SCHEMA_LINK_PREFIX_MISMATCH` | error | `records/contacts/plain-company.md` | `company` line 9 |
| `SCHEMA_ENUM_VIOLATION` | error | `records/invoices/2026/04/bad-status-enum.md` | `status` line 10 |
| `POLICY_FROZEN_PAGE` | error | `policy-refusal/*.json` (write-time) | frozen path |
| `POLICY_IGNORED_TYPE_PRESENT` | info | `records/scratch/throwaway.md` | `type: test` line 2 |
| `POLICY_IGNORED_TYPE_DERIVED` | warning | `wiki/synthesis/derived-from-ignored.md` | `derived_from` line 9 |
| `LOG_BAD_TIMESTAMP` | error | `log.md` | entry line 13 |
| `LOG_UNKNOWN_KIND` | warning | `log.md` | entry line 17 (`frobnicate`) |
| `LOG_OUT_OF_ORDER` | warning | `log.md` | entry line 23 |
| `INDEX_MISSING` | error | `records/misc/` (no index.md) | folder-level |
| `INDEX_STALE_ENTRY` | error | `records/companies/index.md` | line 13 (`ghost-corp`) |
| `INDEX_MISSING_ENTRY` | error | `records/contacts/index.md` | `sarah-chen` omitted |
| `INDEX_ORPHAN` | warning | `wiki/people/index.md` | empty folder |
| `INDEX_WRONG_SCOPE` | warning | `records/decisions/index.md` | `scope` line 3 |
| `INDEX_SUMMARY_MISMATCH` | error | `records/contacts/index.md` | line 12 (`bad-email-shape`) |
| `INDEX_JSONL_DESYNC` | error | `sources/emails/index.jsonl` | 2 entries / 3 files |
| `INDEX_JSONL_MISSING` | error | `records/playbooks/` (index.md present, no index.jsonl) | folder-level |
| `INDEX_JSONL_STALE` | error (×2) | `records/processes/index.jsonl` (`summary` vs `invoicing.md`) + `records/notes/index.jsonl` (`tags` vs `malformed-tags.md`) | any projected field, not just summary/type: the second fixture's sidecar keeps the nested `tags` a rebuild would normalize away |
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
   (`records/companies/northstar.md` + `wiki/companies/northstar.md`);
   the short-form fixture (`[[acme-co]]`) matches zero. This lets both
   codes own a non-overlapping fixture, matching SPEC's wording
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
   `corpus-b-edges/DB.md` across the canonical `sources/`/`records/`/
   `wiki/` layers and does not descend into the non-canonical
   `not-a-store/` sibling. `NOT_A_STORE` is only producible by pointing
   `dbmd validate` directly at `not-a-store/`.
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
    `sources/`/`records/`/`wiki/` under the root, so it never descends
    into `bad-db-md/` — exactly the isolation `not-a-store/` relies on.
11. **`LAYER_TYPE_MISMATCH` fires on a placement, not a defect.** The
    `wiki/contacts/misplaced-contact.md` fixture is a fully valid
    `contact` (schema satisfied, summary + timestamps valid, its one
    wiki-link resolves) whose ONLY anomaly is its layer: a `contact`
    belongs under `records/`, so sitting under `wiki/` is the single
    warning it fires. Its type-folder gets a complete, matching
    `index.md` + `index.jsonl` so no `INDEX_*` co-fires; layer/root
    index completeness is unchecked (rule #6), so `wiki/index.md` need
    not list it.

## What MUST be true (the invariants, restated)

- `dbmd validate --all --json <store>` emits **exactly** the 41 issues
  in `validate.json` (39 distinct codes; `SCHEMA_SHAPE_MISMATCH` twice)
  — no more (no spurious issues on the deliberately clean link targets
  and clean indexes), no fewer.
- Exit code is **non-zero** (there are 25 errors).
- Every `error` blocks; `warning`/`info` do not change the exit code on
  their own — but here errors dominate, so exit is non-zero regardless.
- `dbmd validate --all --json <store>/bad-db-md` emits **exactly** the
  three `DB_MD_*` issues in `bad-db-md.json` (2 errors + 1 warning) and
  exits non-zero; the main sweep emits none of them.
- The clean files exist precisely to catch false positives:
  `records/companies/northstar.md`, `wiki/companies/northstar.md`,
  `records/decisions/2026-q1-strategy.md`, `wiki/contacts/misplaced-contact.md`
  (its links + schema are clean — only the layer warns), every clean
  type-folder `index.md`/`index.jsonl`, and the first two well-formed
  `log.md` entries must produce **no** other issue.
