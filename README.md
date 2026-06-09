# db.md

**The database was a workaround for computers that couldn't read. They can now.**

**Your database is a folder of plain text files.** No server, no tables, no
query language. Every record is one markdown file you can open, read, and edit
by hand. The links between records are written into the text itself.

The folder is the database.

For fifty years that would have been a toy. You needed a server, because the
data would not fit in memory. You needed a schema up front, and a query
language, because software could not read plain writing or find anything in
the pile.

**Every one of those reasons just expired.** A capable agent reads the files,
writes them, links them, and finds anything in them by plain meaning. The
agent is the engine.

And the engine is the bet. **A db.md store gets sharper every time the model
behind it improves.** A SQL database queries exactly as well in ten years as
the day you installed it. Your files ride the model curve, with nothing to
migrate and nothing to rebuild.

So the database stops being software you run and becomes **data you own.**
Text on disk that a person reads easily, a model reads better than anything
else, and that outlasts every tool that ever touches it.

It is not small. **Millions of records live on plain files**, with no vector
database anywhere.

And it replaces a whole class of software: **the products that were only ever
a database with a screen on top.**

Here is a record. It is a file:

```markdown
---
type: contact
name: Elena Rodriguez
email: elena.rodriguez@northstar.io
company: [[records/companies/northstar]]
role: Director of Operations
created: 2025-09-14
updated: 2026-05-22
---

# Elena Rodriguez

Director of Operations at [[records/companies/northstar]]. Champion on the
renewal that expands to 175 seats. See the thread in
[[sources/emails/2026-05-22-elena-rodriguez-renewal]]. Prefers async over
calls unless something is stuck.
```

The frontmatter at the top is the schema. The `[[double bracket]]` entries are
the relationships, the same links a wiki uses. The text below is for you, and
for the agent. A person can read it. Git versions it. A model reads it better
than any row in any table. That is the whole format.

## What it replaces

Most software a company pays for is a database with a screen bolted on: a CRM,
an ops tracker, a contract register, the internal tool every company rebuilds,
the SaaS product that is just a table behind a login. **db.md replaces the
database and the screen at once.** The records are the files, the agent answers
the questions, and the view gets built the moment you ask for one.

It is the pattern from Karpathy's April 2026 LLM Wiki, which scoped a single
research topic, taken to company scope: customers, vendors, contracts,
decisions, meetings, expenses, processes, playbooks, all curated by the agent
your team directs.

## How it works

One directory. Three folders for your data, and one file that runs the place.

```
db/
├── DB.md          # identity, agent instructions, policies, schemas
├── index.md       # a catalog the agent keeps current
├── sources/       # raw evidence, kept as it arrived: emails, PDFs, exports
├── records/       # atomic typed data: contacts, companies, expenses, meetings
└── wiki/          # the agent's synthesis, linked back to the rest
```

`DB.md` is the file that matters most. It holds the store's identity, the
instructions for the agent, the policies it has to follow, and the schemas your
records conform to. The agent reads `DB.md` first and curates everything else
against it. You never write a config format or stand up a service. The agent
writes `DB.md` for you and keeps it honest.

Bring any agent runtime. Claude Code, Codex, or your own. It plays the curator:
reading the files, writing new ones, keeping the links and the catalog in order,
following the contract in `DB.md` and the [spec](SPEC.md). The format is at
v0.2, and from here changes are additive. See the [CHANGELOG](CHANGELOG.md).

## How it compares

Every other way to store data puts something between you and it. Ask two
questions of each one: what sits in the middle, and what does it ride on as
models improve. **db.md puts nothing in the middle, and it rides on the model
itself.** Everything else rides on machinery you maintain, and machinery only
improves when you do the work.

| Approach | What sits between you and your data | What it rides on |
|---|---|---|
| **db.md** | nothing. The data is the files. You read and edit them directly, and so does the agent | the model curve, directly. Every new model works the same files better, with nothing to migrate or rebuild |
| SQL databases | a schema you design up front and migrate when reality shifts, a query language, and an app to drive it | your schema and the app layer. A better model can write the SQL, but the store itself never gets smarter |
| Airtable, Notion | a vendor's service you rent, your data on their servers, an export that drops the relations and the formulas | the vendor's roadmap. You get the AI they bolt on, when they ship it, inside their walls |
| Vector RAG | a store of embeddings you cannot read or edit, reached only through a retrieval service | a separate, smaller embedding model. Recall is capped by it, re-paid on every query, and a smarter reasoning model does not lift it |
| Knowledge-graph memory | a derived graph beside your files, queried through an API, stale until the next rebuild | a better model too, but spent rebuilding a graph that drifts from your files, not the files themselves |
| Karpathy's LLM Wiki | nothing. Plain markdown the model reads. This is db.md's lineage | the model curve, directly on the files. Also db.md's lineage |

The fight db.md picks most directly is with vector RAG. **db.md computes,
stores, and searches no vector, ever.** RAG engineers a retrieval pipeline over
embeddings of your data; db.md keeps the data as files and lets the model read
them, with semantic recall coming from the agent widening its own search in
plain language.

An embedding cannot tell you when a fact was true or whether something later
replaced it. A dated file can. The clearest sign this is the right cut: Mem0's
2026 rewrite went append-only and bolted keyword and entity matching onto its
vectors, moving onto ground db.md already stood on.

## Why files

The database has been a service for decades: a daemon, a wire protocol, a
migration tool, an admin panel. That made sense when storage was expensive and
the index had to live in memory. It does not anymore.

db.md turns the shape inside out:

- **The database is the directory.** There is no daemon and no port. You can
  `cd` into it and `ls` your data.
- **The schema is the frontmatter.** It is typed, optional, and additive. You
  change it by editing a file, not by running a migration.
- **The index is derived.** A plain catalog plus embedded ripgrep reaches
  millions of files with no vector database. Want SQLite or a search index on
  top? Build one. The files stay the source of truth.

## How far it scales

Put a number on "millions." A person who indexes every email they send and
receive adds about **44,000 files a year**, around 440,000 in a decade; a
whole career fits in **1 to 1.5 million plain files**. **A ten-person company
crosses a million files in two to three years.** That is the scale this format
is built to hold.

And the agent never pays for it, because **the agent does not navigate files,
it reads indexes.** Every type folder keeps a human `index.md` (the 500 most
recent entries) and a complete machine `index.jsonl`, both updated in place on
every write. A query reads one small sidecar file and goes straight to the
right record. The interactive loop is **O(changed), never O(store)**: what an
operation costs tracks what changed, not how big the store has grown.

- **High-volume folders shard by date.** An email lands in
  `sources/emails/2026/05/`, an expense in `records/expenses/2026/05/`. No
  directory grows unbounded, and only the current shard is ever hot. Entity
  records and the wiki stay flat, because those sets are bounded by reality:
  you have only so many customers, however much mail they send.
- **The budgets hold at a million files.** A write costs **under 100ms in a
  store of 10,000 files, and under 100ms in a store of a million.** A
  structured query stays **under two seconds at a million files**, and the
  worst case, a cold full-text sweep with nothing indexed, is embedded ripgrep
  clearing a million files in seconds. The full table is in the
  [spec](SPEC.md#scale).
- **Whole-store passes run off the loop.** A full `dbmd validate --all` or
  index rebuild is a linear repair and audit job you schedule. The agent never
  waits on one.

The first ceiling you hit is not the format's. It is git's: vanilla git wants
tuning past **100,000 tracked files** and slows near a million, because its
index rewrites an entry for every tracked file, O(everything), not O(changed).
The store has no such limit, and git is optional tooling over db.md, not part
of it. At that point, version the curated layers (`records/`, `wiki/`) and let
high-volume sources ride filesystem snapshots. The files keep working exactly
the same.

What still wants a real engine: heavy write concurrency, ACID transactions,
sub-millisecond reads, aggregates over billions of rows. That territory is
the packed flavor on the [roadmap](SPEC.md#roadmap), with the same contract
and the files still the source of truth. Until then the two compose cleanly.

## Quick start

Install `dbmd`. One Rust binary, about 5MB, no toolchain:

```bash
curl -fsSL https://raw.githubusercontent.com/carloslfu/db.md/main/scripts/install.sh | sh
# or: brew install carloslfu/tap/dbmd
# or: cargo install dbmd-cli
```

Load the contract once per session. `dbmd spec` prints the whole standard:

```bash
dbmd spec
```

Point your agent at a folder and let it work. It writes `DB.md`, sorts your
files into the three layers, and curates from there. Then, from inside the
store:

```bash
dbmd search "renewal" --in records                   # search content and frontmatter
dbmd query --type contact --where status=active      # filter by frontmatter
dbmd links records/contacts/elena-rodriguez          # who links to this record
dbmd graph neighborhood records/companies/northstar  # the local web around a record
dbmd validate                                        # frontmatter, links, schemas, all checked
```

Every command speaks `--json`, so anything you build on top reads it cleanly.

## The agent is the engine

db.md ships no model and no API keys. The curator is whatever agent you already
use: Claude Code, Codex, or your own. The whole flow is four moves. It discovers
db.md, runs `dbmd spec` for the contract, reads the store's `DB.md`, then
operates with `dbmd`.

The binary is deterministic plumbing. The agent does the thinking. You are
never locked to a model, because the model is the one part you bring and the
one part that keeps improving.

The installer is text. Hand an agent the repo's [llms.txt](llms.txt) and it
sets itself up by reading it and running the commands.

To make your agent reach for db.md on every session, place a skill where it
reads skills, in the open [Agent Skills](https://agentskills.io) format. The
canonical file ships at [`skills/db-md/SKILL.md`](skills/db-md/SKILL.md), and
its body just points at `dbmd spec`, so it cannot drift. There is no install
command for this, on purpose. Copy the file, use your agent's own skill
installer, or tell the agent to set itself up.

## The toolkit

db.md is plain files, so any tool that reads files works. The reference toolkit
is one Rust binary, `dbmd`, in the git / cargo / kubectl shape: one binary, many
subcommands for read, write, validate, extract, graph, index, and log work.

- **Embedded ripgrep.** Fast search with no separate tool to install.
- **Built-in extraction.** `dbmd extract` pulls text out of PDF, docx, xlsx,
  epub, and html, all through Rust crates under MIT and Apache licenses. No GPL
  `pdfgrep`, no AGPL `rga`.
- **Zero AI dependencies.** No provider SDKs, no API keys, no model calls in
  the binary. The agent runtime is yours.
- **A library underneath.** All the logic lives in `dbmd-core`. Run `cargo
  add dbmd-core` to build your own db.md-aware tool.

See [TOOLS.md](TOOLS.md) for the full command surface and the agent bootstrap.

## The memory tools, by name

A wave of products sells "memory" for agents. Each ships a system you adopt and
maintain. db.md ships a convention you own.

| Tool | What it is | What sits between you and your data |
|---|---|---|
| **Mem0** | managed memory: an LLM extracts facts, embeds them, retrieves by similarity | a vector-and-graph service you call. Your memories live as embeddings you cannot read, recall capped by a smaller retrieval model and re-paid on every query |
| **Letta / MemGPT** | self-editing agent memory; it asked whether a filesystem is all you need | an embedding index built over your files. db.md is that filesystem thesis with the vectors removed |
| **Zep / Graphiti** | temporal memory built as a derived knowledge graph | a hosted graph and its API, a second structure kept in step with your data |
| **Cognee** | an extract-and-load pipeline into a graph-and-vector store | one more derived store to build and keep in sync |
| **db.md** | the data is the files; the agent is the query engine; no vector, ever | nothing. It rides the model directly on the files you own |

The memory layer was always a database with the data hidden. db.md is the same
job with the data left in the open. It also composes with the rest of the stack:
[computer.md](https://github.com/carloslfu/computer.md) for the agentic computer
that runs it, AGENTS.md for instructions, MCP for tools. Different layers, not
rivals.

## What's in this repo

```
db.md/
├── SPEC.md          # the format, the curator contract, the validation codes (v0.2)
├── TOOLS.md         # the toolkit: every subcommand, install, agent bootstrap
├── crates/
│   ├── dbmd-core/   # the library: parser, store, graph, validate, query, index, log
│   └── dbmd-cli/    # the dbmd binary (thin wrappers over the library)
├── examples/        # five real stores: research wiki, ops, second brain, agency, CRM
├── skills/db-md/    # the canonical Agent Skill you place in your own agent
└── db/              # db.md's own knowledge, kept as a db.md store
```

The store under `db/` is the proof. db.md's own research, every build decision,
and the synthesis over them live there as a db.md store. The answer to "does
this hold at company scale?" is to read the store of how db.md itself was built.

## Use it on its own

db.md is an open standard, and it stands on its own. A plain markdown vault
becomes a db.md store, with no platform and no account required: Obsidian
users, a researcher running a topic wiki, an agentic computer keeping a company
brain, any agent runtime with a folder of markdown. The [spec](SPEC.md) is the
contract. The runtime is replaceable. **The files outlast both.**

## License

[Apache-2.0](LICENSE), with a patent grant, a trademark clause, and an explicit
modification-disclosure term. Every pull request signs the Apache ICLA through
the CLA Assistant bot. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Sign the Apache ICLA through the CLA
Assistant bot on your first pull request.

## Security

Report vulnerabilities privately through GitHub's "Report a vulnerability"
button on the Security tab. Do not open a public issue for a security problem.
See [SECURITY.md](SECURITY.md).

Releases are built in CI from a tagged commit, never from a developer's laptop.
Every tarball carries a SHA256 checksum and a build-provenance attestation, so
anyone can confirm a download came from this repo's CI and was not tampered
with:

```bash
gh attestation verify dbmd-<version>-<target>.tar.gz --repo carloslfu/db.md
```

The `dbmd-cli` and `dbmd-core` crates publish to crates.io through Trusted
Publishing (OIDC), so there is no long-lived registry token to leak. The binary
ships zero AI dependencies and its tree is MIT and Apache licensed, so you can
audit it or build it from source. Every pull request runs `cargo deny check
advisories` and fails on any open RustSec advisory, and the tree is watched by
Dependabot and Socket supply-chain scanning. See [RELEASING.md](RELEASING.md).
