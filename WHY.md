# The folder is the database

*The argument behind [db.md](README.md): why the database + backend +
frontend stack collapses for a large class of software, what replaces it,
how db.md compares to the memory stacks and the engines, and how far plain
files actually go. The [README](README.md) covers what db.md is and how to
use it; this is why it is shaped this way.*

## The workaround is expiring

For fifty years a folder-as-database would have sounded like a toy. Database
engines earned their place by doing hard things files alone did not: durable
writes, indexes, transactions, concurrent access, permissions, fast queries.
But for the broad middle of software, they also carried a workaround:
ordinary programs could not read and maintain meaning-rich state directly,
so messy reality had to be squeezed into tables before software could use
it.

That workaround is expiring. A capable agent operates meaning-rich files
directly: it reads them, writes them, links them, repairs them.

The agent is the engine.

And the engine is the bet. A db.md store gets sharper every time the model
behind it improves. A better model can read the same files with more
context, repair them with more judgment, and reshape the schema without a
migration ceremony. SQL can still be queried by better agents, but the
store's meaning lives in schema and app code. Your files ride the model
curve.

So the database stops being a service you run and becomes data you own:
text on disk that a person reads easily, that a model reads directly, and
that outlasts every tool that ever touches it.

## The stack collapse

For decades, the default app shape was:

```
Database -> Backend -> Frontend
Postgres + service layer + React app
```

The database held the state. The backend encoded the rules. The frontend
exposed fixed views and actions. That shape made sense when programs could
not understand the data they operated on.

Agents change the default.

For a large class of semantic, evolving, workflow-heavy software, the new
shape is:

```
Markdown files -> Agent harness -> Generated surface
db.md + agent harness -> voice, chat, canvas, forms, approvals, dashboards
```

The files hold the records, context, relationships, policies, and history.
The agent harness reads, writes, validates, repairs, migrates, plans, and
acts. The surface appears when needed: chat, voice, canvas, forms, approval
cards, dashboards, or whatever the task requires.

The old app becomes an agent operating over readable state.

The claim is not "no database." It is agent-operated files-as-database:
records are markdown files, fields are YAML frontmatter, relationships are
wiki-links, schemas and policies live in `DB.md`, indexes are `index.md` /
`index.jsonl`, the deterministic tool is `dbmd`, and the engine is the agent
driving it.

That is why db.md is not merely a markdown database. It is a persistence
layer for the class of software whose main substance is records, context,
relationships, decisions, workflows, and a surface.

Personal software is the easiest place to see the collapse first. The same
shape fits internal tools, company brains, lightweight CRMs, research
systems, support and agency workflows, contract registers, decision logs,
family tools, and agent-native products.

Many of those were previously too small, too specific, too fluid, or too
alive to justify becoming full SaaS products. The long
tail of software becomes possible because the app no longer has to be a
rigid database-backed product with a hand-built backend and frontend. It
can be readable state plus an agent that knows how to operate on it.

db.md replaces the layer where fluidity matters more than hard
transactional machinery. The records are the files, the schema is text, the
relationships are links, and the agent answers questions or builds the
surface the moment you ask for it. Add a field with a line of frontmatter.
Split a type by editing `DB.md`. Let the agent repair the store because it can
read the store. The database becomes fluid because the thing operating it
understands the medium.

[Karpathy's April 2026 LLM Wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f)
is the proof of life: a model can maintain a coherent markdown world.
db.md generalizes that from a wiki into a database. A company brain is one
obvious use case. So is personal software. So is home-cooked software. So
is the next agent-native product whose shape changes every week. None of those is the category. The category is
agent-native persistence: the database layer for software written,
operated, and reshaped by models.

Hard truth still exists. Payments, ledgers, high-concurrency shared state,
strict permission systems, sub-millisecond reads, billion-row analytics,
and regulated financial correctness still want hard engines. Postgres is
for authoritative machinery. db.md is for living context.

Files for meaning. Tools for authority. Agents for execution.

## The comparison field

A wave of products sells "memory" for agents, and a longer history of
databases sells structure. Each ships a system you adopt and maintain;
db.md ships a convention you own. To see the difference, ask four questions
of every option: who operates the live store, what sits between the agent
and the data, what it rides on as the world improves, and what has to stay
true for it to work. **db.md puts nothing between the agent and the data,
rides the model curve, and bets that models improve faster than schema
matters.** The alternatives either ride on machinery you maintain (which
only improves when someone does the work), bet that format standardization
solves the problem, or bake the data into model weights you cannot read.

Each row is an approach, tagged by **kind** and grouped by it, so you can
read down by paradigm or jump to a product you already know by name. But one column settles it: who operates the live store. Read it down
the page and only two rows answer "the agent": db.md, and the wiki demo it
generalizes. That is not a property db.md has more of; it is the premise
it is built on.

| Approach | Who operates the live store | Kind | What sits between the agent and the data | What it rides on | The bet / what has to stay true |
|---|---|---|---|---|---|
| **db.md** | the agent, on the files directly | Files, direct | nothing. The data is the files. The agent reads and edits them directly, and so can you | the model curve, directly. Every new model works the same files better, with no vendor migration and no proprietary index | agents improve faster than infrastructure. Two layers (sources + records) plus a meta-type field and schema repair work because models are smart enough. Files outlast all tools |
| Karpathy's LLM Wiki | the agent, on the files | Files, direct | nothing. Plain markdown the model reads directly | the model curve. Better models read files better, with no index to rebuild | models improve fast enough to read context directly. The foundational idea db.md builds on |
| Open Knowledge Format (OKF) | no one; it is an interchange format | Files, direct | nothing. The data is files; linking is standard markdown | format standardization, not model improvement. Works at any capability level | format simplicity is the bottleneck, not model capability. A shared spec enables exchange across orgs and systems. Google's spec; portability-first, not operational database work |
| GBrain | a retrieval pipeline you maintain | Retrieval stack | a Postgres + pgvector engine and reranker in front of the files | the model curve **and** a maintained graph+embedding stack | files alone aren't enough; the wikilink graph and the index are load-bearing |
| Mem0 | a retrieval service you call | Retrieval stack | a vector-and-graph service you call; your memories mediated by a retrieval stack | an extract → embed → retrieve pipeline you maintain | similarity retrieval over extracted facts beats reading files. Its [2026 migration](https://docs.mem0.ai/migration/oss-v2-to-v3) adds ADD-only extraction, hybrid search, and entity linking, moving toward dated, linked facts |
| Zep / Graphiti | a synced graph and its API | Retrieval stack | a hosted temporal knowledge graph and its API, a second structure kept in step with your data | a derived graph you keep synced | a temporal graph layer beats raw files, and the sync cost is worth it |
| Microsoft GraphRAG | an offline extraction pipeline | Retrieval stack | a knowledge graph an LLM extracts from your corpus, plus community summaries and an index, queried instead of the files | an offline extraction pipeline you re-run as the corpus changes, plus the model | an LLM-built graph answers better than reading the files, and the derived graph is worth extracting, storing, and rebuilding as data drifts |
| Letta / MemGPT | a memory-management runtime | Memory runtime | a runtime and retrieval layer around the context window (editable memory blocks plus archival retrieval) | a memory-management runtime you adopt | managing what's in context beats durable files. db.md keeps durable state as files instead |
| Cartridges / Engram | no one; it lives in the weights | Parametric | the model's own weights. The data is dissolved into parameters and a trained KV-cache; nobody reads or edits it, not you and not the agent | its own training research (cartridges, sparse memory fine-tuning, continual learning) plus two bets: reading whole context at query time stays costly enough to amortize into weights, and weights beat long-context recall | context is too lossy and too costly to read at query time, so knowledge must be baked into weights offline. Updating a fact means re-distillation; erasing one is training-side, not a file edit; provenance has no address |
| SQL / graph (Neo4j) | an app, via a query language | Structured DB | a schema you design up front, a query language (SQL or Cypher), and an app to drive it | your schema and the app layer. Better models write better queries, but the store's meaning lives outside the model | a relational or graph schema, designed up front, is stable enough. Apps mediate between the model and the data. Migrations are acceptable costs |
| Airtable, Notion | the vendor's app | SaaS | a vendor's service you rent, your data on their servers, exports that commonly lose live behavior, views, formulas, relations | the vendor's roadmap and release cycle. You get the AI they bolt on, when they ship it, inside their walls | outsourcing is cheaper than operating. Vendor roadmap aligns with your needs. Platform lock-in is acceptable |

The fight db.md picks most directly is with the retrieval stack. **db.md
computes, stores, and searches no vector, ever.** RAG engineers a retrieval
pipeline over embeddings of your data; db.md keeps the data as files and
lets the model read them, with semantic recall coming from the agent
widening its own search in plain language. An embedding does not naturally
tell you when a fact was true or whether something later replaced it; a
dated file can. The memory stack is already moving this way (Mem0's hybrid
search and entity linking), and GBrain and db.md are two readings of the
same Karpathy LLM Wiki pattern, one with an embedded Postgres, pgvector,
and hybrid search in front of the files and one without. db.md keeps it
the size Karpathy drew.

The trade is real, and worth naming: db.md spends inference where RAG
spends infrastructure. Recall costs agent tokens per query instead of an
embedding pipeline kept in sync, and the sidecar indexes exist to keep
that spend targeted: jump to the right records, don't read the store. The
price of inference is falling on the same curve the whole bet rides. The
price of maintaining a retrieval stack is not.

The opposite end of the field is parametric memory: bake the corpus into
model weights or a trained KV-cache, the bet behind Cartridges and Engram,
so the model answers from weights instead of reading files. It is the
mirror image of db.md. Where db.md keeps facts addressable, dated, and
editable on disk, parametric memory dissolves them into parameters: you
gain whole-corpus recall in one object and you lose provenance, freshness,
deletion, and the ability for the agent to fix a wrong fact by editing a
file. The amortization half of its bet is real; the recall half runs
against the model curve, and as long-context gets cheaper and sharper it
shrinks. db.md rides the same curve the other way. It owns no memory
machinery, reads whatever the best model can read, and gets better for
free. Parametric memory is at most a cache the field may someday compile
from a store like db.md; db.md's bet is that you never need the cache.

The memory layer was always a database with the data hidden. db.md is the
same job with the data left in the open. It composes with AGENTS.md for
instructions and MCP for tools. Different layers, not rivals.

## Why files

The application database has been a service for decades: a daemon, a wire
protocol, a migration tool, an admin panel. That made sense when useful
software over data had to be built around a database engine. It is no
longer the only shape.

db.md turns the shape inside out:

- **The database is the directory.** There is no daemon and no port. You
  can `cd` into it and `ls` your data.
- **Structured fields live in frontmatter.** They are typed, optional, and
  additive. You change the store shape by editing text, not by running a
  migration. Add a field, rename a type, tighten a schema in `DB.md`; the
  agent can read the diff and repair the records.
- **The index is derived.** A plain catalog plus embedded ripgrep is built
  to carry millions of files with no vector database. Want SQLite or a
  search index on top? Build one. The files stay the source of truth.

On portability: db.md is portable by default. It's just files. Git,
tarballs, sync services move them. A capable model reads them directly
without needing a format standard. That's the bet: as models improve, they
read the same files better. If you need to guarantee a third party can read
your knowledge without knowing db.md, OKF is a minimal exchange layer:
exporting costs schema and semantics and buys portability. db.md is the
operational store; OKF is the transport. Over time, model capability
outpaces format standardization, and the distinction fades.

## How far files go

Put a number on "millions." Using roughly 120 sent-plus-received work
emails a day, a person who indexes their mail adds about **44,000 files a
year**, around 440,000 in a decade; a heavy whole-career archive fits
around **1 to 1.5 million plain files**. **A ten-person shared store can
cross a million files in two to three years.** That is the scale this
format is built to hold. Shared the way a repo is shared: one curating
agent per store (the v0.3 contract), the team directing it and reading
freely; clones move through git, append-only logs merge by union, and
derived indexes regenerate rather than merge.

And the agent should not pay the whole-store cost in its normal loop,
because **the agent does not have to inspect every file; it reads
indexes.** Every type folder keeps a human `index.md` (the 500 most recent
entries) and a complete machine `index.jsonl`, both updated on writes. A
query reads the relevant sidecar and goes straight to the right record. The
interactive loop is designed around **O(changed), not O(store)**: what an
operation costs should track what changed, not how big the store has grown.

- **High-volume folders shard by date.** When the agent writes through
  `dbmd write`, source and event types are placed in the shard path
  automatically: an email lands in `sources/emails/2026/05/`, an expense in
  `records/expenses/2026/05/`. The agent supplies the type and date; `dbmd`
  does the folder math. No directory grows unbounded, and only the current
  shard is ever hot. Entity records and conclusions stay flat, because
  those sets are bounded by reality: you have only so many customers,
  however much mail they send.
- **The measured 10k tier is interactive; the million-file tier is an
  opt-in gate.** Sidecar reads are millisecond-scale at 10k, typed and
  full-text searches and working-set validation stay inside their
  documented budgets, and full sweeps run off-loop. Write paths are
  currently near, not under, the tight 100ms target because they compact a
  type-folder `index.jsonl`; that gap is documented in
  [tests/PERF.md](tests/PERF.md). The 1M test is opt-in and asserts the
  sidecar-backed loop and sweep targets when run.
- **Whole-store passes run off the loop.** A full `dbmd validate --all` or
  index rebuild is a linear repair and audit job you schedule. The agent
  never waits on one.

The first practical ceiling you hit is often git, not the format. Large
working trees need tuning (`fsmonitor`, `feature.manyFiles`, sparse or
partial checkout, or Scalar-style tooling) because git's index is still one
structure with an entry for every tracked path. Git is optional tooling
over db.md, not part of the format. At that point, version `records/`
(where the agent's curated data and conclusions live) and let high-volume
sources ride filesystem snapshots. The files keep working exactly the same.

What still wants a real engine: heavy write concurrency, ACID transactions,
sub-millisecond reads, aggregates over billions of rows. That territory is
the packed flavor on the [roadmap](SPEC.md#roadmap): an SQLite-class
engine projected under the same contract, with the files still the source
of truth. Until then the two compose cleanly.

## The bet

The claim is not that engines disappear. It is that the default flips: for
the broad middle of software, state starts as readable files under an
agent, and graduates to hard machinery only where authority demands it.

The standard is a small thing: two folders, frontmatter, wiki-links, one
config file, one small binary. Everything else is the model curve, and the
model curve is the part you get for free. The files outlast every tool that
ever touches them, including ours.
