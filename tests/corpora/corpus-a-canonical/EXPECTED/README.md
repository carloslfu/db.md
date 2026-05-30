# corpus-a-canonical — EXPECTED contract

This directory is the **intent-derived** golden for the canonical happy-path
store. It is the mirror image of `corpus-b-edges/EXPECTED/` (designed-to-fail):
where corpus-b pins exactly which issues a deliberately-broken store must emit,
corpus-a pins what a *clean, well-formed* store must produce — zero validation
issues, the exact set of files each search must match, the exact catalog a
rebuild must write, and the exact graph relationships its wiki-links encode.

Every value here is **hand-derived from `SPEC.md` plus the corpus content**
(which file links which, which `type`/`summary`/timestamp each frontmatter
carries) — never copied from whatever `dbmd` happens to emit. A test whose
expected value is the code-under-test's own output is vacuous; these files are
the independent source of truth the tool is measured against. Each golden also
carries an inline `_comment` restating its own derivation; this README is the
index over them.

The executable contract that asserts these goldens spans two test files, both
driving the **real `dbmd` binary** via `assert_cmd`:
`crates/dbmd-cli/tests/e2e_corpus_a.rs` (validate / search / index / round-trip)
and `crates/dbmd-cli/tests/e2e_corpus_a_graph_log.rs` (graph / log). Re-run them
after any change that could move that output: `cargo test -p dbmd-cli --test
e2e_corpus_a --test e2e_corpus_a_graph_log`.

## Files

| File | What it asserts | How to produce / verify |
|------|-----------------|-------------------------|
| `validate.json` | The complete `issues` array for the full store sweep — an **empty list** (`[]`). The canonical store is the happy path: a SWEEP must find nothing and exit `0`. | `dbmd validate --all --json tests/corpora/corpus-a-canonical` → `issues == []`, all summary tallies `0`. |
| `search.json` | One golden case per query (**10** cases): each lists the complete set of store-relative paths that query MUST match, plus the `--type` / `--in` / `--updated-after` flags. The e2e test runs every case through the binary in both `--json` and text (`file:line:text`) modes and asserts the matched **file set** equals `matches`. | `dbmd search <query> <args> --json --dir tests/corpora/corpus-a-canonical`, collapse per-line matches to files. |
| `graph.json` | Golden for `dbmd graph backlinks` and `dbmd graph neighborhood` over representative seeds: the **backlink** sets (incoming edges) and the **neighborhood** walks (each reached node's `hops` / `direction` / `via` / `type`) the corpus's wiki-links encode — resolved in both frontmatter and body, content files only (meta files are never nodes/edges). `e2e_corpus_a_graph_log.rs` asserts backlinks equal each `matches` array (and excludes catalog/meta paths), asserts the neighborhood node set keyed `path → {hops, direction, via, type}` equals the golden, and audits that each node hydrates the **neighbor's own** `summary` and that its `via`/`direction` edge really exists (cross-checked via `forwardlinks`). | `dbmd graph backlinks <path> --json` / `dbmd graph neighborhood <path> --hops N --json` against the corpus. |
| `log-tail.json` | Golden for `dbmd log tail 20 --json`, normalized. The active log holds **12** entries (no rotation), so `tail 20` is the whole log; the golden is the full ordered array of `{timestamp, kind, object, note}`, **chronological (oldest first)**. It pins the SPEC object-slot encoding: a per-file action carries the raw `[[…]]` wiki-link form, the store-wide `index-rebuild` carries the literal `"-"`, and the bare `validate` (no object slot) carries JSON `null`. | `dbmd log tail 20 --json --dir tests/corpora/corpus-a-canonical`, normalize each entry to those four keys. |
| `index/` | A byte-exact snapshot of the **entire derived index catalog** a from-scratch rebuild must write. See the section below. | `dbmd index rebuild --dry-run` from the corpus root. |

## `index/` — the rebuild snapshot

`index/` mirrors the store tree: for every level it holds the exact `index.md`
(and, for type-folders, the complete `index.jsonl` twin) that
`dbmd index rebuild` must emit. The e2e test parses `index rebuild --dry-run`,
audits each artifact against the SPEC §"`index.md` and `index.jsonl`" format
rules, then asserts each is **byte-identical** to its committed golden here.

| Path | Role |
|------|------|
| `index/ARTIFACTS.txt` | The locked **emission manifest**: every artifact path a full rebuild emits, in the exact order it emits them (type-folders depth-first, then the layer rollup, then the root rollup last). The test asserts the dry-run's path list equals this, set **and** order. |
| `index/index.md` | The **root** rollup (`scope: root`): per-layer counts, links to each layer index. Not a file listing. |
| `index/<layer>/index.md` | A **layer** rollup (`scope: layer`, e.g. `records/`): per-type-folder counts + a one-line lead, links to each type-folder index. Not a file listing. |
| `index/<layer>/<type>/index.md` | A **type-folder** browse view (`scope: type-folder`): one `- [[…]] — <summary>` entry per file, capped at **500** (`## More` overflow footer only when over the cap — corpus-a's largest folder is 490, so no golden here carries `## More`; the over-cap branch is exercised by a synthetic 501-file store in the same test). |
| `index/<layer>/<type>/index.jsonl` | The **complete, uncapped twin**: exactly one valid-JSON object per `.md` file, each carrying the universal keys (`path`, `type`, `summary`, `tags`, `links`, `created`, `updated`). The `index.md` summaries are quoted verbatim from these records (no recomputation). |

Index `scope` levels, restated: **root** and **layer** indexes are count
rollups (no per-file entries, no cap); only **type-folder** indexes list files
and are subject to the 500-cap + `## More` rule and the summary-verbatim rule.

## Relationship to the `.gen-*.py` fixtures

The corpus root holds two generators (`.gen-index.py`, `.gen-expenses.py`).
**These are not goldens and do not live here** — they build parts of the
*input store* (the near-cap 490-record `records/expenses/` folder, and the
store's own in-tree `index.md`/`index.jsonl` catalog so `validate --all`'s
`INDEX_*` checks pass). They mirror the documented render rules in
`crates/dbmd-core/src/index.rs` and the deterministic `summary` composition in
`dbmd-core::summary`, so the corpus is internally consistent. The goldens under
`EXPECTED/` are the *output contract* the binary is held to; the generators
populate the *input* the binary reads.

## What MUST be true (the invariants, restated)

- `dbmd validate --all --json <store>` emits **zero** issues and exits `0`
  (`validate.json` is `[]`). The canonical store has no false positives to trip.
- Every `search.json` case returns **exactly** its `matches` file set — no more
  (no spurious matches in meta files `DB.md`/`index.md`/`index.jsonl`/`log.md`,
  which are never content), no fewer — in both `--json` and text mode.
- `dbmd index rebuild --dry-run` emits **exactly** the artifacts in
  `index/ARTIFACTS.txt`, in that order, each **byte-identical** to its committed
  `index/<path>` golden, and each passing the SPEC cap / `## More` / jsonl-
  completeness / summary-verbatim audit.
- A `fm set summary` round-trip on a record preserves the body byte-for-byte,
  applies the new summary, and writes it through to the type-folder
  `index.jsonl` (asserted in the same e2e test, against a temp copy so the
  committed corpus is never mutated).
- `dbmd graph backlinks <seed> --json` returns **exactly** each `graph.json`
  backlink set (sorted, bare wiki-link form, no `.md`, no catalog/meta paths);
  an orphan seed returns the empty set and exits `0`.
- `dbmd graph neighborhood <seed> --hops N --json` returns **exactly** the
  golden node set (by `path → {hops, direction, via, type}`), every node
  hydrating the **reached neighbor's own** `summary` (never the seed's), every
  `via` a real hop-`(n-1)` parent with a real edge in the stated direction; an
  orphan seed hydrates to `{seed, nodes: []}` and exits `0`.
- `dbmd log tail 20 --json` returns all **12** entries in chronological order,
  each normalizing byte-for-byte to its `log-tail.json` entry — including the
  object-slot encoding (`[[…]]` for per-file actions, `"-"` for the store-wide
  `index-rebuild`, `null` for the bare `validate`). A smaller `tail N` returns
  the newest `N`, still oldest-first.
