# dbmd-core

The reference library for **db.md, the open standard for databases in plain files**.

`db.md` is a database made of markdown files: records are markdown with
YAML frontmatter, relationships are wiki-links, the directory is the
database, frontmatter carries structured fields, `DB.md` declares schemas,
and an agent is the query engine. `dbmd-core` is the Rust library that
implements every db.md operation; the
[`dbmd`](https://crates.io/crates/dbmd-cli) binary is a thin command-line
wrapper over it.

## What it provides

- **Parser** — frontmatter (YAML) + body, wiki-link extraction, section parsing.
- **Store** — store walk, date-sharding, type-folder enumeration, link lookup.
- **Graph** — backlinks, forwardlinks, neighborhood hydration, orphans (on-demand; no maintained graph).
- **Validate** — the full structured issue-code vocabulary (frontmatter, links, schema, policy, index integrity).
- **Query** — type / layer / field filters resolved against the `index.jsonl` sidecars.
- **Index + log** — write-through `index.md` (capped human browse) and complete `index.jsonl` machine twin; month-rotating append-only log.
- **Summary, stats, render, extract** — deterministic summaries, store stats, tree/outline structures, and document text extraction (PDF/docx/xlsx/epub/html).

## Design

- **Zero AI dependencies.** No model calls, no embeddings, no vectors — ever. The intelligence lives in the caller's agent; this library is the deterministic, fast tool it drives.
- **Embedded ripgrep** via the `grep` + `ignore` crates — no separate binary, no shelling out.
- **Permissive licensing only** under the allowlist in the repo root
  (`MIT`, `Apache-2.0`, BSD variants, `0BSD`, `Unlicense`, `MPL-2.0`, `Zlib`,
  `Unicode-3.0`).

## Usage

```toml
[dependencies]
dbmd-core = "0.3.5"
```

```rust
use dbmd_core::Store;

let store = Store::open("path/to/store")?;
// walk, validate, query, build indexes — see the API docs.
```

Full API documentation: <https://docs.rs/dbmd-core>.

## License

Apache-2.0. Copyright 2026 Carlos Galarza. See `LICENSE` and `NOTICE`.
