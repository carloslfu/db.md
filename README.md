# db.md

**The open database, in plain files.**

`db.md` is a database where records are markdown files with YAML
frontmatter, relationships are wiki-links, and the index is whatever
you build on top. The database is the directory; the schema is the
frontmatter.

One file at the root, **`DB.md`**, says how the store behaves — its
identity, the agent's instructions, the policies, the schemas. A
capable agent reads `DB.md` and curates the files directly. That file
is the center of it; the rest is plain data the agent operates.

One directory, three layers, one config file: raw evidence in
`sources/`, atomic typed data in `records/`, curator-synthesized
narrative in `wiki/` — all governed by `DB.md`. Bring any agent
runtime (Claude Code, Codex, or your own); it plays the curator role,
guided by the [SPEC](SPEC.md) and the store's `DB.md`.

```
db/
├── DB.md          # identity + agent instructions + policies + schemas
├── index.md       # curator-maintained catalog (hierarchical)
├── log.md         # chronological log (append-only; rotates monthly)
├── sources/       # raw evidence (immutable; date-sharded at scale)
├── records/       # atomic typed data (contacts, expenses, meetings, ...)
└── wiki/          # curator-synthesized narrative with wiki-links
```

## Quick start

```bash
# install the toolkit (one prebuilt binary, ~5MB, no toolchain)
curl -fsSL https://raw.githubusercontent.com/carloslfu/db.md/main/scripts/install.sh | sh

# create a store — you write DB.md (the agent authors it; there is no `dbmd init`)
mkdir -p db/{sources,records,wiki}
printf -- '---\ntype: db-md\nscope: personal\nowner: me\n---\n' > db/DB.md

dbmd validate db                          # frontmatter + link + schema check
dbmd search "renewal" --in records        # search across the store
dbmd links records/contacts/sarah-chen    # who links to this record?
dbmd index rebuild db                     # regenerate the index hierarchy
```

**Point your coding agent at the store.** The installer is text: db.md is installed and integrated by reading markdown and acting on it, and a capable agent is the installer. Hand the agent the repo-root [`llms.txt`](llms.txt) — the agent-readable entry point for what db.md is and how to install, integrate, and operate it — and it can do the whole bootstrap by reading it and running the commands below. There is no per-harness machinery to depend on: the mechanism is generic text plus a smart model.

Load the contract once per session — `dbmd spec` is the single source of truth:

```bash
claude --append-system "$(dbmd spec)"     # any runtime with a system-prompt hook
```

To persist that across sessions, place a skill where your harness reads skills, in the open [Agent Skills](https://agentskills.io) format — the canonical file ships in the repo at [`skills/db-md/SKILL.md`](skills/db-md/SKILL.md) (a `name`/`description` frontmatter pointer that runs `dbmd spec`): Claude Code reads `~/.claude/skills/db-md/`, Codex reads `~/.codex/skills/db-md/`; any other harness uses its own skills dir or loads `dbmd spec` into its system prompt. Placing it is generic file work — copy the file, use your harness's own skill installer (Codex's `skill-installer`, a Claude Code plugin), or just tell your agent to set itself up. db.md ships **no per-harness install command**; the installer is text and the model does the rest. The skill never copies the SPEC (it points at `dbmd spec`), so it cannot drift.

The format is at **v0.2**: schema enforcement is solely the store's own `DB.md ## Schemas`, and the example types (`contact`, `expense`, …) are illustrative, not normative (v0.1 was the first tagged release, [`v0.1`](https://github.com/carloslfu/db.md/releases/tag/v0.1)). From v0.2 on, changes are additive. See the [CHANGELOG](CHANGELOG.md).

## The curator is your agent

db.md ships **no LLM runtime and no API keys**. "Curator" is a role
any agent runtime plays: Claude Code, Codex, or your own. The agent
loads the contract with `dbmd spec` — the single source of truth, read
once per session — then follows the curator contract and operates the
store through `dbmd` subcommands. Persisting that across sessions is a
skill in the open Agent Skills format (`skills/db-md/SKILL.md` in the
repo) — placed by copying the file, the harness's own skill installer,
or the agent itself; db.md ships no per-harness install command. The
toolkit is deterministic file/data plumbing; the
agent does the reasoning. See [SPEC.md](SPEC.md) § The curator
contract and § The agent session.

## Why files

The database has been a service for decades: a daemon, a wire
protocol, a schema migration tool, an admin UI. That made sense when
storage was expensive and indexes had to live in RAM. It doesn't
anymore.

db.md inverts the shape:

- **The database is the directory.** No daemon, no port.
- **The schema is the frontmatter.** Type-tagged, additive, optional.
- **The index is derived.** db.md ships its own (a hierarchical
  `index.md` catalog plus embedded ripgrep) and reaches millions of
  files with no vector database. Build a SQLite or tantivy index on
  top if another tool needs one; the files stay the source of truth.

Three properties files have that tables don't:

- **Human-editable.** A record is a file. Open it, edit it, commit it.
- **Version-controllable.** Git is the audit log.
- **LLM-native.** The format an LLM reads best is the format a human
  reads best.

Most databases are not Google-scale; they are records with a form
or a dashboard on top: a CRM, an ops tracker, a contract register,
the internal tools a company rebuilds, the SaaS apps that are a
database with a UI bolted on. db.md replaces the database for that
whole class, and the app over it. The agent reads the records and
builds the view on demand. The genuinely hard remainder (high write
concurrency, ACID, sub-millisecond reads, billions-row aggregates)
is where the roadmap takes db.md next (the packed engine, projected
through a VFS); a real engine still earns its place there today, and
until then the two compose cleanly. The direction is one way:
eventually, all of them, and never by adding vectors.

Extends Karpathy's April 2026 LLM Wiki pattern from topic scope to
**company scope**: customers, vendors, contracts, decisions,
meetings, expenses, processes, playbooks, all maintained by the
curator agent the team directs.

The native toolkit holds company scale: a company's full email
history (hundreds of thousands to millions of records) on plain
files with embedded ripgrep, no vector database. See
[SPEC.md § Scale](SPEC.md) for the budgets and the sizing model.

## Tooling

db.md is plain files; any tool that reads files works. The reference
toolkit is **one Rust binary**, `dbmd`:

- **One binary, many subcommands** (git / cargo / kubectl shape) for
  read / write / validate / extract / graph / index / log ops.
- **Embedded ripgrep** (via the `grep` crate) for fast search, with
  no separate `rg` to install.
- **Built-in extraction** (`dbmd extract`) for PDF / docx / xlsx /
  epub / html via MIT Rust crates. No GPL `pdfgrep`, no AGPL `rga`.
- **Zero LLM dependencies.** No provider SDKs, no API keys. The agent
  runtime is BYO.
- **`dbmd-core` library.** All logic lives in the library crate; the
  binary is thin wrappers. `cargo add dbmd-core` to build
  db.md-aware Rust tools.

### Install

One self-contained binary (~5MB, no runtime deps). Every path below
installs the **same prebuilt, checksummed binary** built in CI — pick one:

```sh
# Recommended — prebuilt binary, no toolchain (macOS + Linux)
curl -fsSL https://raw.githubusercontent.com/carloslfu/db.md/main/scripts/install.sh | sh

# Homebrew
brew install carloslfu/tap/dbmd

# Already have the Rust toolchain? Build from crates.io instead.
cargo install dbmd-cli
```

The install script resolves the latest release and downloads the binary
**directly from this repo's [GitHub Releases](https://github.com/carloslfu/db.md/releases)** —
no account, no platform, nothing between you and the binary. Prefer no
script at all? Download a tarball and verify it yourself — every release
ships `SHA256SUMS` plus a build-provenance attestation (see [Security](#security)):

```sh
gh release download -R carloslfu/db.md -p 'dbmd-*-darwin-aarch64.tar.gz' -p 'SHA256SUMS'
shasum -a 256 -c SHA256SUMS --ignore-missing && tar -xzf dbmd-*.tar.gz
```

See [TOOLS.md](TOOLS.md) for the full subcommand surface and the agent
bootstrap pattern.

## Repository layout

```
db.md/
├── SPEC.md             # format spec + curator contract + validation codes (v0.2)
├── README.md
├── TOOLS.md            # toolkit reference (subcommand surface, install, bootstrap)
├── skills/db-md/       # the canonical Agent Skill (SKILL.md) — the distributable agents/harnesses install
├── Cargo.toml          # Rust workspace
├── crates/
│   ├── dbmd-core/      # library: parser, store, graph, validate, stats, query, index, log
│   └── dbmd-cli/       # the `dbmd` binary (thin wrappers)
├── db/                 # the project's own db.md store (the dogfood, see below)
├── examples/           # role-flavored example stores (three-layer: sources/ records/ wiki/)
│   ├── research-wiki/
│   ├── ops-store/
│   ├── personal-second-brain/
│   ├── agency-knowledge-base/
│   └── customer-database/
└── tests/corpora/      # test stores (canonical, edges, formats, scale, agent)
```

The flagship worked example is `db/`, db.md's own knowledge as a
db.md store: the research that grounds the design under `sources/`,
every material build decision under `records/decisions/`, and the
narrative synthesis under `wiki/`. It is how db.md itself was built,
and the answer to "how do you run db.md at company scale?" is to read
the store of how db.md itself was built. It is co-located with the
code and operated by `dbmd` as the toolkit grows. An agentic computer
typically ships with its own db.md store at `~/db/`.

## License

[Apache-2.0](LICENSE). Patent grant, trademark clause, explicit
modification disclosure. CLA on every PR via CLA Assistant. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## How db.md relates to other approaches

db.md is the open database in plain files: your data lives in files you can read, edit, and own, and a capable agent operates them directly. Most software that looks like an app is a database with a UI bolted on. db.md replaces both: the records become files, the agent is the query engine, and the view is built on demand.

The question under every alternative is the same, asked on two axes: what sits between you and your data, and what each rides on as the models improve. With db.md nothing sits between, and what it rides on is the model itself. Every other approach puts a layer in the way, a server, a vendor, an engine, or a derived cache, and that layer is machinery you maintain, not intelligence you rent from the model. Machinery only gets better when you do the work; the model gets better on its own, and db.md compounds with every release.

| Approach | What sits between you and your data | What it rides on |
|---|---|---|
| **db.md** | nothing: the data is the files; you read and edit them directly, and the agent works the same files | the model curve, directly: every new model works the same files better, with nothing to migrate or rebuild |
| SQL / relational databases | a schema you design up front and migrate when reality changes, a query language, and an app to use it | your schema and the app layer; a better model can write the SQL, but the store sits outside the model curve and never gets smarter |
| Airtable / Notion (the database with a UI) | a vendor's service you rent, your data on their servers; export is lossy and drops the relations and formulas | the vendor's roadmap; you get the AI they bolt on, when they ship it, and only inside their walls |
| Graphify | a derived knowledge-graph beside your files, queried through its API, stale until the next rebuild | a better model too, but spent rebuilding a derived graph that drifts from your files, not on the files themselves |
| QMD | a SQLite search index and bundled small models, kept beside files you still own | its bundled small models, capped at their size; recall climbs when QMD ships new ones, not when the frontier moves |
| Vector RAG | a vector store of embeddings you cannot read or edit, reached only through a retrieval service | a separate, smaller embedding model and reranker; recall is capped by that retrieval stack, re-paid every query, and a better reasoning model does not lift it |
| Karpathy's LLM Wiki | nothing: plain markdown the model reads (db.md's lineage) | the model curve, directly on the files (db.md's lineage) |

Vector RAG is the approach db.md bets most directly against: db.md computes, stores, and searches no vector, ever. Where RAG engineers retrieval over embeddings of your data, db.md keeps the data as files and lets the frontier model read them, with semantic recall coming from the agent expanding the query over plain lexical search, not a separate embedding model.

The memory products built on that approach, by name, and what each stands for:

| Memory tool | What it stands for | What sits between you and your data |
|---|---|---|
| **Mem0** | managed memory: an LLM extracts facts, embeds them, and retrieves by similarity (keyword and entity matching added in 2026) | a vector-and-graph service you call; your memories kept as embeddings you cannot read, recall capped by a separate, smaller retrieval model and re-paid on every query |
| **Letta / MemGPT** | self-editing agent memory; it asked whether a filesystem is all you need and answered files plus embeddings | an embedding index built automatically over your files; db.md is that same filesystem thesis with the vectors removed |
| **Zep / Graphiti** | temporal memory built as a derived knowledge graph | a hosted graph and its API, a derived structure built from your data and kept in step with it |
| **Cognee** | an extract-cognify-load pipeline into a graph-and-vector store | one more derived store to build and keep in sync with your files |
| **db.md** | the data is the files; the agent is the query engine; no vector, ever | nothing: it rides the frontier model directly on the files you own |

The mechanism is the whole argument. An embedding has no notion of when a fact was true or whether it was later superseded; a dated file does. db.md answers time and knowledge-update questions by filtering frontmatter, not by hoping a vector lands nearby. The clearest proof that this is the right cut is that Mem0's own 2026 rewrite went append-only and bolted keyword and entity matching onto its vectors, moving onto the ground db.md already stands on. db.md is that endpoint, without the vector tax.

Every tool here ships a memory system you adopt: a service, an engine, an index you rent and maintain. db.md ships a convention you own: plain files the frontier model reads and writes directly. The memory layer was always a database with the data hidden; db.md is the same job with the data left in the open.

For the genuinely hard remainder (high write concurrency, ACID, sub-millisecond reads, billions-row aggregates), a real database still backs db.md. That is the roadmap, not the claim for today.

db.md composes with the rest of the agent stack: [computer.md](https://github.com/carloslfu/computer.md/blob/main/spec/SPEC.md) for the agentic computer that runs it, AGENTS.md for instructions, MCP for tools. Different layers, not alternatives.

Your data belongs in files you own, not behind a server, a vendor, or a cache. The tool stays small and model-free; the intelligence is the agent's, rented not built. Every other approach asks you to maintain more machinery; db.md asks you to trust the model, and the model is the thing that compounds. db.md and its LLM Wiki lineage are the only approaches that ride that curve directly on the files; db.md is the agent-native build-out of that bet, at company scale, replacing the database and the app over it for the whole class that was only ever records with a view on top. Bet on the model, not the machinery.

## Independently usable

db.md is a self-contained standard. A plain markdown vault becomes a
db.md store: Obsidian users, researchers running a topic wiki, an
agentic computer keeping its company brain, any agent runtime with a
folder of markdown. No platform, no account, no hosted service
required. [The spec](SPEC.md) is the contract; the runtime is replaceable.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Sign the Apache ICLA via
the CLA Assistant bot on your first PR.

## Security

Report vulnerabilities privately via GitHub's "Report a vulnerability"
(Security tab); do not open a public issue for security problems. See
[SECURITY.md](SECURITY.md).

**Releases are auditable and trusted.** Every release is built in CI from
a tagged commit, not from a developer's machine. Prebuilt tarballs carry
SHA256 checksums and build-provenance attestations, so anyone can confirm
a download came from this repo's CI and was not tampered with:

```bash
gh attestation verify dbmd-<version>-<target>.tar.gz --repo carloslfu/db.md
```

The `dbmd-cli` and `dbmd-core` crates publish to crates.io through Trusted
Publishing (OIDC), so no long-lived registry token exists to leak. The
toolkit ships zero AI/LLM dependencies and its tree is MIT/Apache, so you
can audit it or build from source. See [RELEASING.md](RELEASING.md).

**Dependencies are continuously audited.** Every pull request runs
`cargo deny check advisories`, so the build fails on any open RustSec
advisory (vulnerability, unsound, or unmaintained). The tree is also
watched by GitHub Dependabot and Socket supply-chain scanning (malware,
typosquats, suspicious install scripts), and every crate plus its license
is recorded in [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES).
