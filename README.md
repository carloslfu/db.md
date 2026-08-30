# db.md

**Your database is a folder of plain text files.** No daemon, no fixed
tables, no query language in the way. Every record is one markdown file
you can open, read, and edit by hand. The relationships between records
are written into the text itself.

The folder is the database.

The index still exists. It is derived: a plain catalog riding beside
the files, read by queries, rebuilt from the files themselves. Sort
everything, group by year, filter by any field: one command. The full
answer is [Where is the index?](#where-is-the-index).

Before agents, text was documentation. After agents, text is operational
state. A capable agent reads the files, writes them, links them, repairs
them, and finds the connections between them by meaning. The agent is the
engine, and the engine improves on its own: your files ride the model
curve.

Here is a record. It is a file:

```markdown
---
type: trip
name: Kyoto spring trip
dates: 2026-04-11..2026-04-18
status: planning
summary: Seven days in Kyoto with Maya and Jules; ryokan booked, one open afternoon still unplanned
travelers:
  - [[records/people/maya]]
  - [[records/people/jules]]
home_base: [[records/places/kyoto-station]]
created: 2026-01-12T09:15:00-07:00
updated: 2026-06-03T14:20:00-07:00
---

# Kyoto spring trip

Seven days in Kyoto with Maya and Jules. The current plan keeps the first
two nights near [[records/places/kyoto-station]], then moves to the ryokan
from [[sources/emails/2026-06-03-ryokan-confirmation]]. Jules wants temples
in the morning, Maya wants one open afternoon for wandering, and nobody wants
another spreadsheet.
```

The YAML block on top is frontmatter: the structured surface of the record,
simple labels the agent can sort, filter, and repair, with store-specific
schemas declared in `DB.md`. The `[[double brackets]]` are the
relationships, the same links a wiki uses. The body holds what a database
row usually throws away. A person reads it. Git versions it. A model reads
it with full context. That is the whole format. (Records the tooling writes
also carry a stable `id`, a lowercase ULID minted on create. It is
recommended, not required: a hand-written file like this one stays fully
valid.)

**A huge class of future software will not be built as database + backend +
frontend. It will be built as readable context + agent harness + generated
surface. db.md is the persistence layer for that world:** records are
markdown files like the one above, fields are YAML frontmatter,
relationships are wiki-links, schemas and policies live in one `DB.md`, an
agent operates all of it, and there is no vector database anywhere.
Designed for stores that grow into millions of plain files. The full
argument is in [WHY.md](WHY.md).

## Quick start

db.md is operated by agents, and the installer is text. Paste this into
Claude Code, Codex, or any agent with a shell:

```text
Read https://raw.githubusercontent.com/carloslfu/db.md/main/llms.txt and set
up db.md here: install dbmd, load the standard, and create a store. If I
have existing notes or a knowledge base (an Obsidian vault, a Notion export,
a folder of files), evaluate it first and show me a migration plan before
moving anything.
```

"Here" is whatever folder your agent is open in: start your agent where
your notes live to migrate them, or in an empty folder for a fresh start. The agent
reads [llms.txt](llms.txt), installs one small binary, loads the contract,
and either scaffolds a fresh store or maps your existing notes into one
with your approval, preserving where every piece came from. Stores are
version-controlled by default: plain files that Git or any sync service
saves and carries.

Prebuilt releases cover macOS, Linux, and Windows. On macOS or Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/carloslfu/db.md/main/scripts/install.sh | sh
# or: brew install carloslfu/tap/dbmd
# or: cargo install dbmd-cli    # build from source
```

On Windows, from PowerShell:

```powershell
irm https://www.sevrahq.com/install/dbmd.ps1 | iex
```

Then, from inside a store:

```bash
dbmd spec                                  # print the whole standard
dbmd search "renewal" --in records         # search content and frontmatter
dbmd query --type contact --where status=active   # filter by frontmatter
dbmd ask "who is up for renewal this quarter?"    # the same store, in English
```

Every command speaks `--json`, so anything you build on top reads it
cleanly. A prompt that ends in an installed binary deserves receipts:
[Safe to paste](#safe-to-paste) has them.

## Watch it operate

The examples in this repo are complete stores. Take
[`examples/agency-knowledge-base`](examples/agency-knowledge-base), the
store of a twelve-person agency. Ask your agent to log a client kickoff,
and this is the loop it runs:

```bash
$ dbmd query --type client --where status=active
records/clients/brightmore-group.md
records/clients/lumio.md
records/clients/riverkeep.md

$ dbmd write records/meetings/2026-07-01-lumio-kickoff --type meeting \
    --summary "Lumio spring campaign kickoff; scope and dates agreed" \
    --fm date=2026-07-01 --fm 'client=[[records/clients/lumio]]' \
    --fm 'attendees=[[records/contacts/maya-okonkwo]]'
records/meetings/2026/07/2026-07-01-lumio-kickoff.md

$ dbmd graph backlinks records/clients/lumio
records/contacts/maya-okonkwo
records/contacts/theo-ramos
records/meetings/2026/07/2026-07-01-lumio-kickoff
records/meetings/2026-05-21-lumio-brand-review
records/projects/lumio-brand-identity

$ dbmd validate
0 issue(s): 0 error(s), 0 warning(s), 0 info
```

The write landed in the right date shard without anyone doing folder math.
The new meeting shows up in the client's backlinks. And `validate` held it
to the `meeting` schema this store declares in its `DB.md`: link an
attendee who doesn't exist and it fails loudly with `WIKI_LINK_BROKEN`.
The binary is deterministic plumbing; the agent does the thinking.

## How it works

One directory. Two folders for your data, one file that runs the place,
and a derived catalog.

```
db/
├── DB.md          # identity, agent instructions, policies, schemas
├── index.md       # a catalog the agent keeps current
├── sources/       # evidence, kept as it arrived: emails, PDFs, exports, and
│                  #   notes that capture what someone told the agent
└── records/       # everything the agent authors: contacts, invoices,
                   #   meetings, and synthesis, tagged by a meta-type field
```

`sources/` holds evidence. `records/` holds what the agent writes, where a
`meta-type` field separates atomic facts (`fact`, the default) from
operating state (`operational`) and from the synthesis the agent keeps
current as the facts move under it (`conclusion`).

Picture a one-person agency running a couple of clients:

```
db/
├── DB.md
├── sources/
│   ├── contracts/northwind-msa.pdf
│   ├── emails/2026/06/2026-06-02-lumen-invoice-question.md
│   └── notes/2026/06/northwind-wants-weekly-updates.md   # told_by: Dan Ruiz
└── records/
    ├── clients/northwind.md                    # retainer, renewal, status
    ├── contacts/dan-ruiz.md                    # Northwind, founder
    ├── projects/northwind-site-redesign.md     # meta-type: operational
    ├── invoices/2026-039-northwind.md
    ├── accounts/northwind.md                   # meta-type: conclusion -
    │                                           #   the account, synthesized
    └── synthesis/pipeline.md                   # meta-type: conclusion -
                                                #   every client, next step
```

Each client is some evidence in `sources/` and a spread of records linking
back to the contract or call that produced them. Add a client and the shape
repeats. Nothing new to stand up.

`DB.md` is the file that matters most: the store's identity, the
instructions for the agent, the policies it follows, and the schemas your
records conform to. There is no config format to learn. The agent writes
`DB.md` for you, and `dbmd validate` holds every record to it.

| Old stack part | db.md shape |
|---|---|
| Row | Markdown record |
| Column | YAML frontmatter field |
| Foreign key | Wiki-link |
| Migration | Text edit to `DB.md` plus agent repair |
| Index | [Derived sidecars](#where-is-the-index): `index.md` for browsing, `index.jsonl` for structured reads |
| Backend logic | Agent harness plus deterministic `dbmd` operations |
| UI | Chat, voice, forms, dashboards, or whatever the agent generates |

The format and the toolkit are versioned independently. `SPEC.md` carries
the format version; repo tags carry toolkit releases. The format is at v0.4,
with an additive policy from v0.3 on: new fields and codes layer on, and
existing stores keep validating. The contract is [SPEC.md](SPEC.md); the
current toolkit release and the history of both axes are in the
[CHANGELOG](CHANGELOG.md).

## What it is for

Software that is mostly meaning-rich context under a surface: a
[local CRM](examples/customer-database), an
[ops tracker](examples/ops-store), a contract register, a decision log, a
support queue, a [research system](examples/research-wiki), a
[second brain](examples/personal-second-brain), a company brain, a family
tool, a trip planner. Underneath, these are records, relationships,
workflows, and judgment. Most were always too small, too specific, or too
alive to justify becoming SaaS products. With an agent operating the
files, they stop needing to be products at all.

Hard truth still exists. Payments, ledgers, high-concurrency shared state,
sub-millisecond reads, and billion-row analytics still want hard engines.
Postgres is for authoritative machinery; db.md is for living context.

## How it compares

Ask one question of every option: **who operates the live store?** A
vendor's app operates Notion and Airtable; you rent the machinery and get
the AI they bolt on, when they ship it, inside their walls. A retrieval
pipeline operates a vector-memory stack (Mem0, Zep, GraphRAG); embeddings,
graphs, and rerankers stand between the agent and your data, and you keep
them synced. A schema and an app operate SQL; better models write better
queries, but the store's meaning lives in schema and app code, outside the
data itself.

In db.md, the agent operates the store directly, on files you can read.
Nothing sits in between. **db.md computes, stores, and searches no vector,
ever.** Semantic recall is the agent widening its own search in plain
language, and a dated file can say when a fact was true and what replaced
it; an embedding by itself cannot.
[Karpathy's LLM Wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f)
showed a model can maintain a coherent markdown world; db.md turns that
demonstration into a database format and keeps it the size he drew.

The full field, including parametric memory and the interchange formats, is
worked through in [WHY.md](WHY.md#the-comparison-field). db.md composes
with AGENTS.md for instructions and MCP for tools: different layers, not
rivals.

## How far it scales

Designed for millions of plain files. A person who indexes their work
email adds about 44,000 files a year; a ten-person shared store can cross
a million files in two to three years. The agent never pays the
whole-store cost in its interactive loop: every type folder keeps a small
derived index
(`index.md` for people, `index.jsonl` for machines), high-volume folders
shard by date when the agent writes through `dbmd write`, and the
interactive loop is O(changed), not O(store). Whole-store validation and
index rebuilds are sweep jobs that run off the loop. The interactive
budgets are measured at the 10k-file tier in CI; the million-file tier is
an opt-in test with published targets. Both are in
[tests/PERF.md](tests/PERF.md), and the full scale math is in
[WHY.md](WHY.md#how-far-files-go).

One writer, many readers is the local file contract. A store assumes a
single curating agent, and teams share it the way they share a repo: people
direct the curator and read freely, clones move through git, append-only
logs merge by union, and the derived indexes regenerate with `dbmd index
rebuild` rather than merge. Want SQLite or a search index on top? Build one;
the files stay the source of truth.

## Safe to paste

Start with the fact that matters most: **the binary makes no network call
you did not ask for.** There is no telemetry, no auto-update, no AI SDK,
and no endpoint baked in. Local format commands stay local. Exactly two
command families reach the network, both only when you run them and both
only to a place you named: the [sync commands](#sync-if-you-want-it) talk
to the hub you select, and [`ask` / `do` / `build`](#ask-it-in-english)
talk to the model endpoint you configure. You don't have to take this
page's word for anything. The audit is one more prompt:

```text
Read scripts/install.sh, scripts/install.ps1, and .github/workflows/release.yml
in carloslfu/db.md and tell me whether this is safe to install.
```

For the reader who verifies by hand, the chain:

- **The installers split authority.** [`scripts/install.sh`](scripts/install.sh)
  and [`scripts/install.ps1`](scripts/install.ps1) resolve `latest` and the
  expected SHA-256 from an independently deployed manifest, then download the
  release from GitHub. A compromised release origin cannot choose both bytes
  and digest. They write only to the user's db.md install directory, require no
  administrator access, and do not edit shell configuration. `DBMD_VERSION`
  pins a version.
- **Every binary traces back to source.** Releases are built in CI from a
  tagged commit, never on a laptop, and every binary artifact carries a signed
  build-provenance attestation anyone can verify:
  `gh attestation verify <downloaded-file> --repo carloslfu/db.md`
- **No stored publish token.** crates.io releases go through Trusted
  Publishing (OIDC): CI mints a short-lived token per release.
- **The dependency tree is audited in CI.** Small, permissively licensed,
  zero AI crates, license allowlist machine-enforced, RustSec advisories on
  every dependency change and on a daily schedule.

If you want no prebuilt binary at all, `cargo install dbmd-cli` builds from
source. [SECURITY.md](SECURITY.md) holds the threat model, including the
one that matters in daily use: prompt injection through ingested sources,
and why treating sources as data rather than instructions is the
harness's job. [RELEASING.md](RELEASING.md) documents the release pipeline
end to end.

## The agent is the engine

db.md ships no model and no API keys. The curator is whatever agent you
already use: Claude Code, Codex, or your own. Its whole flow is four moves:
discover db.md, run `dbmd spec` for the contract, read the store's `DB.md`,
operate with `dbmd`. You are never locked to a model, because the model is
the one part you bring and the one part that keeps improving.

To make your agent reach for db.md on every session, place the canonical
skill ([`skills/db-md/SKILL.md`](skills/db-md/SKILL.md), in the open
[Agent Skills](https://agentskills.io) format) where your harness reads
skills. There is no install command for this, on purpose: copy the file,
use your agent's own skill installer, or tell the agent to set itself up.

## Ask it in English

Bring your own agent and it operates the store natively. That is the main
path and the better one. But some callers cannot host an agent: an app
talking to the store, or a machine with a model on it and nothing to drive
it. For those, `dbmd` carries a deliberately tiny harness of its own — a
tool-calling loop, a few hundred lines, whose only tools are the same
`dbmd` verbs you just saw.

```bash
dbmd ask   "which invoices are unpaid and older than 60 days?"   # read verbs
dbmd do    "mark the Lumio invoice paid and log it"              # + write verbs
dbmd build "make the invoice view group by client"               # + workspace files
```

The verb is the permission. `ask` has no write tools at all, so content
injected through an ingested source can produce a wrong answer and nothing
further. `do` writes only through the same verbs you would run by hand, so
schema checks, frozen pages, link-aware deletes, and the store log all
still apply. `build` adds file edits inside a workspace you declare.

**It is not a coding agent, and it is not trying to become one.** There is
no shell at any level, so it cannot install a dependency, run your build,
run your tests, or check its own work — a real agent does that, and this
one is a doorbell rather than a tenant. What it is good at is the thing it
is scoped to: answering questions about a store, and making changes through
the store's own contract.

The model is yours: a local server is found automatically (Ollama, LM
Studio, llama.cpp), an API key works through
`--provider anthropic|openai|openrouter|…`, and a ChatGPT subscription
signs in with `dbmd login codex`. No default vendor, no key ever stored
inside a store, nothing metered. Apps get the same loop over loopback HTTP
with `dbmd api --ask`.

## The toolkit

db.md is plain files, so any tool that reads files works. The reference
toolkit is one Rust binary, `dbmd`, in the git / cargo / kubectl shape.

- **Embedded ripgrep.** Fast search with no separate tool to install.
- **Built-in extraction.** `dbmd extract` pulls text out of PDF, docx,
  xlsx, epub, and html.
- **Local operations.** Read, write, edit, query, validate, link, rename,
  delete, index, emit, watch, introspect schemas, and audit a store without
  a daemon — and serve that whole surface to local apps over HTTP
  (`dbmd api`) when one is wanted.
- **A harness when you need one.** [`ask` / `do` / `build`](#ask-it-in-english)
  run those same verbs from a plain-English request, against the model you
  configure.
- **An optional network client.** Address and sync brains, grant access,
  review proposals, manage keys, follow changes, and mirror or re-serve a
  verified copy through [link.md](https://github.com/carloslfu/link.md)
  when you select a hub.
- **No AI dependencies, no vendor.** No provider SDKs, no bundled model,
  no endpoint baked in, nothing metered. Every verb is deterministic; the
  one part that calls a model is [`ask` / `do` / `build`](#ask-it-in-english),
  and it calls the model *you* configure.
- **A library underneath.** All the logic lives in `dbmd-core`. Run
  `cargo add dbmd-core` to build your own db.md-aware tool.

Run `dbmd --help` for the exact command surface. [TOOLS.md](TOOLS.md)
explains the toolkit and agent bootstrap.

## What's in this repo

```
db.md/
├── SPEC.md          # the format, the curator contract, the validation codes (format v0.4)
├── WHY.md           # the argument: the stack collapse, the comparison field, the scale math
├── TOOLS.md         # toolkit reference, install, agent bootstrap
├── llms.txt         # the agent-readable entry point
├── crates/
│   ├── dbmd-core/   # the library: local store operations + the link.md client
│   └── dbmd-cli/    # the dbmd binary (thin wrappers over the library)
├── examples/        # five complete stores: research wiki, ops, second brain, agency, CRM
├── tests/corpora/   # canonical, edge-case, format, scale, and agent-eval stores
└── skills/db-md/    # the canonical Agent Skill you place in your own agent
```

The examples and corpora are the proof surface: small enough to read,
complete enough to exercise the real contract, and varied enough to show
the shape across personal, team, research, agency, and customer-data
stores.

## Use it on its own

db.md is an open standard, and it needs nothing else. A plain markdown vault
becomes a db.md store, with no platform and no account required: a personal
app, a family tool, an Obsidian vault, a research wiki, an agent-built
internal tool, a customer database, any runtime with a folder of markdown.
The [spec](SPEC.md) is the contract. The runtime is replaceable. **The
files outlast both.**

db.md needs no host. If you want one anyway, [Sevra](https://sevrahq.com)
is the hosted home: your store kept always on, indexed, and curated. The
standard stays neutral, Apache-2.0, and self-hostable no matter where a
store lives.

## Sync, if you want it

When a store should live in more than one place, `dbmd` speaks
[link.md](https://github.com/carloslfu/link.md) to a hub you choose. Run
`dbmd sync @brain` to pull a new checkout. Run it again from that checkout
to reconcile remote and local changes.

Sync moves changed files, never whole snapshots. Every transfer verifies
the hub's signed state and compares both sides against the last baseline
you accepted. Clean edits and deletions flow in either direction; a
divergent edit moves nothing and leaves a private conflict bundle for you
to resolve explicitly. Nothing is silently overwritten, and no model is
ever asked to invent a merge.

Access is permissioned. A grant exposes a whole store or a path-scoped
view, and writes that need review arrive as proposals rather than landing
directly. `.sevralocal` keeps selected files on your machine; moving one
into hosting takes explicit intent. Beyond sync, `resolve` reads one
remote record, `subscribe` follows new heads, and `mirror` plus `serve`
republish a verified read-only copy with its original signatures intact.

All of it is optional client machinery, not a second storage format. No
hub is baked in, local commands stay local, and link.md is not required
for a valid db.md store. Run `dbmd sync --help` for the exact options; the
[interconnect reference](TOOLS.md#interconnect-the-linkmd-client) covers
the deeper mechanics: incremental scans, resumable pulls, phased `DB.md`
changes, and recovery.

## Where is the index?

The first objection every engineer raises, and the right one: a folder
famously lacks the thing that makes a database a database. A filesystem
cannot sort your documents by year. Modification times lie, filenames
carry no schema, and the date inside a PDF is invisible to `ls`.

db.md answers with structure, then a catalog. Every record carries typed
frontmatter, and every type folder keeps `index.jsonl`: one JSON line per
record, updated on every write. `dbmd query` reads that sidecar, never
the whole tree, so the skeptic's example is one command, millisecond-scale
at the measured 10k-file tier ([tests/PERF.md](tests/PERF.md)):

```bash
dbmd query --in records --json |
  jq 'group_by(.created[:4]) | map({year: .[0].created[:4], records: length})'
```

The rule underneath: **the index is derived, never authoritative.**
Delete every `index.md` and `index.jsonl` and nothing is lost; `dbmd
index rebuild` regenerates them from the files, byte for byte, the way
Git regenerates its pack indexes on clone. That cut is what keeps db.md
implementable in an afternoon, and what makes the index something you can
never lose. When a store outgrows the catalog, the rule holds: build a
local SQLite from the files, or let a host maintain a live index. Swap
the cache, never the format.

One more thing the objection misses: "group my documents by year" never
failed on files for lack of a B-tree. It failed because nothing filled in
the year, and Postgres cannot sort by a date nobody extracted either.
Extraction was always the hard part, and extraction is the agent's job at
ingestion. The missing piece was the agent, not the B-tree.

## License

[Apache-2.0](LICENSE), including the Apache patent grant and
NOTICE/attribution terms. First-time contributors sign the Apache ICLA
through the CLA Assistant bot. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Security

Report vulnerabilities privately through GitHub's "Report a vulnerability"
button on the Security tab. Do not open a public issue for a security
problem. See [SECURITY.md](SECURITY.md) for the threat model. The supply
chain is covered in [Safe to paste](#safe-to-paste) above;
[RELEASING.md](RELEASING.md) documents the release pipeline end to end.
