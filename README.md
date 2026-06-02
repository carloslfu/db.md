# db.md

**The open database, in plain files.**

`db.md` is a database where records are markdown files with YAML
frontmatter, relationships are wiki-links, and the index is whatever
you choose to build on top. The database is the directory. The
schema is the frontmatter. Simple and open.

One directory, three layers, one config file. Raw evidence lives in
`sources/`, atomic typed data in `records/`, curator-synthesized
narrative in `wiki/`. Identity, agent instructions, policies, and
schemas all live in a single `DB.md` file at the root. An agent
runtime you bring (Claude Code, Codex, or your own) plays the curator
role, guided by the [SPEC](SPEC.md) and the store's `DB.md`.

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
# install the toolkit (one binary, ~5MB, MIT/Apache)
cargo install dbmd-cli

# create a store
mkdir -p db/{sources,records,wiki} && dbmd fm init db/DB.md

dbmd validate db                          # frontmatter + link + schema check
dbmd search "renewal" --in records        # search across the store
dbmd links records/contacts/sarah-chen    # who links to this record?
dbmd index rebuild db                     # regenerate the index hierarchy
```

Point any agent runtime at the store. The [SPEC](SPEC.md) becomes its contract:

```bash
claude --append-system "$(dbmd spec)"     # Claude Code, Codex, or any runtime
```

The format is at **v0.1**, tagged [`v0.1`](https://github.com/carloslfu/db.md/releases/tag/v0.1); changes are additive only.

## The curator is your agent

db.md ships **no LLM runtime and no API keys**. "Curator" is a role
any agent runtime plays: Claude Code, Codex, or your own. The agent
reads the [SPEC](SPEC.md) (`dbmd spec`), follows the curator contract, and
operates the store through `dbmd` subcommands. The toolkit is
deterministic file/data plumbing; the agent does the reasoning. See
[SPEC.md](SPEC.md) § The curator contract and § The agent session.

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

Install: `cargo install dbmd-cli` (from crates.io), or
`brew install carloslfu/tap/dbmd`, or download a prebuilt binary from the
[GitHub releases](https://github.com/carloslfu/db.md/releases) (checksummed
and provenance-attested; see [Security](#security)).
See [TOOLS.md](TOOLS.md) for the full subcommand surface and the agent
bootstrap pattern.

## Repository layout

```
db.md/
├── SPEC.md             # format spec + curator contract + validation codes (v0.1)
├── README.md
├── TOOLS.md            # toolkit reference (subcommand surface, install, bootstrap)
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

The question under every alternative is the same: what sits between you and your data. With db.md, nothing does. Every other approach puts a layer in the way, a server, a vendor, an engine, or a derived cache, and that layer is machinery you maintain, not intelligence you rent from the model.

| Approach | What sits between you and your data |
|---|---|
| **db.md** | nothing: the data is the files; you read and edit them directly, and the agent works the same files |
| SQL / relational databases | a schema, a query language, and an app to use it |
| Airtable / Notion (the database with a UI) | a vendor's service you rent; export is lossy |
| Graphify | a derived knowledge-graph, rebuilt from your files and queried through its API |
| QMD | a search index and its own bundled models, kept beside your files |
| Vector RAG | a separate vector store and a retrieval service |
| Karpathy's LLM Wiki | nothing: plain markdown the model reads (db.md's lineage) |

For the genuinely hard remainder (high write concurrency, ACID, sub-millisecond reads, billions-row aggregates), a real database still backs db.md. That is the roadmap, not the claim for today.

db.md composes with the rest of the agent stack: [computer.md](https://github.com/carloslfu/computer.md/blob/main/spec/SPEC.md) for the agentic computer that runs it, AGENTS.md for instructions, MCP for tools. Different layers, not alternatives.

Your data belongs in files you own, not behind a server, a vendor, or a cache. The tool stays small and model-free; the intelligence is the agent's, rented not built. Bet on the model, not the machinery.

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
