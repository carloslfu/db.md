# Changelog

All notable changes to db.md are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); db.md uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Two things version independently:

- **The format** (`SPEC.md`) — **v0.2** (v0.1 was the first tagged release).
- **The toolkit** (the `dbmd` binary, `crates/`) — versioned in
  `Cargo.toml`, currently **v0.2.4**.

## [Unreleased]

### Toolkit

#### Added

- **`dbmd install-skill`** — install a coding-agent skill that teaches Claude
  Code (`~/.claude/skills/db-md/SKILL.md`) or Codex
  (`~/.codex/instructions/db-md.md`) to operate a db.md store with `dbmd`. The
  persistent sibling of `dbmd spec`: where `spec` loads the contract for one
  session, `install-skill` drops a skill the agent keeps across sessions.
  Autodetects the target (`--target claude-code|codex` to force one),
  `--uninstall` removes it. The skill body points at `dbmd spec` as the single
  source of truth — it never inlines the SPEC, so it cannot drift from it.

### Format — v0.2 (breaking: the type model is now generic)

The spec no longer ships a built-in type vocabulary. `type` is a free-form
label, and schema enforcement comes solely from the store's own
`DB.md ## Schemas`. The `contact` / `expense` / … types are now illustrative
**examples**, not normative. **Migration:** a store that relied on the old
implicit schemas (e.g. `contact.company` enforced as a `records/companies/`
link, or the type-specific dedup) must declare those rules explicitly in
`## Schemas` — copy the example schema pack from SPEC § Example types.

#### Added

- **`unique:` schema directive** — declare a uniqueness constraint over one or
  more fields (`- unique: email` / `- unique: date, amount, vendor`);
  collisions warn as the new generic `DUP_UNIQUE_KEY` code. Wiki-link fields
  compare by target; list fields compare as a sorted set.
- **`summary_template:` schema directive** — a `{field}`-interpolation pattern
  for a type's default `summary` (e.g. `summary_template: {role} at {company}`),
  replacing the old built-in per-type composers.

#### Removed

- The implicit / built-in per-type schemas — no type carries an enforced schema
  unless `## Schemas` declares it.
- Seven validation codes: `LAYER_TYPE_MISMATCH` and the six type-specific
  collisions (`DUP_CONTACT_EMAIL`, `DUP_COMPANY_DOMAIN`, `DUP_EXPENSE_TUPLE`,
  `DUP_INVOICE_TUPLE`, `DUP_EMAIL_REINGEST`, `DUP_MEETING_TUPLE`) — superseded by
  the schema-driven `DUP_UNIQUE_KEY`. The live SPEC table now has 38 codes.
- The hard-coded per-type `summary` composers, and the `dbmd stats`
  recognized-vs-custom type split (every type is now the store's own).

#### Changed

- Folder placement is no longer enforced by type (`LAYER_TYPE_MISMATCH` is
  gone); the three-layer layout stays a convention.

Toolkit impact: this is a breaking 0.x change — recommend bumping the crate to
**0.3.0** at release.

## [0.2.4] — 2026-06-01

- **Release process documented.** Added `RELEASING.md` (a cold-start release
  runbook) and `AGENTS.md`, and referenced the tagged `SPEC.md` v0.1 from the
  README. The `crates-io` publish environment no longer requires a manual
  approval click (solo maintainer). No functional changes to the toolkit.

## [0.2.3] — 2026-05-30

- **First release published from CI via Trusted Publishing.** Both
  crates are published by the `release.yml` GitHub Actions workflow on a
  version tag, using crates.io Trusted Publishing (OIDC, no stored API
  token), with SLSA build-provenance attestations on the release
  binaries. No functional changes to the toolkit. See
  [RELEASING.md](RELEASING.md).

## [0.2.2] — 2026-05-30

- **Crate READMEs.** `dbmd-core` and `dbmd-cli` now ship `README.md`
  files (with the `readme` field set) so their crates.io pages render
  documentation. No functional change to the toolkit.

## [0.2.1] — 2026-05-30

- **Self-contained standard.** db.md stands alone with no external
  project dependency: the spec, the `dbmd` toolkit, and the docs make
  no reference to any other standard or platform.
- **Vendor-neutral distribution.** Install via `cargo install dbmd-cli`,
  the Homebrew tap (`brew install carloslfu/tap/dbmd`), or the prebuilt,
  checksummed, provenance-attested tarballs on the GitHub releases page.
- **Security reporting** via GitHub private vulnerability reporting.

## [0.2.0] — 2026-05-29

The all-Rust rewrite. db.md becomes a single deterministic binary with
zero AI dependencies, and the store model settles into three layers
plus one config file.

### Added

- **One Rust binary, `dbmd`** (git / cargo / kubectl shape) doing every
  db.md-specific file/data operation: read, write, search, validate,
  extract, graph, index, log.
- **Embedded ripgrep** via the `grep` + `ignore` crates — fast search
  with no separate `rg` to install and no shelling out.
- **Built-in document extraction** (`dbmd extract`) for PDF, docx,
  xlsx, epub, and html via permissively-licensed Rust crates — no GPL
  `pdfgrep`, no AGPL `rga`.
- **`dbmd-core` library crate.** All logic lives in the library; the
  binary is thin arg-parse/format wrappers. `cargo add dbmd-core` to
  build db.md-aware Rust tools.
- **`records/` layer.** The store is now three layers — `sources/`
  (raw evidence), `records/` (atomic typed data), `wiki/`
  (curator-synthesized narrative).
- **Single `DB.md` config file** with parseable, validated sections:
  `## Agent instructions`, `## Policies` (`### Frozen pages`,
  `### Ignored types`), and `## Schemas` (`### <type>` field
  definitions with `required` / shape / `link to` / `default` / `enum`
  modifiers). Frozen-page writes are refused by `dbmd validate`.
- **Hierarchical `index.md` catalog**, maintained write-through by the
  write commands, with a 500-entry cap per node and a `## More`
  overflow footer.
- **Append-only `log.md`** with monthly rotation into
  `log/<YYYY-MM>.md`.
- **Required `summary` frontmatter field** on every content file — the
  single source of truth each `index.md` reads to build its catalog.
- **Six-step agent session lifecycle** and the full curator contract,
  documented in `SPEC.md` (§ The agent session, § The curator
  contract).
- **O(changed) vs. O(store) discipline.** Loop ops (search, fm,
  backlinks, write, log tail, working-set `validate`) stay flat as the
  store grows; sweep ops (`validate --all`, `index rebuild`, `stats`,
  whole-graph queries) run off the interactive loop. Performance
  budgets are baked into the toolkit contract.
- **Distribution**: a crates.io crate (`cargo install dbmd-cli`), a
  Homebrew tap (`brew install carloslfu/tap/dbmd`), and prebuilt,
  checksummed, provenance-attested tarballs on the GitHub releases page.
- **`dbmd spec`** prints the bundled canonical spec — install the
  binary, run `dbmd spec` to read the standard and load it into an
  agent harness's system prompt.
- **Mechanical license + zero-AI enforcement.** `cargo deny` over the
  whole resolved tree plus a `license_policy` test over the shipped
  closure: MIT / Apache-2.0 / BSD / Unlicense / MPL / Zlib /
  Unicode-3.0 only, and a banned-crate list covering provider SDKs and
  every embeddings / vector / ANN crate.

### Changed

- **The agent harness is bring-your-own.** "Curator" is a role any
  agent (Claude Code, Codex, a custom loop) plays by reading the spec
  and driving `dbmd` subcommands. db.md ships no LLM runtime and no API
  keys.
- **Wiki-links require the full store-relative path**
  (`[[records/contacts/sarah-chen]]`). Short-form links are now a
  validation error.
- **Atomic typed data moved from `wiki/<plural>/` to
  `records/<plural>/`.** The `wiki/` layer is now narrative synthesis
  only; the typed rows live in `records/`.

### Removed

- **The Go toolchain.** The five Go binaries (`dbmd`, `dbmd-curator`,
  `dbmd-file-watcher`, `dbmd-email-imap`, `dbmd-mcp-fetcher`), the Go
  `parser` package, `go.mod` / `go.sum`, and the v0.1 reference
  ingesters are gone.
- **The `dbmd-curator` binary and any LLM backend.** Curation is the
  agent's job using `dbmd` primitives — no curator binary, no
  `dbmd curate` subcommand, no `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`
  handling anywhere in the toolkit.
- **The reference ingesters.** Getting data in is "land a file under
  `sources/`, then `dbmd fm init`" composed with the tools you already
  have (`mbsync`, `rsync`, `curl`, cron), plus `dbmd write` for
  tool-produced text.
- **The `rules/` folder.** Its `curator.md`, `policies.md`, and
  `schemas/` content folds into the single `DB.md` config file.
- **The curator + ingester `docker-compose.yml`** (it ran the dropped
  binaries with provider API keys).

## [0.1.0]

The original Go reference implementation: the `dbmd` CLI plus the
`dbmd-curator` / `dbmd-file-watcher` / `dbmd-email-imap` /
`dbmd-mcp-fetcher` binaries, a `sources/ wiki/ rules/` store model, and
a Go `parser` package. Superseded by 0.2.0.

[0.2.0]: https://github.com/carloslfu/db.md/releases/tag/v0.2.0
[0.1.0]: https://github.com/carloslfu/db.md/releases/tag/v0.1.0
