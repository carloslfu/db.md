# Changelog

All notable changes to db.md are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); db.md uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Two things version independently:

- **The format** (`SPEC.md`) — **v0.2** (v0.1 was the first tagged release).
- **The toolkit** (the `dbmd` binary, `crates/`) — versioned in
  `Cargo.toml`, currently **v0.3.9**.

## [0.3.9] — 2026-06-14

A launch-readiness correctness and security release. An adversarial code
review of the toolkit surfaced reproduced defects across the codebase; this
release fixes all of them, plus the issues a follow-up adversarial review of
the fixes themselves found. The format is unchanged (still v0.2), and the
public `dbmd-core` API stays backward-compatible (only additive helpers).

### Fixed

**Silent data loss (critical):**

- `dbmd write` to a type-folder `index.md` / `index.jsonl` no longer lets the
  write-through catalog destroy the just-written record; reserved catalog
  filenames are refused at the write surface, at any folder depth.
- `dbmd index rebuild` no longer deletes user content files named `index.md`
  inside date shards, and an abort on one malformed file no longer destroys
  existing catalogs or leaves the store in a permanently unfixable validation
  state.
- A `dbmd log` note whose line looks like an entry header
  (`## [<date>] <kind> | <obj>`) can no longer fabricate phantom entries or
  corrupt the append-only log on rotation — fixed both at the write path
  (escaping) and in the reverse reader (a block-boundary header-scan bug).

**Store→host security boundary:**

- `dbmd search --type` / `--where` no longer reads files outside the store via
  a crafted `index.jsonl` sidecar path (path traversal / exfiltration).
- `dbmd write` and `dbmd extract --out` no longer write outside the store
  through a symlinked directory anywhere in the path (parent included).
- Graph traversal and `dbmd stats` no longer dereference `..` wiki-link
  targets outside the store root.

**Frontmatter integrity:**

- Sequence/mapping values on universal keys (`summary`, `status`, `type`,
  `tags`) and non-string YAML keys now round-trip verbatim on rewrite instead
  of being silently deleted or rewritten to debug form; nested plain string
  lists are no longer fabricated into wiki-links; a fence written with a
  trailing space is read consistently across every surface.

**Validation correctness:**

- Null / non-scalar `created`, `updated`, and schema `required` values are now
  caught instead of bypassing checks; unreadable (non-UTF-8) files are
  reported rather than silently passing; wiki-links inside fenced code are no
  longer flagged; `validate --all` no longer skips in-layer `log/` folders;
  links to existing non-`.md` source files are no longer false-flagged as
  broken; `Shape::Url` / `Shape::Email` edge cases corrected.

**Graph / index / extraction / query / render:**

- Link-edge detection now agrees across graph, `stats`, `rename`, and
  `validate` on fenced code, letter case, and surrounding whitespace, so
  `rename` no longer rewrites fenced documentation examples or misses real
  links, and backlinks no longer over- or under-report.
- Index rollup `(N)` counts stay consistent between full rebuild and
  write-through; multi-line summaries no longer corrupt `index.md`.
- docx / EPUB / spreadsheet extraction preserves XML entities, percent-encoded
  hrefs, spreadsheet dates, and literal bracketed / `#` text.
- `query --type`, `tree --type`, default-summary heading detection, and
  `sections` / `outline` source-relative line numbers corrected.
- `updated` is now auto-maintained on `fm set`, `link`, and `rename`.

**Filesystem / packaging:**

- `write_atomic` preserves destination file permissions instead of resetting
  them; `install.sh` upgrades atomically (no truncated binary on a
  cross-filesystem move) and honors `DBMD_BASE_URL`; a flaky macOS
  test-harness mtime guard was removed; the runtime `log.md.lock` advisory
  lock is git-ignored.

## [0.3.8] — 2026-06-13

A docs-in-binary release. No code behavior changes.

### Changed

- Setup guidance now makes a store **version-controlled by default**. The
  README quick-start prompt, `llms.txt`, and `dbmd spec` (embedded SPEC §
  "Creating a store") tell an agent to create the store inside the current
  repo when its data lives there, otherwise `git init` the store or use a
  synced folder — never to drop it at a bare, unversioned global path, and
  never to move repo-owned data out without the operator's say-so. A
  machine-global `~/db` is positioned as a symlink to the real, versioned
  store, not as the store's bare home. This is a design default, not a
  preference; the operator opts out explicitly for a throwaway store. The
  format is unchanged (still v0.2).

## [0.3.7] — 2026-06-11

### Changed

- `dbmd write` now uses a reusable `dbmd-core` create-new durable writer
  (`write_atomic_new`) instead of a CLI-level empty sentinel. Existing
  `PATH_COLLISION` behavior is unchanged, but the collision guarantee now lives
  beside the core atomic write primitive and no placeholder file is created.

## [0.3.6] — 2026-06-10

A docs-in-binary release. No code behavior changes.

### Changed

- `dbmd spec` prints the current contract: the embedded SPEC picks up the
  stack-collapse thesis, the agent-operated files-as-database framing, the
  sharded write surface, and the claims audit that landed since 0.3.5. The
  same tightening runs across the README, llms.txt, and the crate summaries
  shipped to crates.io.
- `dbmd --help` and the installer header carry the current tagline, "the open
  standard for databases in plain files", matching the README, SPEC, llms.txt,
  and crate descriptions.
- README: the quick start is now a prompt you hand to an agent, and a new
  "Safe to paste" section documents the verifiable install chain (SHA256SUMS
  verification, CI-from-tag builds, signed build-provenance attestations,
  Trusted Publishing). llms.txt carries the same audit story.

## [0.3.5] — 2026-06-09

A correctness-and-robustness release. An adversarial multi-agent review of the
toolkit surfaced 27 confirmed defects (7 high, 11 medium, 9 low); all are fixed
here, each guarded by a regression test that reconstructs the trigger and fails
against the prior code. No behavior changes outside the named bugs.

### Fixed

- **`dbmd format` silently deleted universal frontmatter fields.** A
  `type`/`id`/`summary`/`status` written as a bare YAML scalar that parses as a
  number, bool, or null (e.g. `id: 100`, `summary: 2026`, `status: 0`) was
  dropped on parse and erased on the next `format`, while `validate` reported
  the file clean. The parser now coerces these to their string form the way
  `validate`/`store`/`index` already do, so all surfaces agree and the field
  round-trips.
- **`validate --all` false-failed on a clean store.** Any `summary` containing
  ` · ` (a middle dot) tripped a spurious `INDEX_SUMMARY_MISMATCH` (exit 6); the
  index-entry summary is now matched against the renderer's real, double-spaced
  `  ·  #tag` suffix instead of the first ` · `.
- **Log month-rotation could duplicate entries.** A crash or I/O error between
  the archive write and the active-file trim, followed by the natural retry,
  permanently duplicated the prior month's entries. Rotation is now
  crash/retry-idempotent.
- **`dbmd extract` could be OOM-killed by a crafted spreadsheet.** A small
  `.xlsx`/`.ods`/`.xlsb` declaring a huge sheet range forced an unbounded dense
  allocation. The spreadsheet adapter now bounds it, returning a typed error on
  untrusted `sources/` input.
- **`--type` queries dropped records in non-canonical type-folders;** **`graph
  backlinks --type` under-reported;** **structured `search` aborted on a single
  stale sidecar entry;** **`rename` left a half-applied state on partial
  failure** (now rolls back). Plus medium/low fixes across working-set and
  post-rotation validation, the `graph neighborhood` traversal bound, write
  TOCTOU, `write_atomic` temp-file cleanup, summary truncation on UTF-8
  boundaries, and several CLI flag-placement / exit-code / help-text mismatches.

### Changed

- **Cross-module consistency.** A leading UTF-8 BOM is now tolerated uniformly
  by the parser, validator, and graph frontmatter readers (previously only some
  accepted it); `dbmd search` decodes matched lines lossily so a single invalid
  UTF-8 byte can no longer abort a scan; and `dbmd graph neighborhood` routes
  `--limit` (default 200) into the bounded traversal rather than only truncating
  the printed result.

## [0.3.4] — 2026-06-09

### Fixed

- **Broken-pipe panic when a reader closed `dbmd`'s output early.** Piping a
  command into a consumer that exits first (`dbmd spec | head`, `dbmd search …
  | grep -q`) made `print!`/`println!` panic on the closed pipe and exit `101`
  with a Rust backtrace. `dbmd` now stops cleanly with exit `0` when the reader
  on its stdout has gone away, the standard Unix behavior for a producer whose
  consumer has left. The v0.3.3 release smoke test caught it (`dbmd spec | head
  -20` under `set -o pipefail`); a regression test (`tests/broken_pipe.rs`) now
  locks the behavior in.

## [0.3.3] — 2026-06-04

### Docs

- **Explicit agent flow in every surface.** The skill, the SPEC
  (§ "How an agent uses db.md"), and `llms.txt` now open with the four-move
  path — discover → `dbmd spec` → `DB.md` → operate — so the path is
  unmistakable the moment an agent reads any of them.
- **Corrected store creation: the agent writes `DB.md`; there is no `dbmd init`.**
  The SPEC, README, and CLI README documented `dbmd fm init DB.md` as the way to
  initialize a store, but that command refuses on a directory with no `DB.md`
  (chicken-and-egg), and store creation is agent/operator-authored **by design**,
  not a tool command. Replaced the bogus one-liner with the real method (write a
  `DB.md` with `type: db-md` + `scope` + `owner`) and stated the thin-tool
  principle explicitly: `dbmd` plumbs (validate / index / query / link) and never
  scaffolds what a capable agent authors. Also documented that `scope`/`owner`
  are required (enforced by `dbmd validate --all`), which the SPEC understated.

## [0.3.2] — 2026-06-04

### Removed

- **`dbmd install-skill` / `dbmd uninstall-skill`.** The installer is text:
  `dbmd spec` + the repo-root `llms.txt` are the contract, and the open-format
  skill ships in the repo at `skills/db-md/SKILL.md`. Placing that skill is
  generic file work — copy it, use your harness's own skill installer (Codex's
  `skill-installer`, a Claude Code plugin), or tell your agent to. db.md no
  longer ships per-harness install code: it was the one thing coupled to harness
  internals (and the thing that broke when Codex moved its skills directory). The
  mechanism is generic text plus a capable model — nothing to maintain or drift.

### Docs

- **Repo-root `llms.txt`** — an agent-readable entry point at the top of the
  repo, in the [llms.txt](https://llmstxt.org) spirit: the installer is text. An
  agent (or a human) reads one plain file to learn what db.md is and how to
  install, integrate, and operate a store.
- **Docs reframed around the text path.** README, TOOLS.md, SPEC.md, and
  `llms.txt` present one model: `dbmd spec` is the single source of truth;
  persistence is a skill *file* you place (or your agent / harness installer
  places), not a `dbmd` command. Nothing inlines the SPEC, so nothing drifts.

### Fixed

- Preserved the declared Rust 1.85 MSRV, end to end. The direct `zip` dependency
  stays on the 7.2 line (`zip` 8.x now requires Rust 1.88), and the test suite
  again compiles on 1.85 — a handful of `PathBuf == *"…"` comparisons needed
  `PartialEq<str> for PathBuf`, which std only added after 1.85, so they now use
  `Path::new(…)`. Verified with a real `cargo +1.85 build` and `test`.

## [0.3.1] — 2026-06-03

A world-class hardening pass — a deep adversarial audit of every core module
(parser, validate, store, index, log, graph, stats, summary) plus the two gaps
left open at 0.3.0. Every finding was adversarially verified and fixed with a
regression test; the toolkit stays clippy-clean (`-D warnings`) and
`unsafe`-free.

### Format (additive to v0.2)

- **`shard: by-date | flat` schema directive** — on a `### <type>` block in
  `DB.md ## Schemas`, declare whether that type's records are date-sharded on
  disk or kept flat. It overrides the built-in default, so a custom event type
  opts into sharding the generic v0.2 way, and any type can force flat.

### Toolkit

#### Fixed

- **Schema validation no longer silently accepts a non-scalar value.** A shape-
  or enum-constrained field holding a YAML list or mapping now flags
  `SCHEMA_SHAPE_MISMATCH` instead of skipping the check entirely.
- **Frontmatter parsing no longer panics** on a YAML-tagged top-level mapping
  (a `!tag` on the frontmatter) — it is handled, never aborts the process.
- **`validate` flags a type-folder `index.md` entry that is missing its summary
  text** when the file has a `summary` (`INDEX_SUMMARY_MISMATCH`), per the SPEC.
- **Content files named `log.md` / `DB.md` inside a layer are no longer dropped**
  from store walks — those names are reserved only at the store root, so a
  `records/…/log.md` is real content to `validate` / `index` / `stats`.
- **`dbmd stats` orphan count now agrees with `dbmd graph orphans`** for
  self-linking files (a self-link is not a graph edge in either surface).
- **`summary_template` interpolates `{tags}` / `{created}` / `{updated}`** — the
  typed universal fields, which previously rendered empty.
- **A bare `enum` schema modifier** (`enum, a, b`, no colon) no longer includes
  the keyword `enum` itself as an allowed value.
- **`dbmd index rebuild --folder` cascades to the layer and root rollups**
  instead of leaving stale counts a later `validate` would flag as an index
  desync — consistent with `rebuild` and the write-through path.
- **`index.jsonl` paths are written OS-independently** (forward slashes), so the
  catalog is byte-portable across platforms (a Windows-written store cloned onto
  POSIX and vice versa).

#### Internal

- The atomic, **durable** write for primary data (content records, `log.md` and
  its archives, link rewrites) is now one shared `dbmd_core::write_atomic`
  primitive (temp file + fsync + rename + parent-dir fsync) instead of four
  near-identical copies. The rebuildable `index.md` / `index.jsonl` keep their
  intentionally lighter, atomic-but-not-fsync write — a crash-lost catalog entry
  is recovered by `dbmd index rebuild`, so a per-write fsync there would be cost
  without benefit.

## [0.3.0] — 2026-06-03

### Toolkit

#### Added

- **`dbmd install-skill`** / **`dbmd uninstall-skill`** — install (or remove) the
  cross-agent [Agent Skill](https://agentskills.io) that teaches a local coding
  agent to operate a db.md store with `dbmd`. One source, every agent: the skill
  is authored once at `skills/db-md/SKILL.md`, embedded in the binary, and dropped
  into each agent's skills dir in the open `SKILL.md` format — Claude Code
  (`~/.claude/skills/db-md/SKILL.md`) and Codex (`~/.codex/skills/db-md/SKILL.md`),
  the same file, frontmatter and all. With no `--target` it points every detected
  agent in one command (`--target claude-code|codex` narrows to one). The
  persistent sibling of `dbmd spec`: where `spec` loads the contract for one
  session, `install-skill` drops a skill the agent discovers on every future
  session, and `uninstall-skill` removes exactly what it wrote (preserving any
  user-created siblings). The skill body is a thin pointer that runs `dbmd spec`,
  the single source of truth — it never inlines the SPEC, so it cannot drift.
- Validation now emits `FM_MISSING_CREATED` and `FM_MISSING_UPDATED` when a
  content file omits the universal timestamps.

#### Changed

- `dbmd validate` falls back to a per-file content sweep when the default
  working-set has no logged changed objects, avoiding vacuous clean reports on
  fresh stores or externally edited stores with no `log.md` entry.
- `dbmd index show --json` and `dbmd index rebuild --dry-run --json` now emit
  machine-parseable envelopes instead of ignoring global JSON mode.

#### Fixed

- Mutating CLI paths (`write`, `link`, `rename`, `fm`, and scoped `index`
  operations) now reject absolute/traversal paths outside the opened store.
- Core write paths use exclusive, same-directory temp files before atomic
  rename, closing predictable-temp clobber races.
- Normal CLI commands fail closed on unreadable or malformed `DB.md` instead of
  silently using a default config.
- `fm init` can initialize raw markdown files that were externally dropped into
  the store.
- Schema `default <value>` modifiers are applied by `write` and `fm init`
  without overwriting explicit fields.
- Schema-declared `link to` fields now still warn on `.md` wiki-link targets.
- `index rebuild --layer <layer>` repairs child type-folder `index.jsonl`
  artifacts before rendering the layer rollup.
- Summary templates and `index.jsonl` projection normalize unquoted wiki-link
  YAML shapes consistently.
- `graph orphans` counts only links to existing store files as graph edges.
- Skill install/uninstall refuses to overwrite or remove unmanaged agent
  instruction files.

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
  the schema-driven `DUP_UNIQUE_KEY`. The live SPEC table now has 40 codes.
- The hard-coded per-type `summary` composers, and the `dbmd stats`
  recognized-vs-custom type split (every type is now the store's own).

#### Changed

- Folder placement is no longer enforced by type (`LAYER_TYPE_MISMATCH` is
  gone); the three-layer layout stays a convention.

Toolkit impact: this is a breaking 0.x change; the crate is bumped to **0.3.0**
for this release.

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
