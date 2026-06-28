# db.md

**A huge class of future software will not be built as database + backend +
frontend. It will be built as readable context + agent harness + generated
surface. db.md is the persistence layer for that world.**

**Your database is a folder of plain text files.** No daemon, no fixed tables,
no query language standing between the agent and the data. Every record is one
markdown file you can open, read, and edit by hand. The links between records
are written into the text itself.

The folder is the database.

Before agents, text was documentation. After agents, text becomes operational
state.

For fifty years a folder-as-database would have sounded like a toy. Database
engines earned their place by doing hard things files alone did not: durable
writes, indexes, transactions, concurrent access, permissions, fast queries.
But for the broad middle of software, they also carried a workaround: ordinary
programs could not read and maintain meaning-rich state directly, so messy
reality had to be squeezed into tables before software could use it.

**That workaround is expiring.** A capable agent reads the files, writes them,
links them, and finds the connections between them by meaning. The agent is the
engine.

And the engine is the bet. **A db.md store gets sharper every time the model
behind it improves.** A better model can read the same files with more context,
repair them with more judgment, and reshape the schema without a migration
ceremony. SQL can still be queried by better agents, but the store's meaning
lives in schema and app code. Your files ride the model curve.

So the database stops being a service you run and becomes **data you own.** Text
on disk that a person reads easily, a model can read directly, and that outlasts
every tool that ever touches it.

It is not tiny. **db.md is built for stores that grow into millions of plain
files**, with no vector database anywhere.

## The stack collapse

For decades, the default app shape was:

```
Database -> Backend -> Frontend
Postgres + service layer + React app
```

The database held the state. The backend encoded the rules. The frontend
exposed fixed views and actions. That shape made sense when programs could not
understand the data they operated on.

Agents change the default.

For a large class of semantic, evolving, workflow-heavy software, the new shape
is:

```
Markdown files -> Agent harness -> Generated surface
db.md + agent harness -> voice, chat, canvas, forms, approvals, dashboards
```

The files hold the records, context, relationships, policies, and history. The
agent harness reads, writes, validates, repairs, migrates, plans, and acts. The
surface appears when needed: chat, voice, canvas, forms, approval cards,
dashboards, or whatever the task requires.

The old app becomes an agent operating over readable state.

The claim is not "no database." It is **agent-operated files-as-database**:
records are markdown files, fields are YAML frontmatter, relationships are
wiki-links, schemas and policies live in `DB.md`, indexes are `index.md` /
`index.jsonl`, the deterministic tool is `dbmd`, and the engine is the agent
driving it.

That is why db.md is not merely a Markdown database. It is the default
persistence layer for agent-native software: the class of software whose main
substance is records, context, relationships, decisions, workflows, and a
surface.

Personal software is the easiest place to see the collapse first. The same
shape fits internal tools, company brains, ops trackers, lightweight CRMs,
research systems, project systems, support workflows, agency workflows,
contract registers, decision logs, admin tools, family tools, field-specific
tools, and agent-native products whose shape changes every week.

Many of those were previously too small, too specific, too fluid, or too alive
to justify becoming full SaaS products. Agents change that. The long tail of
software becomes possible because the app no longer has to be a rigid
database-backed product with a hand-built backend and frontend. It can be
readable state plus an agent that knows how to operate on it.

Hard truth still exists. Payments, ledgers, high-concurrency shared state,
strict permission systems, sub-millisecond reads, billion-row analytics, and
regulated financial correctness still want hard engines. Postgres is for
authoritative machinery. db.md is for living context.

Files for meaning. Tools for authority. Agents for execution.

Here is a record. It is a file:

```markdown
---
type: trip
name: Kyoto spring trip
dates: 2026-04-11..2026-04-18
status: planning
travelers:
  - [[records/people/maya]]
  - [[records/people/jules]]
home_base: [[records/places/kyoto-station]]
created: 2026-01-12
updated: 2026-06-03
---

# Kyoto spring trip

Seven days in Kyoto with Maya and Jules. The current plan keeps the first
two nights near [[records/places/kyoto-station]], then moves to the ryokan
from [[sources/emails/2026-06-03-ryokan-confirmation]]. Jules wants temples
in the morning, Maya wants one open afternoon for wandering, and nobody wants
another spreadsheet.
```

The small YAML block at the top is frontmatter. In db.md, that is the structured
surface of the record: simple labels the agent can sort, filter, and repair,
with store-specific schemas declared in `DB.md`. The `[[double bracket]]`
entries are the relationships, the same links a wiki uses. The text below is
for you, and for the agent. A person can read it. Git versions it. A model reads
it with the context a row usually hides. That is the whole format.

## What it is for

db.md fits software that is mostly meaning-rich context under a surface: a trip
planner, baby tracker, migraine log, reading system, local CRM, ops tracker,
contract register, decision log, backlog, internal admin panel, support queue,
or project system. Underneath, these are records, relationships, workflows, and
judgment.

The old default was Postgres + backend + React because that was the default
shape. Now much of this territory can start as db.md + agent harness +
generated UI.

**db.md replaces the layer where fluidity matters more than hard transactional
machinery.** The records are the files, the schema is text, the relationships
are links, and the agent answers questions or builds the surface the moment you
ask for it. Add a field by adding frontmatter. Split a type by editing `DB.md`.
Let the agent repair the store because it can read the store. The database
becomes fluid because the thing operating it understands the medium.

Karpathy's April 2026 LLM Wiki is the proof of life: a model can maintain a
coherent markdown world. db.md generalizes that from a wiki into a database.
A company brain is one obvious use case. So is personal software. So is
home-cooked software. So is the next agent-native product whose shape changes
every week. None of those is the category. The category is agent-native
persistence: the database layer for software written, operated, and reshaped by
models.

## How it works

One directory. Two folders for your data, and one file that runs the place.

```
db/
├── DB.md          # identity, agent instructions, policies, schemas
├── index.md       # a catalog the agent keeps current
├── sources/       # evidence, kept as it arrived: emails, PDFs, exports, and
│                  #   notes that capture what someone told the agent
└── records/       # everything the agent authors: contacts, companies, expenses,
                   #   meetings, and synthesis — tagged by a meta-type field
```

`sources/` holds evidence in two kinds: documentary (artifacts from outside the
operator's hand — emails, PDFs, exports) and testimonial (a `note` that records
what a person told the agent, with a `told_by` field). `records/` holds
everything the agent writes, and a `meta-type` frontmatter field marks what kind
of record it is: `fact` (the default), `operational`, or `conclusion`. A
`conclusion` is the synthesis layer — the linked narrative the agent compiles
from the rest.

Picture a one-person agency running a couple of clients. The same two folders
fill in like this:

```
db/
├── DB.md
├── index.md
├── sources/
│   ├── contracts/
│   │   ├── northwind-msa.pdf
│   │   └── lumen-sow-q2.pdf
│   ├── emails/
│   │   └── 2026/06/2026-06-02-lumen-invoice-question.md
│   └── notes/
│       └── 2026/06/northwind-wants-weekly-updates.md   # told_by: Dan Ruiz
└── records/
    ├── clients/
    │   ├── northwind.md     # meta-type: fact — retainer, renewal, status
    │   └── lumen-health.md
    ├── contacts/
    │   ├── dan-ruiz.md      # Northwind, founder
    │   └── priya-shah.md    # Lumen, billing
    ├── projects/
    │   └── northwind-site-redesign.md   # meta-type: operational — live status, next step
    ├── invoices/
    │   ├── 2026-039-northwind.md
    │   └── 2026-041-lumen.md
    ├── accounts/
    │   └── northwind.md     # meta-type: conclusion — the account, synthesized
    └── synthesis/
        └── pipeline.md      # meta-type: conclusion — every client, status, next step
```

Each client is some evidence in `sources/` and a spread of records in
`records/`: the contact, the project, the invoices, each linking back to the
contract or call that produced it. A `project` is `operational`: its body is the
live state, rewritten as the work moves, not a log. The agent's read on an
account (`accounts/northwind.md`) and on the whole book of business
(`synthesis/pipeline.md`) are `conclusion`s it keeps current as the facts move
under them. Add a client and the shape repeats. Nothing new to stand up.

`DB.md` is the file that matters most. It holds the store's identity, the
instructions for the agent, the policies it has to follow, and the schemas your
records conform to. The agent reads `DB.md` first and curates everything else
against it. You never write a config format or stand up a service. The agent
writes `DB.md` for you and keeps it honest.

Bring any agent runtime. Claude Code, Codex, or your own. It plays the curator:
reading the files, writing new ones, keeping the links and the catalog in order,
following the contract in `DB.md` and the [spec](SPEC.md). The format is at
v0.3, and from here changes are additive. See the [CHANGELOG](CHANGELOG.md).

| Old stack part | db.md shape |
|---|---|
| Row | Markdown record |
| Column | YAML frontmatter field |
| Foreign key | Wiki-link |
| Migration | Text edit to `DB.md` plus agent repair |
| Index | `index.md` for browsing, `index.jsonl` for complete structured reads |
| Backend logic | Agent harness plus deterministic `dbmd` operations |
| UI | Chat, voice, canvas, forms, approval cards, dashboards, or generated UI |

## How it compares

A wave of products sells "memory" for agents, and a longer history of databases
sells structure. Each ships a system you adopt and maintain; db.md ships a
convention you own. To see the difference, ask three questions of every option:
what sits in the middle, what does it ride on as the world improves, and what
has to stay true for it to work. **db.md puts nothing in the middle, rides the
model curve directly, and bets that models improve faster than schema matters.**
The alternatives either ride on machinery you maintain (which only improves when
someone does the work), bet that format standardization solves the problem, or
bake the data into model weights you cannot read.

Each row is an approach, tagged by **kind**; rows are grouped by kind, so you
can read down by paradigm or jump to a product you already know by name.

| Approach | Kind | What sits between you and your data | What it rides on | The bet / what has to stay true |
|---|---|---|---|---|
| **db.md** | **Files, direct** | nothing. The data is the files. You read and edit them directly, and so does the agent | the model curve, directly. Every new model works the same files better, with no vendor migration and no proprietary index | agents improve faster than infrastructure. Two layers (sources + records) plus a meta-type field and schema repair work because models are smart enough. Files outlast all tools |
| Karpathy's LLM Wiki | **Files, direct** | nothing. Plain markdown the model reads directly | the model curve. Better models read files better, with no index to rebuild | models improve fast enough to read context directly. The foundational idea db.md builds on |
| Open Knowledge Format (OKF) | **Files, direct** | nothing. The data is files; linking is standard markdown | format standardization, not model improvement. Works at any capability level | format simplicity is the bottleneck, not model capability. A shared spec enables exchange across orgs and systems. Google's spec; portability-first, not operational database work |
| GBrain | **Retrieval stack** | a Postgres + pgvector engine and reranker in front of the files | the model curve **and** a maintained graph+embedding stack | files alone aren't enough; the graph and index are load-bearing. Its own benchmark says the graph carries the result and the embeddings add almost nothing |
| Mem0 | **Retrieval stack** | a vector-and-graph service you call; your memories mediated by a retrieval stack | an extract → embed → retrieve pipeline you maintain | similarity retrieval over extracted facts beats reading files. Its 2026 migration adds ADD-only extraction, hybrid search, and entity linking, moving toward dated, linked facts |
| Zep / Graphiti | **Retrieval stack** | a hosted temporal knowledge graph and its API, a second structure kept in step with your data | a derived graph you keep synced | a temporal graph layer beats raw files, and the sync cost is worth it |
| Microsoft GraphRAG | **Retrieval stack** | a knowledge graph an LLM extracts from your corpus, plus community summaries and an index, queried instead of the files | an offline extraction pipeline you re-run as the corpus changes, plus the model | an LLM-built graph answers better than reading the files, and the derived graph is worth extracting, storing, and rebuilding as data drifts |
| Letta / MemGPT | **Memory runtime** | a runtime and retrieval layer around the context window (editable memory blocks plus archival retrieval) | a memory-management runtime you adopt | managing what's in context beats durable files. db.md keeps durable state as files instead |
| Cartridges / Engram | **Parametric** | the model's own weights. The data is dissolved into parameters and a trained KV-cache; nobody reads or edits it, not you and not the agent | its own training research (cartridges, sparse memory fine-tuning, continual learning) plus two bets: that reading the whole context at query time stays costly enough to amortize into weights once, and that doing so beats long-context recall. The amortization bet is real; the recall bet runs *against* the model curve, and as long-context gets cheaper and sharper that half of the wedge shrinks | context is too lossy and too costly to read at query time, so knowledge must be baked into weights offline. Updating a fact means re-distillation; erasing one is training-side, not a file edit; provenance has no address |
| SQL / graph (Neo4j) | **Structured DB** | a schema you design up front, a query language (SQL or Cypher), and an app to drive it | your schema and the app layer. Better models write better queries, but the store's meaning lives outside the model | a relational or graph schema, designed up front, is stable enough. Apps mediate between the model and the data. Migrations are acceptable costs |
| Airtable, Notion | **SaaS** | a vendor's service you rent, your data on their servers, exports that commonly lose live behavior, views, formulas, relations | the vendor's roadmap and release cycle. You get the AI they bolt on, when they ship it, inside their walls | outsourcing is cheaper than operating. Vendor roadmap aligns with your needs. Platform lock-in is acceptable |

The fight db.md picks most directly is with the retrieval stack. **db.md
computes, stores, and searches no vector, ever.** RAG engineers a retrieval
pipeline over embeddings of your data; db.md keeps the data as files and lets the
model read them, with semantic recall coming from the agent widening its own
search in plain language. An embedding does not naturally tell you when a fact
was true or whether something later replaced it; a dated file can. The memory
stack is already moving this way (Mem0's hybrid search and entity linking), and
GBrain and db.md are two readings of the same Karpathy LLM Wiki pattern, one with
Postgres, pgvector, and a reranker in front of the files and one without. db.md
keeps it the size Karpathy drew.

The opposite end of the field is parametric memory: bake the corpus into model
weights or a trained KV-cache, the bet behind Cartridges and Engram, so the
model answers from weights instead of reading files. It is the mirror image of
db.md. Where db.md keeps every fact addressable, dated, and editable on disk,
parametric memory dissolves them into parameters: you gain whole-corpus recall
in one object and you lose provenance, freshness, deletion, and the ability for
the agent to fix a wrong fact by editing a file. db.md rides the same curve the
other way. It owns no memory machinery, reads whatever the best model can read,
and gets better for free. Parametric memory is at most a cache the field may
someday compile from a store like db.md; db.md's bet is that you never need the
cache.

The memory layer was always a database with the data hidden. db.md is the same
job with the data left in the open. It composes with AGENTS.md for instructions
and MCP for tools. Different layers, not rivals.

## Why files

The application database has been a service for decades: a daemon, a wire
protocol, a migration tool, an admin panel. That made sense when useful software
over data had to be built around a database engine. It is no longer the only
shape.

db.md turns the shape inside out:

- **The database is the directory.** There is no daemon and no port. You can
  `cd` into it and `ls` your data.
- **Structured fields live in frontmatter.** They are typed, optional, and
  additive. You change the store shape by editing text, not by running a
  migration. Add a field, rename a type, tighten a schema in `DB.md`; the agent
  can read the diff and repair the records.
- **The index is derived.** A plain catalog plus embedded ripgrep is built to
  carry millions of files with no vector database. Want SQLite or a search
  index on top? Build one. The files stay the source of truth.

**On portability:** db.md is portable by default. It's just files. Git, tarballs, sync services move them. A capable model reads them directly without needing a format standard. That's the bet: as models improve, they read the same files better. If you need to guarantee a third party can read your knowledge without knowing db.md, OKF is a minimal exchange layer. Export, lose schema and semantics, gain portability. db.md is the operational store. OKF is the transport. Model capability outpaces format standardization over time. Format distinctions fade.

## How far it scales

Put a number on "millions." Using roughly 120 sent-plus-received work emails a
day, a person who indexes their mail adds about **44,000 files a year**, around
440,000 in a decade; a heavy whole-career archive fits around **1 to 1.5
million plain files**. **A ten-person shared store can cross a million files in
two to three years.** That is the scale this format is built to hold.

And the agent should not pay the whole-store cost in its normal loop, because
**the agent does not have to inspect every file; it reads indexes.** Every type
folder keeps a human `index.md` (the 500 most recent entries) and a complete
machine `index.jsonl`, both updated on writes. A query reads the relevant
sidecar and goes straight to the right record. The interactive loop is designed
around **O(changed), not O(store)**: what an operation costs should track what
changed, not how big the store has grown.

- **High-volume folders shard by date.** When the agent writes through
  `dbmd write`, source and event types are placed in the shard path
  automatically: an email lands in `sources/emails/2026/05/`, an expense in
  `records/expenses/2026/05/`. The agent supplies the type and date; `dbmd`
  does the folder math. No directory grows unbounded, and only the current shard
  is ever hot. Entity records and conclusions stay flat, because those sets are
  bounded by reality: you have only so many customers, however much mail they
  send.
- **The measured 10k tier is interactive; the million-file tier is an opt-in
  gate.** Sidecar reads are millisecond-scale at 10k, typed/full-text searches
  and working-set validation stay inside their documented budgets, and full
  sweeps run off-loop. Write paths are currently near, not under, the tight
  100ms target because they compact a type-folder `index.jsonl`; that gap is
  documented in [tests/PERF.md](tests/PERF.md). The 1M test is opt-in and
  asserts the sidecar-backed loop/sweep targets when run.
- **Whole-store passes run off the loop.** A full `dbmd validate --all` or
  index rebuild is a linear repair and audit job you schedule. The agent never
  waits on one.

The first practical ceiling you hit is often git, not the format. Large working
trees need tuning (`fsmonitor`, `feature.manyFiles`, sparse/partial checkout,
or Scalar-style tooling) because git's index is still one structure with an
entry for every tracked path. Git is optional tooling over db.md, not part of
the format. At that point, version `records/` (where the agent's curated data
and conclusions live) and let high-volume sources ride filesystem snapshots. The
files keep working exactly the same.

What still wants a real engine: heavy write concurrency, ACID transactions,
sub-millisecond reads, aggregates over billions of rows. That territory is
the packed flavor on the [roadmap](SPEC.md#roadmap), with the same contract
and the files still the source of truth. Until then the two compose cleanly.

## Quick start

db.md is operated by agents, and the installer is text. The quick start is a
prompt you hand to an agent, and it covers both cases: a clean start, or
bringing an existing knowledge base across. You do not have to decide which.
The agent looks at what you have and proposes the path. It is safe to paste:
the install chain is verifiable, and the install stays fast. Paste this into
Claude Code, Codex, or any agent with a shell:

```text
Read https://raw.githubusercontent.com/carloslfu/db.md/main/llms.txt and set
up db.md on this machine: install dbmd, run `dbmd spec` to load the standard,
then create a store. By default the store is version-controlled, never a bare
folder at a global path. That is the design, not a preference: the store is
plain files whose whole point is that Git or a sync service can save, version,
and carry them. So version-control it by default. If this is a git repo and the
data I want in the store already lives here, create the store inside the repo
(for example ./db/) so it rides this repo's history, and do not move repo-owned
files out without asking me. If there is no repo, git init the store itself and
offer to wire a remote (a folder I already sync is also fine). Only skip version
control if I explicitly tell you I want a throwaway, unversioned store. If I
already have notes, exports, or a knowledge base to bring in (a folder of files,
an Obsidian vault, a Notion export, anything), evaluate it first and show me a
migration plan that maps it in by provenance, not polish: whatever I didn't
author is a source (however finished it looks), and the synthesis you write from
it is a record with meta-type: conclusion. Reorganize by type, meta-type, and
links rather than mirroring the old folder tree. Migrate it once I approve,
preserving each source's provenance and verifying nothing was lost. Moving the data is only half of it:
also find whatever already connects to that knowledge base (my skills, commands,
and scripts) and update them to point at the new store, so nothing keeps reading
the old location. Don't create long-lived migration-map pages or temporary
artifacts unless I ask; git is the audit and rollback trail. Confirm where the
store lives and how it is backed up before you scaffold.
```

The agent reads [llms.txt](llms.txt), installs the binary, and loads the
contract. If you have existing data, it evaluates it and proposes a migration
into the two layers (sources and records, synthesis written as records with
meta-type: conclusion) with frontmatter and links, then moves it once you
approve. If you are starting fresh, it scaffolds an empty store. Either way it
writes `DB.md` and curates from there.

Want to confirm it is safe before trusting it? You do not have to verify
anything to install, but you can: [Safe to paste](#safe-to-paste) below has
the receipts and a one-line verify command, and you can ask your agent to run
the audit for you.

Installing by hand is the same one Rust binary, about 6MB in the current
release build, no toolchain:

```bash
curl -fsSL https://raw.githubusercontent.com/carloslfu/db.md/main/scripts/install.sh | sh
# or: brew install carloslfu/tap/dbmd
# or: cargo install dbmd-cli    # build from source
```

Load the contract once per session. `dbmd spec` prints the whole standard.
Then, from inside the store:

```bash
dbmd search "renewal" --in records                   # search content and frontmatter
dbmd query --type contact --where status=active      # filter by frontmatter
dbmd links records/contacts/elena-rodriguez          # who links to this record
dbmd graph neighborhood records/companies/northstar  # the local web around a record
dbmd validate                                        # frontmatter, links, schemas, all checked
```

Every command speaks `--json`, so anything you build on top reads it cleanly.

### Safe to paste

You do not need to verify anything to install. The install is the fast path
above. But a prompt that ends in an installed binary deserves the option, so
the chain is built to be checked, by you or by the agent you hand it to:

- **The installer is readable.** [`scripts/install.sh`](scripts/install.sh)
  is about 140 lines of POSIX sh: detect the platform, download the tarball
  from this repo's GitHub Releases, verify its SHA-256 against the release's
  `SHA256SUMS`, install to `~/.dbmd/bin`. No sudo, no shell-config edits,
  nothing outside that folder. `DBMD_VERSION` pins a version.
- **Every binary traces back to source.** Releases are built in CI from a
  tagged commit, never on a developer's laptop, and every tarball carries a
  signed build-provenance attestation tying it to the exact commit and
  workflow that built it. Anyone can check it:

  ```bash
  gh attestation verify dbmd-<version>-<target>.tar.gz --repo carloslfu/db.md
  ```

- **The binary makes no network calls.** No telemetry, no API keys, no AI
  SDKs. `dbmd` reads and writes local files and does nothing else.
- **No stored publish token.** crates.io releases go through Trusted
  Publishing (OIDC): CI mints a short-lived token per release, so there is
  no long-lived registry credential to leak.
- **The dependency tree is audited in CI.** Small, permissively licensed,
  zero AI crates. The license allowlist is machine-enforced, and RustSec
  advisories run on every dependency change and on a daily schedule.

Do not take the list's word for it. The audit is one more prompt:

```text
Read scripts/install.sh and .github/workflows/release.yml in carloslfu/db.md
and tell me whether this is safe to install.
```

If you want no prebuilt binary at all, `cargo install dbmd-cli` builds from
source. [SECURITY.md](SECURITY.md) holds the threat model.
[RELEASING.md](RELEASING.md) documents the release pipeline end to end.

## The agent is the engine

db.md ships no model and no API keys. The curator is whatever agent you already
use: Claude Code, Codex, or your own. The whole flow is four moves. It discovers
db.md, runs `dbmd spec` for the contract, reads the store's `DB.md`, then
operates with `dbmd`.

The binary is deterministic plumbing. The agent does the thinking. You are
never locked to a model, because the model is the one part you bring and the
one part that keeps improving.

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
  epub, and html through permissively licensed Rust crates. No GPL `pdfgrep`,
  no AGPL `rga`.
- **Zero AI dependencies.** No provider SDKs, no API keys, no model calls in
  the binary. The agent runtime is yours.
- **A library underneath.** All the logic lives in `dbmd-core`. Run `cargo
  add dbmd-core` to build your own db.md-aware tool.

See [TOOLS.md](TOOLS.md) for the full command surface and the agent bootstrap.

## What's in this repo

```
db.md/
├── SPEC.md          # the format, the curator contract, the validation codes (v0.3)
├── TOOLS.md         # the toolkit: every subcommand, install, agent bootstrap
├── crates/
│   ├── dbmd-core/   # the library: parser, store, graph, validate, query, index, log
│   └── dbmd-cli/    # the dbmd binary (thin wrappers over the library)
├── examples/        # five complete stores: research wiki, ops, second brain, agency, CRM
├── tests/corpora/   # canonical, edge-case, format, scale, and agent-eval stores
└── skills/db-md/    # the canonical Agent Skill you place in your own agent
```

The examples and corpora are the proof surface: small enough to read, complete
enough to exercise the real contract, and varied enough to show the shape across
personal, team, research, agency, and customer-data stores.

## Use it on its own

db.md is an open standard, and it stands on its own. A plain markdown vault
becomes a db.md store, with no platform and no account required: a personal
app, a family tool, an Obsidian vault, a research wiki, an agent-built internal
tool, a customer database, an agentic computer's operating store, any runtime
with a folder of markdown. The [spec](SPEC.md) is the contract. The runtime is
replaceable. **The files outlast both.**

## License

[Apache-2.0](LICENSE), including the Apache patent grant and NOTICE/attribution
terms. First-time contributors sign the Apache ICLA through the CLA Assistant
bot. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Sign the Apache ICLA through the CLA
Assistant bot on your first pull request.

## Security

Report vulnerabilities privately through GitHub's "Report a vulnerability"
button on the Security tab. Do not open a public issue for a security problem.
See [SECURITY.md](SECURITY.md) for the threat model.

The supply chain is covered in [Safe to paste](#safe-to-paste) above: built
in CI from tagged commits, checksummed, provenance-attested, published with
no stored token, dependency policy machine-enforced.
[RELEASING.md](RELEASING.md) documents the release pipeline end to end.

## Star history

<a href="https://www.star-history.com/#carloslfu/db.md&Date">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=carloslfu/db.md&type=Date&theme=dark" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=carloslfu/db.md&type=Date" />
   <img alt="Star history chart for carloslfu/db.md" src="https://api.star-history.com/svg?repos=carloslfu/db.md&type=Date" />
 </picture>
</a>
