# db.md

**Your database is a folder of plain text files.** No daemon, no fixed
tables, no query language in the way. Every record is one markdown file
you can open, read, and edit by hand. The relationships between records
are written into the text itself.

The folder is the database.

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

Installing by hand gets you the same binary, about 6MB, macOS and Linux,
no toolchain:

```bash
curl -fsSL https://raw.githubusercontent.com/carloslfu/db.md/main/scripts/install.sh | sh
# or: brew install carloslfu/tap/dbmd
# or: cargo install dbmd-cli    # build from source
```

Then, from inside a store:

```bash
dbmd spec                                  # print the whole standard
dbmd search "renewal" --in records         # search content and frontmatter
dbmd query --type contact --where status=active   # filter by frontmatter
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
| Index | `index.md` for browsing, `index.jsonl` for structured reads |
| Backend logic | Agent harness plus deterministic `dbmd` operations |
| UI | Chat, voice, forms, dashboards, or whatever the agent generates |

The format is at v0.4 and the `dbmd` toolkit at 0.6.4. Two versions,
because they are two things: SPEC.md carries the format's, the crates
carry the toolkit's, and repo tags track toolkit releases. The format's
policy from v0.3 on is additive: new fields and codes layer on,
existing stores keep validating. The contract is [SPEC.md](SPEC.md);
the history of both axes is the [CHANGELOG](CHANGELOG.md).

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

One writer, many readers. A store assumes a single curating agent; the
contract says so plainly, and teams share a store the way they share
a repo: people direct the curator and read freely, clones move through
git, append-only logs merge by union, and the derived indexes regenerate
with `dbmd index rebuild` rather than merge. Want SQLite or a search index
on top? Build one; the files stay the source of truth.

## Safe to paste

Start with the fact that matters most: **the binary makes no network
calls.** No telemetry, no API keys, no AI SDKs; `dbmd` reads and writes
local files and does nothing else (check `Cargo.lock`: there is no HTTP
client in the tree). And you don't have to take this page's word for
anything. The audit is one more prompt:

```text
Read scripts/install.sh and .github/workflows/release.yml in carloslfu/db.md
and tell me whether this is safe to install.
```

For the reader who verifies by hand, the chain:

- **The installer is readable.** [`scripts/install.sh`](scripts/install.sh)
  is about 160 lines of POSIX sh: detect the platform, download the tarball
  from this repo's GitHub Releases, verify its SHA-256 against the
  release's `SHA256SUMS`, install to `~/.dbmd/bin`. No sudo, no
  shell-config edits, nothing outside that folder. `DBMD_VERSION` pins a
  version. The checksum proves integrity; provenance is the separate check
  below.
- **Every binary traces back to source.** Releases are built in CI from a
  tagged commit, never on a laptop, and every tarball carries a signed
  build-provenance attestation anyone can verify:
  `gh attestation verify dbmd-<version>-<target>.tar.gz --repo carloslfu/db.md`
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

## The toolkit

db.md is plain files, so any tool that reads files works. The reference
toolkit is one Rust binary, `dbmd`, in the git / cargo / kubectl shape: one
binary, with subcommands for read, write, validate, extract, graph, index,
and log.

- **Embedded ripgrep.** Fast search with no separate tool to install.
- **Built-in extraction.** `dbmd extract` pulls text out of PDF, docx,
  xlsx, epub, and html.
- **Zero AI dependencies.** No provider SDKs, no API keys, no model calls
  in the binary. The agent runtime is yours.
- **A library underneath.** All the logic lives in `dbmd-core`. Run
  `cargo add dbmd-core` to build your own db.md-aware tool.

See [TOOLS.md](TOOLS.md) for the full command surface and the agent
bootstrap.

## What's in this repo

```
db.md/
├── SPEC.md          # the format, the curator contract, the validation codes (format v0.4)
├── WHY.md           # the argument: the stack collapse, the comparison field, the scale math
├── TOOLS.md         # the toolkit: every subcommand, install, agent bootstrap
├── llms.txt         # the agent-readable entry point
├── crates/
│   ├── dbmd-core/   # the library: parser, store, graph, validate, query, index, log
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
