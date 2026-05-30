# db.md — v0.1

`db.md` is **the open database, in plain files**. Records are markdown
files with YAML frontmatter. Relationships are wiki-links. The database
is the directory; the schema is the frontmatter; the index is whatever
you want to build on top. It is built for agents: a database a harness
reads, writes, links, and curates directly, and the native persistence
layer for the agent-native tools and apps built on it. An agent wants
files, not a schema to migrate or a query language to wrap around. db.md
is files. Simple and open by design.

One directory, three folders, one config file. Raw evidence lives in
`sources/`. Atomic typed data lives in `records/`. Curator-synthesized
narrative lives in `wiki/`. The store's identity, agent instructions,
policies, and custom schemas all live in a single `DB.md` file at the
root.

This document is the format spec. The reference toolkit (`dbmd` CLI) ships
in this same repo. Anyone can build a db.md-aware tool — the format is
open and intentionally simple.

---

## Status

**Spec version:** `v0.1` (this document is the tagged release).
**Stable:** the three-folder layout, the `DB.md` config file, and the
universal frontmatter contract are stable. The recognized-type
vocabulary is additive only.
**Tooling:** Apache-2.0 Rust `dbmd` CLI (one binary, subcommands for
read / write / validate / extract ops, zero LLM dependencies). The
agent runtime is BYO (Claude Code, Codex, or any harness).

## The shape

A **db.md store** is one directory. The canonical layout:

```
db/                          # any path; one db.md store per scope
├── DB.md                    # store identity + agent instructions + policies + schemas
├── index.md                 # curator-maintained catalog (the alternative to embedding RAG)
├── log.md                   # active chronological log (older months roll into log/)
├── log/                     # rotated log archives (log/2026-04.md, …): one timeline, paginated
├── sources/                 # raw evidence from outside (immutable; auto date-sharded at scale)
│   ├── emails/
│   │   └── 2026/05/         # high-volume folders shard by date — no unbounded directory
│   ├── transcripts/
│   ├── docs/
│   └── exports/
├── records/                 # atomic typed data; event types shard by date, entity types flat
│   ├── contacts/            # entity — flat (dedup-bounded)
│   ├── companies/           # entity — flat
│   ├── expenses/            # event — shards by date:
│   │   └── 2026/05/         # …like sources, because event records track volume
│   ├── meetings/            # event — shards by date
│   ├── decisions/           # flat (no primary date field)
│   └── invoices/            # event — shards by date
└── wiki/                    # curator-synthesized narrative with cross-links
    ├── people/
    ├── projects/
    ├── themes/
    ├── playbooks/
    └── synthesis/           # cross-cutting overview pages
```

**Required:** the `DB.md` file + at minimum one of `sources/` / `records/` / `wiki/` (most stores have all three). Sub-folders by type are convention; tools may use other groupings.

**Curator-maintained (optional, created on first curator action):** `index.md` (catalog of the store) and `log.md` (chronological action log). Absent at store creation; populated by the curator as it works. Each non-empty **type-folder** additionally carries an `index.jsonl` — the complete, machine-readable twin of its `index.md` (the `.md` is the capped human browse view; the `.jsonl` is the uncapped structured catalog that backs `dbmd fm query` / `dbmd index query` / dedup). See [The `index.md` and `log.md` files](#the-indexmd-and-logmd-files).

**Filename convention:** the config file is `DB.md` (uppercase), matching README / LICENSE / NOTICE conventions for "main file in a project root" and differentiating from the standard name `db.md` (lowercase, referring to the project / spec). `index.md` and `log.md` are lowercase — they're curator-maintained content, not config.

### Three folders, three data models

A db.md store composes three data models in one directory:

- **`sources/`** — **document store.** Raw artifacts from outside the
  operator's hand: emails, transcripts, exports, PDFs, scrapes.
  Preserved verbatim. Immutable. Frontmatter is metadata about the
  artifact (where it came from, when it arrived) — the body is the
  artifact itself. Because sources never change after ingest, the
  toolkit processes each one once and never re-parses it; high-volume
  source folders auto-shard by date (`sources/emails/2026/05/`) so no
  directory grows unbounded. This is the layer built to reach millions
  of files — see [Scale](#scale).

- **`records/`** — **relational-ish store.** Atomic typed data points:
  expenses, meetings, decisions, invoices, contacts, companies.
  Frontmatter-heavy (the structured "row"), body-light or empty (a
  short note when useful). Originated by the operator (via chat),
  by an agent extracting from sources, or by direct edit. Write-mostly,
  occasionally amended. "Relational but not that much, it's still
  markdown."

- **`wiki/`** — **graph store.** Curator-synthesized narrative with
  dense cross-references. Body-heavy markdown with wiki-links to
  records, sources, and other wiki pages. The "understanding" layer
  that emerges from atomic records and raw sources. Rewrite-and-grow.

The pattern: *sources are evidence; records are facts; wiki is
understanding.* Same store, three composed models.

### Where the agent reads what

1. **`dbmd spec`** prints the canonical spec (this document, bundled
   into the binary). The agent reads it once at startup to understand
   the format and the curator contract.
2. **`DB.md`** at the store root: identity (frontmatter) + per-store
   overrides (sections: agent instructions, policies, schemas). The
   agent reads it on every operation on this store.
3. The store itself: `sources/`, `records/`, `wiki/`. The agent
   operates here via `dbmd` subcommands.

## The universal frontmatter contract

Every markdown file in a db.md store carries YAML frontmatter with at
minimum:

```yaml
---
type: <type>          # required — what kind of thing this is
id: <id>              # optional; derived from path if absent
created: <RFC3339>    # required for content files; auto-set on create
updated: <RFC3339>    # required for content files; auto-maintained
summary: <one-line>   # required for content files; the catalog line
status: active        # optional; lifecycle state
tags: [tag1, tag2]    # optional; categorical labels
---
```

Type-specific fields layer on top — see [Recognized types](#recognized-types)
for the canonical vocabulary.

**Content files** = everything under `sources/`, `records/`, `wiki/`. **Meta files** = `DB.md`, `index.md`, `log.md` (these have their own contracts; they do not need `summary`, `created`, or `updated`).

**The `summary` field is canonical and required on every content file.** It is the **single source of truth** for what the file is about. Every hierarchical `index.md` reads this field directly to populate its catalog entries — no extraction rules, no recomputation. The agent writes a thoughtful summary when creating files (the curator's judgment), `dbmd fm init` writes a deterministic default if the agent doesn't (composed from other frontmatter fields per type), and the agent can always override via `dbmd fm set <file> summary='...'`.

**`summary` rules:**
- Required on every content file.
- One line. No newlines. Plain text (no markdown formatting beyond `[[wiki-links]]`).
- ≤ 200 characters (keeps indexes readable when many entries appear together).
- Captures the most important thing about the file at a glance. Not a copy of the body; not a copy of `name`/`title`. The summary is what an agent or a human reading an index needs to decide "is this the file I'm looking for?"

**Rules:**

- Frontmatter is **YAML**, not TOML or JSON. Wider ecosystem support
  (Obsidian, Hugo, every static-site generator), better human-edit
  ergonomics.
- The frontmatter block must be the very first thing in the file
  (no leading whitespace, no preceding markdown).
- The block is delimited by `---` on its own line at the start and
  end.
- The manager / curator auto-generates frontmatter on file create
  and maintains it on edit. **Operators don't write frontmatter by
  hand.**
- `type` is required and is the primary way tools query the store.
- `id` is optional; if absent, it's derived from the file path (e.g.
  `wiki/people/sarah-chen.md` → id `sarah-chen`).
- Timestamps are ISO-8601 (`2026-05-27T08:00:00-07:00`).
- Unknown fields pass through. Tools that don't recognize a field
  treat it as ambient context.

## Recognized types

The canonical vocabulary the spec ships at v0.1. **Custom types are
valid** — the manager / curator treats unknown types as ambient
context.

**Every content type (everything below except `db-md`, `index`, `log`) requires `summary` in frontmatter** — see the [universal frontmatter contract](#the-universal-frontmatter-contract). The "Type-specific fields" column lists fields *in addition to* the universal contract (`type`, `id`, `created`, `updated`, `summary`, `status`, `tags`).

| `type`         | Layer    | Default location         | Type-specific fields (in addition to the universal contract)          |
|----------------|----------|--------------------------|-----------------------------------------------------------------------|
| `db-md`        | root     | `DB.md` (the file)       | `scope`, `owner`, `computer_id` (if any). *Meta file: no `summary`.*  |
| `index`        | any      | `index.md` (root / per-layer / per-type-folder) | `scope: root\|layer\|type-folder`, `folder: <path>` (on layer + type-folder). *Meta file: no `summary`.* |
| `log`          | root     | `log.md` (single, global)| (none — body is the timeline). *Meta file: no `summary`.*             |
| `email`        | sources  | `sources/emails/`        | `from`, `to`, `date`, `subject`, `thread`, `in_reply_to`              |
| `transcript`   | sources  | `sources/transcripts/`   | `recorded_at`, `attendees`, `duration_min`, `language`                |
| `pdf-source`   | sources  | `sources/docs/`          | `received_from`, `received_at`, `doc_type`                            |
| `contact`      | records  | `records/contacts/`      | `name`, `email`, `company` (link → `records/companies/`), `role`, `first_touch`, `last_touch`|
| `company`      | records  | `records/companies/`     | `name`, `domain`, `industry`, `relationship`                          |
| `expense`      | records  | `records/expenses/`      | `date`, `amount`, `currency`, `category`, `vendor` (link → `records/companies/`), `contact` (link → `records/contacts/`)|
| `meeting`      | records  | `records/meetings/`      | `date`, `attendees`, `location`, `duration_min`, `expense` (link → `records/expenses/`)|
| `decision`     | records  | `records/decisions/`     | `decided_by`, `affects`, `alternatives_considered`                    |
| `invoice`      | records  | `records/invoices/`      | `date`, `amount`, `vendor` (link → `records/companies/`), `status`, `paid_at`|
| `wiki-page`    | wiki     | `wiki/<topic>/`          | `topic`, `derived_from` (list of record/source links)                 |

**Reading rules:**

- Custom types pass through. A reader that doesn't recognize `type:
  proposal` reads the file as ambient context.
- The folder layout is convention, not enforcement. A `type: contact`
  in `sources/foo/` is valid (though unusual).
- A single entity (e.g. a person) can have both a `records/contacts/`
  data row AND a `wiki/people/` narrative page. The record is the
  atomic fact; the wiki page is the synthesis that cross-references
  it.
- Custom types **also require `summary`** — the field is universal across
  content files regardless of whether the spec recognizes the type.
- **Canonical types carry implicit schemas.** Every field marked
  `(link → <prefix>/)` in the table above — `contact.company`,
  `expense.vendor`, `expense.contact`, `meeting.expense`, and
  `invoice.vendor` — is validated like any `DB.md ## Schemas` link
  field: a plain string where a wiki-link under the named prefix is
  expected is a `SCHEMA_LINK_PREFIX_MISMATCH` error. The implicit
  schema enforces **exactly** the fields the table marks `(link)` and
  no others. An explicit `### <type>` block in `DB.md ## Schemas`
  overrides the implicit schema for that type.
- **`wiki-page.derived_from` is deliberately not implicit-schema
  enforced.** Its links may point into *either* `records/` or
  `sources/`, so it has no single canonical prefix; it is left to
  ordinary wiki-link validation (broken targets still reported)
  rather than the single-prefix link check. To pin a prefix on it,
  declare an explicit `### wiki-page` schema in `DB.md ## Schemas`.

**Worked example — a `contact` record (note `summary` in frontmatter):**

```yaml
---
type: contact
id: sarah-chen
created: 2026-05-22T10:00:00-07:00
updated: 2026-05-22T10:00:00-07:00
summary: "Director of Ops at Northstar; renewal champion who drove the 175-seat expansion"
name: Sarah Chen
email: sarah@northstar.io
company: [[records/companies/northstar]]
role: Director of Operations
first_touch: 2025-09-14
last_touch: 2026-05-22
tags: [customer]
status: active
---

# Sarah Chen
...
```

The `summary` field is what `records/contacts/index.md` prints next to `[[records/contacts/sarah-chen]]`. It's the agent's judgment captured in data — not recomputed by tooling.

**Deterministic defaults per type** (what `dbmd fm init` writes when the agent doesn't override):

| `type`       | Default `summary` template                                 |
|--------------|------------------------------------------------------------|
| `contact`    | `<role> at <company-name> (last_touch: <date>)`            |
| `company`    | `<relationship>; <industry>`                               |
| `expense`    | `<date> — <amount> <currency> — <vendor>`                  |
| `meeting`    | `<date> — <first 3 attendees> (+N more)` when `N>0`        |
| `decision`   | `<decided_by>: <title-or-first-heading>`                   |
| `invoice`    | `<vendor> — <amount> — <status>`                           |
| `email`      | `<from> → <to> — <subject>`                                |
| `transcript` | `<recorded_at> — <attendees>`                              |
| `pdf-source` | `<doc_type> from <received_from>`                          |
| `wiki-page`  | `<topic>` or the file's first non-heading paragraph        |
| (custom)     | first non-heading paragraph, truncated to ≤200 chars       |

The agent can always overwrite the default with `dbmd fm set <file> summary='<better>'`. The tool generates a deterministic floor; the agent provides the ceiling.

## Linking

**Doctrine: wiki-links for everything inside the store. Standard
markdown links for everything outside.** No exceptions.

### Internal references → wiki-links

Any reference to another file in the same db.md store is a wiki-link
in double-bracket form. **Always a full store-relative path**, no
short forms (no `[[sarah-chen]]` shorthand — write
`[[records/contacts/sarah-chen]]`). The full-path requirement
eliminates ambiguity, makes the graph engine's job trivial, and
keeps agent-driven resolution deterministic.

```markdown
[[records/contacts/sarah-chen]]
[[wiki/people/sarah-chen|Sarah]]
[[sources/emails/2026-05-22-elena-renewal]]
```

- Target is a path relative to the store root, without the `.md`
  extension. A target with the `.md` suffix
  (e.g. `[[records/contacts/sarah-chen.md]]`) is accepted by the
  parser but `dbmd validate` warns (code `WIKI_LINK_HAS_EXTENSION`)
  and the canonical writers (`dbmd write`, `dbmd link`,
  `dbmd rename`) always emit the bare form.
- Optional `|display` segment overrides display text.
- Wiki-links appear in:
  - **Scalar frontmatter fields** that reference other files —
    inline form: `company: [[records/companies/northstar]]`.
  - **List-valued frontmatter fields** — YAML block-sequence form,
    one wiki-link per item:
    ```yaml
    attendees:
      - [[records/contacts/elena]]
      - [[records/contacts/sarah]]
    ```
    Block form is unambiguous; the inline flow form
    (`attendees: [[[a]], [[b]]]`) is rejected by `dbmd validate`
    because YAML parses it as a nested list rather than a list of
    wiki-link strings.
  - `summary` fields (encouraged for navigation)
  - Body text (curator's primary synthesis primitive)
  - `index.md` entries (every catalog line)
  - `log.md` entries (the `object` slot in `## [date] kind | <object>`
    is a wiki-link when the object is a file in the store)

### External references → standard markdown links

URLs, internet resources, and paths to files outside the store use
standard markdown link syntax:

```markdown
See [the Karpathy thread](https://x.com/karpathy/status/...).
Source PDF lives in [the shared drive](/Volumes/share/contracts/x.pdf).
[Acme's website](https://acme.io).
```

External link targets are not part of the graph, are not validated
against the store, and don't participate in `dbmd graph` queries.

### Why the split

- **Wiki-links express relationships in the store** — they're edges
  in the graph engine, they're what `dbmd rename` rewrites, they're
  what `backlinks` / `forwardlinks` / `orphans` operate on.
- **Markdown links express external references** — pointers to
  things outside the store's authority. They don't need rewriting on
  rename (nothing in the store moved); they don't need graph
  integrity checks (the targets aren't ours).

The agent (or `dbmd validate`) can tell at a glance which kind a
reference is: `[[...]]` vs. `[...](...)`.

### Collision detection

Wiki-links can collide in subtle ways. `dbmd validate` checks for
the canonical collision modes:

**Hard collisions (errors):**
- **ID collision** — two files in the store declare the same explicit
  `id` in frontmatter.
- **Short-form wiki-link** — a wiki-link target isn't a full
  store-relative path (e.g. `[[sarah-chen]]` instead of
  `[[records/contacts/sarah-chen]]`). The doctrine requires
  full paths.
- **Broken wiki-link** — target file doesn't exist.
- **Wiki-link target ambiguity** — defensive check; with full-path
  doctrine this should never trigger, but if a future short-form
  resolver is introduced and matches multiple files, it's a hard
  error.

**Soft collisions (warnings; type-aware entity-dedup signals):**
- Two `contact` records with the same `email`.
- Two `company` records with the same `domain`.
- Two `expense` records with the same `(date, amount, vendor)` tuple.
- Two `invoice` records with the same `(vendor, date, amount)`.
- Two `email` source files with the same `(from, subject, date)`
  (likely re-ingest).
- Two `meeting` records with the same `(date, attendees-set)`.

Soft collisions don't fail validation; they emit warnings the agent
reads (machine-parseable via `dbmd validate --json`) and decides
how to resolve — usually by `dbmd rename` to merge or `dbmd link` to
cross-reference. The toolkit detects; the agent decides.

Each collision maps to a structured issue code (`DUP_ID`,
`DUP_CONTACT_EMAIL`, `DUP_COMPANY_DOMAIN`, ...); see
[Validation](#validation) for the complete code vocabulary.

A reader that doesn't speak wiki-links treats them as text — no
breakage.

## The `DB.md` file

Every db.md store has a `DB.md` file at its root. Presence of `DB.md`
(uppercase) is the canonical signal that a folder is a db.md store.
The casing is deliberate: `DB.md` matches the README / LICENSE /
NOTICE convention for "main file in a project root" and visually
differentiates the file from the standard name `db.md`.

**If `DB.md` is absent**, the directory is not a db.md store. Every
store-walking `dbmd` subcommand (`validate`, `search`, `graph`,
`fm query`, `index rebuild`, `stats`, ...) exits non-zero with
structured error code `NOT_A_STORE` rather than guessing.
Initializing a fresh store is one composed command:
`mkdir -p mystore/{sources,records,wiki} && dbmd fm init mystore/DB.md`.

The file carries identity in frontmatter and optional per-store
overrides in sections. Required: frontmatter with `type: db-md`.
Optional: any of the standard sections below.

```markdown
---
type: db-md
scope: company           # company | personal | research | <custom>
owner: Sarah Chen
computer_id: acme-ops    # optional; an agentic computer's identifier
---

# Acme operations knowledge base

Company-scale institutional memory for Acme.

## Agent instructions

Prioritize creating `contact` records from new-sender emails. Use
British English in wiki pages. When a vendor invoice arrives, also
create an `expense` record linked to the invoice. Don't synthesize
wiki pages from sources tagged `transient`.

## Policies

### Frozen pages
- `records/decisions/2026-q1-strategy.md` — finalized, do not modify.
- `wiki/synthesis/2026-annual-plan.md` — signed-off plan.

### Ignored types
- `test`, `temp` — read but never synthesize.

## Schemas

### contact
- name (required)
- email (required, email)
- company (required, link to records/companies/)
- role (string)
- first_touch (date)
- last_touch (date)

### expense
- date (required, date)
- amount (required)
- currency (default USD)
- category (string)
- vendor (link to records/companies/)
- receipt (link to sources/)
```

**Canonical sections (all optional):**

- **`## Agent instructions`** — operator-authored override layer on
  top of the canonical curator contract (below). Free-form prose;
  the agent reads it on every store operation.
- **`## Policies`** — what the agent must/must-not do. Recognized
  sub-sections:
  - **`### Frozen pages`** — path list, never modified by the
    curator. `dbmd validate` reads this list; any write to a
    frozen path fails (the toolkit refuses; the agent doesn't have
    to remember).
  - **`### Ignored types`** — type list the curator never
    synthesizes (still readable as ambient context, but no
    derived wiki pages, no new records).
- **`## Schemas`** — custom type definitions or overrides of the
  canonical types. Parseable and enforced by `dbmd validate`.

  Each schema is a `### <type>` heading followed by one field per
  bulleted line in the form `- <field-name> (<modifiers>)`. One
  field per line; modifiers are comma-separated inside parens.
  Bullets without parens declare a free-form optional field of any
  shape.

  **Recognized modifiers:**
  - `required` — field must be present and non-empty.
  - Shape modifiers: `string`, `int`, `bool`, `date`, `email`,
    `currency`, `url`. Validate enforces the shape (date is
    RFC3339 / ISO-8601-date; email matches `<local>@<domain>`;
    etc.).
  - `link to <prefix>/` — value must be a wiki-link whose target
    path starts with `<prefix>/` (typically
    `records/<plural>/` or `sources/<plural>/`). Plain strings in
    a `link`-modified field are a hard error.
  - `default <value>` — value used when the field is absent (the
    composed default is also written by `dbmd fm init`).
  - `enum: <v1>, <v2>, ...` — value must be one of the listed
    options.

  Unknown modifiers are ignored (read as ambient context, no
  error). Custom schemas override the canonical recognized-type
  rules for the named `### <type>`; canonical types not named in
  `## Schemas` use spec defaults.

  `dbmd validate` emits structured `Issue`s (codes
  `SCHEMA_MISSING_REQUIRED`, `SCHEMA_SHAPE_MISMATCH`,
  `SCHEMA_LINK_PREFIX_MISMATCH`, `SCHEMA_ENUM_VIOLATION`) so the
  agent can read and remediate them via `--json`.

Absence of a section = use canonical defaults. The `DB.md` file is the
single point of configuration; there is no separate `rules/` folder.

## The `index.md` and `log.md` files

Two curator-maintained files at the store root. Both are markdown,
both are optional at store creation (the curator creates them on
first action), both are part of the canonical layout from then on.

### `index.md` — content-oriented catalog (hierarchical, opinionated)

The LLM-curated catalog. **The alternative to embedding-based RAG.**
Pattern originates in Karpathy's April 2026 LLM Wiki (single flat
index for ~hundreds of pages). db.md adopts the pattern at three
canonical levels — root, layer, type-folder — so the same model works
at every scale. The agent reads the closest index and drills up or
down; each level fits in an LLM context window.

**Three canonical levels. One `index.md` per non-empty folder at each
level. No opt-in, no thresholds, no flags — the structure is the
same everywhere.**

```
my-store/
├── index.md                  # ROOT — store-wide catalog (layers + type counts)
├── sources/
│   ├── index.md              # LAYER — every type folder under sources/
│   ├── emails/
│   │   ├── index.md          # TYPE-FOLDER — every file in sources/emails/
│   │   └── (.eml or .md files)
│   └── docs/
│       ├── index.md          # TYPE-FOLDER
│       └── (.pdf files)
├── records/
│   ├── index.md              # LAYER
│   ├── contacts/
│   │   ├── index.md          # TYPE-FOLDER — every contact record
│   │   └── (.md files)
│   ├── companies/
│   │   ├── index.md
│   │   └── ...
│   └── ...
└── wiki/
    ├── index.md              # LAYER
    ├── people/
    │   ├── index.md          # TYPE-FOLDER — every bio
    │   └── ...
    └── projects/
        └── ...
```

**The three levels:**

- **Root `index.md`** — exists whenever the store has any files. Lightweight: lists each layer + each type folder under it with counts. One entry per type folder; does NOT enumerate every file. Wiki-links target the layer indexes.
- **Layer `index.md`** (`sources/index.md`, `records/index.md`, `wiki/index.md`) — exists whenever that layer has any files. Lists each type folder under the layer with counts and brief summaries. Wiki-links target type-folder indexes.
- **Type-folder `index.md`** — exists whenever the type folder has any files. The **human / recency browse view**: lists files in the type-folder, **across date-shards**, with a one-line summary, **capped at 500 entries** selected by recency (newest first by the frontmatter `updated` field — clone-stable, unlike filesystem mtime, which `git clone` resets — ties broken by store-relative path ascending). Above the cap it lists the 500 most-recent and ends with a `## More` section pointing to `dbmd fm query` / `dbmd index query --type <t> --in <layer>` (the complete twin below) for full enumeration. The cap keeps the browse view inside an LLM context budget and write-through O(1), regardless of corpus growth — completeness lives in the `index.jsonl` twin, not here.
- **Type-folder `index.jsonl`** — the complete, **uncapped** machine twin of `index.md`: one JSON object per file in the folder (across date-shards), `{path, type, summary, tags, links, created, updated, <other frontmatter fields>}` — where **`tags` and `links` are the document's expansion** (`tags` = the LLM's flat semantic labels; `links` = wiki-links to concept pages + related records). Same kind of artifact as `index.md` — a derived, write-through, rebuildable **plain file** (JSONL, so appends are O(1), it stays git-diffable line-by-line, and it's ripgrep-able), not a database engine. It is the **backing for structured reads**: `dbmd fm query`, `dbmd index query`, `dbmd search --type/--where`, the dedup pre-write checks, and `dbmd graph backlinks` read it (one sequential, complete read per type-folder — cold-cache-proof) instead of scanning frontmatter across the tree. This is what makes the catalog complete *and* fast with no engine; ad-hoc full-text body search stays ripgrep. **Tags ≠ concepts:** a tag is a flat label (the agent filters/aggregates it on demand from this sidecar; no page of its own); a concept is a wiki page the doc links to (`links`), navigated via `graph backlinks`. Both are LLM-authored, never inferred — they are the *doc-side* of query expansion, so the agent's expanded query and the document's tags/concepts meet lexically here, with no embeddings. (Root and layer levels stay markdown-only rollups — the `.jsonl` twin lives at the type-folder level, where the records are.)

**Empty folders have no `index.md`.** Folders below the type-folder level (sub-sub-folders, if an operator creates them) are operator territory — not part of the canonical hierarchy, no auto-indexing.

**Example — root `index.md`:**

```markdown
---
type: index
scope: root
updated: 2026-05-27T10:00:00Z
---

# Knowledge base index

## Sources
- [[sources/emails/index|Emails]] (42 files) — vendor and customer correspondence
- [[sources/docs/index|Docs]] (18 files) — PDFs, contracts, exports

## Records
- [[records/contacts/index|Contacts]] (27 files) — people we've interacted with
- [[records/companies/index|Companies]] (12 files) — vendor and customer orgs
- [[records/meetings/index|Meetings]] (34 files)

## Wiki
- [[wiki/people/index|People]] (15 bios)
- [[wiki/projects/index|Projects]] (5)
- [[wiki/themes/index|Themes]] (3)
```

**Example — folder `index.md` (e.g. `wiki/people/index.md`):**

```markdown
---
type: index
scope: folder
folder: wiki/people
updated: 2026-05-27T10:00:00Z
---

# wiki/people

- [[wiki/people/sarah-chen]] — Renewal-champion bio; Q2 timeline
- [[wiki/people/elena-rodriguez]] — Acme VP; engineering relationship
- [[wiki/people/marcus-okafor]] — New Northstar contact (May 2026)
```

**Conventions:**
- Frontmatter: `type: index`, `updated: <RFC3339>`, `scope: root|layer|type-folder`, and `folder: <path>` on layer + type-folder indexes.
- **Each entry quotes the target file's `summary` field directly** — `- [[<path>]] — <frontmatter.summary>  ·  <#tags>`, where the optional compact `#tag` suffix comes from the file's `tags` (omitted when none). No extraction logic; no recomputation. The summary and tags live once, in the file's frontmatter, and are referenced from every index that lists the file. Root and layer entries include `(N)` file counts.
- Each level summarizes the level below it (root → layer → type-folder).
- **No opt-in.** Every non-empty type folder gets an `index.md`. The structure is uniform across stores at every scale.
- **Cap: 500 entries per type-folder `index.md`** (the browse view only — the `index.jsonl` twin is uncapped and complete). Selected by recency (newest first by the frontmatter `updated` field — clone-stable, not filesystem mtime), ties broken by store-relative path ascending (a total order, so write-through and rebuild never disagree on who is #500 vs #501), aggregating across date-shards. Overflow folders ship the 500 most-recent entries followed by a deterministic footer:

  ```markdown
  ## More

  This folder has 12,348 files. The 500 most recent are listed above.
  Use `dbmd index query --type email --in sources` for the complete catalog.
  ```
- **Indexes are maintained write-through, not rebuilt in the loop.** The write commands (`dbmd write` / `dbmd fm init` / `dbmd fm set` / `dbmd rename`) update the affected entries in place as the agent works — bounded work: splice the ≤500-entry `index.md`, append/upsert one line in `index.jsonl`, plus two parent counts. The catalog is always current; there is no rebuild step in the normal session. `dbmd index rebuild` is the from-scratch repair — after a bulk external drop into `sources/`, or to recover a damaged index — walking the store once, rewriting all three levels (both `index.md` and the complete `index.jsonl`, compacting the jsonl), deleting stale indexes. Never edited by hand.

### `log.md` — chronological action log

An append-only timeline of what the curator (or the operator)
did and when. The agent reads recent entries to know what's been
done lately, avoid duplicate work, and reconstruct the store's
evolution. Designed to be parseable with plain Unix tools.

```markdown
---
type: log
---

# Curator log

## [2026-05-27 10:00] ingest | sources/emails/2026-05-22-elena-renewal.eml
Email received from Elena re: renewal expansion to 175 seats.

## [2026-05-27 10:05] create | records/meetings/2026-05-22-renewal-call
From email thread; attendees: Elena, Sarah, the CTO.

## [2026-05-27 10:10] update | records/companies/northstar
Seat count 120 → 175 (pending signature).

## [2026-05-27 10:15] update | wiki/people/elena-rodriguez
Added Q2 renewal context. Linked records/meetings/2026-05-22-renewal-call.

## [2026-05-27 10:20] validate
PASS — 0 errors, 2 warnings (unknown type `proposal` in records/proposals/x.md; orphan wiki/themes/draft.md).
```

**Conventions:**
- Entry header: `## [YYYY-MM-DD HH:MM] <kind> | <object>` (object optional for store-wide actions like `validate`).
- Recognized kinds: `ingest`, `create`, `update`, `delete`, `rename`, `link`, `validate`, `index-rebuild`, `contradiction`. Custom kinds are valid; `dbmd validate` warns on unrecognized kinds without failing.
- Body (one or more lines) explains what happened.
- Append-only. The curator never rewrites past entries; if a finding is wrong, append a corrective entry below it.
- Parseable with `grep "^## \[" log.md | tail -5` or any similar pipeline (or `dbmd log tail`).
- **Rotation.** `log.md` is the active timeline; `dbmd log` automatically rolls older months into `log/<YYYY-MM>.md` on append. The full history is the archives plus the active file — one timeline, paginated so the active file (and every read of it) stays small no matter how old the store gets. `dbmd log tail` / `dbmd log since` reverse-read from the active file and cross into archives only when the requested range does.
- **Concurrent-clone merges.** A single-writer store (one agent, one clone — the v0.1 contract; see [Writers and readers](#writers-and-readers)) never has a merge. When two git clones of a store both append (multi-machine sync, a shared repo), git's line merge conflicts on the shared end-of-file region. Resolution is the agent's: a curator with this SPEC in context semantically merges — keep both entries, order by timestamp. For merges where no agent is in the loop (a human, CI), set `log.md merge=union` in `.gitattributes`: because every entry is timestamped, the union driver keeps both sides (never drops one) and a later agent pass reorders. The derived `index.md` needs no merge logic at all — on conflict, regenerate it with `dbmd index rebuild`.

## The curator contract

The "curator" is a **role**, not a binary. Any agent (Claude Code,
Codex, a custom harness) operating a db.md store plays the curator
role. The spec defines the behavior contract; the agent runtime is the
user's choice. **db.md ships no LLM runtime and no API keys.**

**The agent acting as curator:**

1. **Knows the SPEC** (this document, loaded into the harness's
   system prompt via `dbmd spec` at bootstrap — see
   [Tooling](#tooling)). The SPEC is the canonical behavior
   contract; the agent doesn't re-read it per session.
2. **Reads the store's `DB.md`** on every session — frontmatter for
   identity; `## Agent instructions` / `## Policies` / `## Schemas`
   sections for per-store overrides.
3. **Warms up via `dbmd log tail 20`** (or `dbmd log since
   <last-session-time>` for a precise diff) to know what was done
   lately and avoid duplicate work.
4. **Detects new state when invoked.** The agent doesn't run a
   watcher. The harness wakes the agent on schedule (cron,
   systemd-timer) or on an external trigger (the operator's
   message, a webhook, a file-event script). On wake, the agent
   uses `dbmd log since <ts>` and `dbmd search --updated-after
   <ts>` to learn what's new.
5. **Extracts atomic facts from new sources into `records/`** (e.g.
   an email becomes a `meeting` record + a `contact` record).
   **Every created content file gets a `summary` in its
   frontmatter** — thoughtful summary if the agent has context for
   one; `dbmd fm init` writes a deterministic default otherwise.
6. **Creates or updates `wiki/` pages** reflecting entities,
   projects, and themes — synthesizing across records and sources,
   with dense wiki-links. **Same summary contract: every wiki page
   has `summary` in frontmatter.**
7. **Refreshes `summary` whenever the content meaningfully changes**
   — e.g. if a contact's role changes, the agent updates both the
   `role` field and the `summary` field. Stale summaries are an
   anti-pattern; `dbmd validate` warns on suspiciously old summaries
   relative to the file's body.
8. **Maintains cross-references** (a wiki page about a person links
   to the contact record, the company record, and meeting records).
9. **Flags contradictions** (two sources disagree on a contact's
   employer) without silently picking a winner. Canonical
   mechanism: append a `## Open questions` section to the relevant
   wiki page with both candidate facts cited via wiki-links to the
   conflicting sources, then `dbmd log contradiction <object> -m
   "<short description>"`. Surface the disagreement; let the
   operator (or a later session with more evidence) resolve.
10. **Relies on write-through indexes.** The write commands
    (`dbmd write` / `dbmd fm init` / `dbmd fm set` /
    `dbmd rename`) keep the hierarchical `index.md` catalog (root,
    layer, type-folder) current as the agent works — there is no
    rebuild step in the normal loop. After a bulk external drop
    into `sources/` (rsync, mbsync), the agent runs `dbmd index
    rebuild` once to fold the new files in. See [Scale](#scale).
11. **Appends to `log.md`** on every action — ingest, create,
    update, delete, rename, link, validate, contradiction
    (`dbmd log <kind> <object> -m <note>` is the canonical
    append).
12. **Respects `## Policies` in `DB.md`** — the toolkit refuses
    writes to `### Frozen pages`, so the agent doesn't have to
    remember the list; the agent's part is knowing the policy
    exists and choosing alternate paths or escalating to the
    operator when blocked. `### Ignored types` are never
    synthesized into derived wiki pages.

**The agent does not (in its curator role):**

- Delete files in `sources/`. Sources are evidence; the operator
  deletes them explicitly.
- Edit `DB.md`. That's operator-owned.
- Rewrite past `log.md` entries. The log is append-only; corrections
  go on the end.
- Use any tool other than `dbmd` for store operations. All file/data
  ops go through `dbmd` subcommands; the harness can do anything else
  it wants outside the store.

### Pre-write checks

Before `dbmd write`, `dbmd link`, or `dbmd fm set`, the agent
should:

1. **Search for existing entities** to avoid soft collisions —
   `dbmd fm query email=<addr> --in records/contacts` before creating
   a `contact`; `dbmd fm query domain=<host> --in records/companies`
   before creating a `company` (each path-scoped to the entity's flat
   type-folder ⇒ O(entities), not O(store)); the
   collision-detection vocabulary in
   [Linking → Collision detection](#collision-detection) is the
   canonical list.
2. **Use full wiki-link paths** for every internal reference —
   `[[records/contacts/sarah-chen]]`, never `[[sarah-chen]]`.
   Short-form fails `dbmd validate`.
3. **Confirm wiki-link targets exist** before writing them.
   Broken targets fail `dbmd validate`.
4. **Set a thoughtful `summary`** when creating a file; **refresh
   it** when the body changes meaningfully.
5. **Tag with the existing vocabulary.** Before adding `tags`, glance at
   the type-folder catalog (`index.md` / `index.jsonl`) and reuse labels
   already in use — mint a new tag only for a genuinely new concept, so
   the vocabulary stays coherent. The catalog you're already reading is
   your memory of your own labels; there's no separate tag index to
   consult. For a concept that deserves explanation, create or link a
   `wiki/` page rather than a tag — tags are flat labels, concepts are
   pages.

### Post-write checks

After a meaningful batch of writes (a session, a sweep, a recovery
pass):

1. **`dbmd validate`** — validates the working set (the files
   touched this session plus anything linking to them); surfaces
   missed pre-write checks (broken links, missing summaries, schema
   violations from `DB.md`'s `## Schemas`). `dbmd validate --all`
   is the full-store sweep — CI or recovery, not the loop.
2. **`dbmd log <kind> <object>`** — append a chronological entry
   for the action (every meaningful write).

Indexes need no explicit step — the write commands maintain them
write-through (see [Scale](#scale)).

## The agent session

Every session against a db.md store follows the same shape. The
toolkit doesn't enforce it; the contract lives here.

1. **Open** — the harness already has `dbmd spec` in its system
   prompt from bootstrap (see [Tooling](#tooling)); if it doesn't,
   `dbmd spec` and load it now. Read the store's `DB.md` for
   identity, agent instructions, policies, and schemas.
2. **Warm up** — `dbmd log tail 20` to learn what was done lately;
   `dbmd log since <last-session-time>` for a precise diff.
3. **Operate** — read with `dbmd search` / `dbmd fm query` /
   `dbmd graph` / `dbmd extract`; write with `dbmd write` /
   `dbmd fm set` / `dbmd link` / `dbmd rename`. Apply
   [pre-write checks](#pre-write-checks) before every write.
   **When searching, expand the query into its related terms and
   synonyms and run them together** —
   `dbmd search "(revenue|sales|income|ARR)"` — you are the semantic
   layer; db.md has no embeddings.
   **Append `dbmd log <kind> <object> -m <note>` for every
   meaningful action.**
4. **Validate** — `dbmd validate` after any non-trivial change
   validates the working set (fast, O(changed)); `dbmd validate
   --all` is the periodic full sweep. Hard issues block; soft
   warnings are decision points the agent resolves with `dbmd
   rename` / `dbmd link` / `dbmd write`.
5. **Catalog stays current automatically** — the write commands
   maintain `index.md` write-through, so there is no rebuild step
   in the loop. Run `dbmd index rebuild` only after a bulk external
   drop into `sources/`.
6. **Close** — a final `dbmd log` entry capturing what the session
   accomplished, when natural.

The discipline matters because the next session begins by reading
the log. A skipped step 3 log entry is a step the next session
can't see.

## Validation

`dbmd validate` is the single validation entrypoint (there is no
separate `dbmd lint`). It walks the store and emits a list of
structured **issues**. With `--json`, each issue is a machine-
parseable object the agent branches on:

```json
{
  "severity": "error",
  "code": "WIKI_LINK_SHORT_FORM",
  "file": "wiki/people/sarah-chen.md",
  "line": 12,
  "key": null,
  "message": "wiki-link '[[sarah-chen]]' is not a full store-relative path",
  "suggestion": "replace with [[records/contacts/sarah-chen]]",
  "related": []
}
```

`severity` is `error` | `warning` | `info`. **Any `error` fails
validation** (non-zero exit); warnings and info don't. `suggestion`
is a deterministic remediation hint — the agent applies it without
guessing. `related` lists other files involved (e.g. the duplicate
partner in a collision).

**Scope.** `dbmd validate` validates the **working set** by default —
content files changed since the last `validate` entry in `log.md` (or
since `--since <ts>`), plus any file linking to a changed, renamed, or
removed path. This keeps the post-write check O(changed), flat in
store size. `dbmd validate --all` walks the entire store — every link,
every index, and the entity-dedup collisions (`DUP_*`), which the
working-set pass leaves to the pre-write checks and to `--all`. Both
modes emit the same issue vocabulary below.

**Canonical issue codes** (the complete vocabulary the agent will
see; grouped by category):

| Code | Severity | Meaning / remediation |
|------|----------|-----------------------|
| `NOT_A_STORE` | error | path has no `DB.md`; not a db.md store |
| `DB_MD_BAD_TYPE` | error | the store's `DB.md` is not `type: db-md` |
| `DB_MD_MISSING_FIELD` | error | the store's `DB.md` frontmatter lacks `scope` or `owner` |
| `DB_MD_UNKNOWN_SECTION` | warning | `DB.md` has an `##` section other than `Agent instructions` / `Policies` / `Schemas` |
| `FM_MISSING_TYPE` | error | content file has no `type:` |
| `FM_MALFORMED_YAML` | error | frontmatter block isn't valid YAML |
| `FM_BAD_TIMESTAMP` | error | `created` / `updated` / date field isn't ISO-8601 |
| `LAYER_TYPE_MISMATCH` | warning | a recognized `type:` sits in a layer other than its canonical one (e.g. `contact` under `sources/`) |
| `SUMMARY_MISSING` | error | content file has no `summary` — run `dbmd fm init` |
| `SUMMARY_EMPTY` | error | `summary` present but empty |
| `SUMMARY_MULTILINE` | error | `summary` contains newlines |
| `SUMMARY_TOO_LONG` | warning | `summary` > 200 chars |
| `WIKI_LINK_SHORT_FORM` | error | target isn't a full store-relative path |
| `WIKI_LINK_BROKEN` | error | target file doesn't exist |
| `WIKI_LINK_AMBIGUOUS` | error | target matches multiple files (defensive) |
| `WIKI_LINK_HAS_EXTENSION` | warning | target carries `.md` — drop it |
| `WIKI_LINK_FLOW_FORM_LIST` | error | frontmatter list uses `[[[a]], [[b]]]` — use block form |
| `DUP_ID` | error | two files declare the same `id` |
| `DUP_CONTACT_EMAIL` | warning | two `contact`s share `email` |
| `DUP_COMPANY_DOMAIN` | warning | two `company`s share `domain` |
| `DUP_EXPENSE_TUPLE` | warning | two `expense`s share `(date, amount, vendor)` |
| `DUP_INVOICE_TUPLE` | warning | two `invoice`s share `(vendor, date, amount)` |
| `DUP_EMAIL_REINGEST` | warning | two `email`s share `(from, subject, date)` |
| `DUP_MEETING_TUPLE` | warning | two `meeting`s share `(date, attendees-set)` |
| `SCHEMA_MISSING_REQUIRED` | error | `DB.md` schema requires a field that's absent |
| `SCHEMA_SHAPE_MISMATCH` | error | value doesn't match the schema's shape modifier |
| `SCHEMA_LINK_PREFIX_MISMATCH` | error | `link to <prefix>/` field has a plain or wrong-prefix value |
| `SCHEMA_ENUM_VIOLATION` | error | value not in the schema's `enum` |
| `POLICY_FROZEN_PAGE` | error | write attempted on a `### Frozen pages` path (write-time) |
| `POLICY_IGNORED_TYPE_PRESENT` | info | a file with an `### Ignored types` type exists |
| `POLICY_IGNORED_TYPE_DERIVED` | warning | a `wiki-page` derives from an ignored-type record |
| `LOG_BAD_TIMESTAMP` | error | `log.md` entry header timestamp unparseable |
| `LOG_UNKNOWN_KIND` | warning | `log.md` entry kind not recognized |
| `LOG_OUT_OF_ORDER` | warning | `log.md` entries not in non-decreasing time order (possible rewrite) |
| `INDEX_MISSING` | error | a non-empty canonical folder lacks `index.md` — run `dbmd index rebuild` |
| `INDEX_STALE_ENTRY` | error | an `index.md` lists a file that no longer exists |
| `INDEX_MISSING_ENTRY` | error | a file isn't listed in its folder's `index.md` |
| `INDEX_ORPHAN` | warning | an `index.md` sits in an empty / non-canonical folder |
| `INDEX_WRONG_SCOPE` | warning | index `scope:` doesn't match filesystem location |
| `INDEX_SUMMARY_MISMATCH` | error | an index entry's text doesn't match the file's `summary` |
| `INDEX_JSONL_MISSING` | error | a type-folder's `index.jsonl` twin is missing — run `dbmd index rebuild` |
| `INDEX_JSONL_DESYNC` | error | a file isn't in the `index.jsonl`, or a jsonl record points at a missing file |
| `INDEX_JSONL_STALE` | error | an `index.jsonl` record's fields don't match the file's frontmatter |
| `TAGS_MALFORMED` | warning | `tags` isn't a flat YAML list of short scalar labels |

The vocabulary is additive across spec versions (new codes layer on;
existing codes keep their meaning). Errors block; the agent resolves
warnings and info at its discretion — usually via `dbmd rename`,
`dbmd link`, `dbmd fm set`, or `dbmd index rebuild`.

**`DB.md` structure.** The store's `DB.md` is the identity file, so its
shape is checked directly (not as a content file — it carries no
`summary`). Its frontmatter MUST declare `type: db-md`
(`DB_MD_BAD_TYPE` otherwise, including when `type:` is absent or
malformed) and MUST carry both `scope` and `owner`
(`DB_MD_MISSING_FIELD`, one issue per absent field). Its body MAY
contain only the three recognized `##` sections — `Agent instructions`,
`Policies`, `Schemas`; any other `##` heading is a likely typo or
misplacement and surfaces as `DB_MD_UNKNOWN_SECTION` (warning — the
parser ignores it, so it does not corrupt the config, but it signals
the operator wrote a section the toolkit will never read). Recognized
`###` sub-headings inside `Policies` / `Schemas` (e.g. `Frozen pages`,
`Ignored types`, a `### <type>` schema block) are not flagged.

**Layer-appropriate types.** The recognized-type table assigns each
canonical content type a home layer (`email`/`transcript`/`pdf-source`
→ `sources/`; `contact`/`company`/`expense`/`meeting`/`decision`/
`invoice` → `records/`; `wiki-page` → `wiki/`). A file whose recognized
`type:` sits in a *different* layer (a `type: contact` under
`sources/`, a `type: email` under `wiki/`) gets `LAYER_TYPE_MISMATCH`
(warning, not error): the folder layout is convention, not enforcement,
so the placement is valid-but-unusual and worth a curator's eye, not a
hard block. Custom / unrecognized types carry no layer expectation and
are never flagged; meta types (`db-md`, `index`, `log`) have their own
contracts and are exempt.

## Why files

The database has been a service for decades — a daemon, a wire
protocol, a schema migration tool, an admin UI. That made sense when
storage was expensive and indexes had to live in RAM. It doesn't
anymore. A modern computer can ripgrep a million files in seconds.
An LLM reads markdown directly. Git already does what database
snapshots try to.

db.md inverts the shape:

- **The database is the directory.** No daemon, no port, no
  migration tool.
- **The schema is the frontmatter.** Type-tagged, additive, optional.
- **The index is derived.** db.md ships its own — the hierarchical
  `index.md` catalog plus embedded ripgrep — and it carries the store
  to millions of files with no vector database. Want a SQLite catalog
  or a tantivy index for some other tool? Build it on top; the files
  stay the source of truth and any derived index is rebuildable.

Three properties files have that tables don't:

1. **Human-editable.** A record is a file. Open it, edit it,
   commit it. No migration, no admin UI, no ORM.
2. **Version-controllable.** Git is the audit log. Every change is
   reversible, branchable, diffable.
3. **LLM-native.** The format an LLM reads best is the format a
   human reads best.

Most databases are not Google-scale. They are a set of records with a
form or a dashboard on top: a CRM, a knowledge base, an ops tracker, a
contract register, the internal tools a company builds and rebuilds,
the SaaS apps that are a database with a UI bolted on. db.md replaces
the database for that whole class — and the app over it, because the
agent reads and relates the records directly and builds the view on
demand. The genuinely hard remainder is real: high write concurrency,
ACID transactions, sub-millisecond reads, aggregates over billions of
rows. A real engine still earns its place there today, and that is
where the [roadmap](#roadmap) takes db.md next — the packed engine
(SQLite-class, projected through a VFS) under this same contract: the
directory is the database, the files are the source of truth. Until
then the two compose cleanly — write to both, treat db.md as the
canonical, human-readable layer. The direction is one way: eventually,
all of them, and never by adding vectors.

## Writers and readers

By design, db.md is **many-writer for `sources/` and `records/`,
single-writer for `wiki/`**. Anything can drop files into `sources/`
(rsync, mbsync, manual cp). Anything can append atomic facts to
`records/` (the agent, the operator via `dbmd write`, scripts).
But `wiki/` — the synthesis layer — has a single voice. One curator
agent reconciles it. Multiple agents writing to `wiki/`
concurrently is an anti-pattern.

Files dropped into `sources/` by an external tool join the catalog
when the agent next seeds them with `dbmd fm init` (write-through) or
folds the whole drop in with one `dbmd index rebuild`. Until then they
are on disk and findable by `dbmd search` (ripgrep doesn't need the
catalog), but not yet listed in `index.md`. The agent reconciles a
bulk drop once, not file-by-file in the loop.

**Single-agent-per-store is the v0.1 contract.** db.md does not
coordinate multiple curator agents writing to the same store
concurrently. The operator runs one curator at a time. If multiple
agents need to operate, give each its own store (and link the
stores externally) or serialize via the operator's own tooling.
Multi-agent coordination — locks, leases, conflict resolution —
is out of scope at v0.1.

## Scale

db.md scales to **millions of files natively** — no embeddings index,
no vector store, no external catalog required. The store *is* the
database; the filesystem, embedded ripgrep, and a write-through
catalog are the engine. One rule makes this hold:

**The interactive loop is O(changed), never O(store).** Every
operation the agent runs in its write loop — search, frontmatter
lookup, backlinks, the pre-write dedup checks, the per-write catalog
update, the post-write validate — costs in proportion to what changed,
not to how large the store is. Whole-store passes exist (`dbmd
validate --all`, a full `dbmd index rebuild`, `dbmd stats`) but they
are repair/audit operations, off the interactive path.

Four properties deliver it:

- **Sources and event-type records are date-sharded; entity records
  and wiki stay flat.** Raw evidence never changes after ingest, so the
  toolkit parses each source once and never again. High-volume folders
  auto-partition by date (`sources/emails/2026/05/…`,
  `records/expenses/2026/05/…`) so no directory holds an unbounded
  number of entries and only the current shard is ever "hot."
  **Sharding is a property of the type, not the layer:** event-driven
  types (`email`, `transcript`, `expense`, `invoice`, `meeting`, +
  custom event types) carry a primary date field and shard;
  dedup-bounded *entity* types (`contact`, `company`) stay flat because
  the entity set itself is bounded; `wiki/` stays flat
  (curation-bounded). This is what lets a company's event records —
  expenses, invoices, orders, which track business volume, not curation
  effort — scale the same way sources do. The type-folder catalog
  (`records/expenses/index.md`) aggregates across shards; the shards
  themselves are storage, not catalog levels.
- **Structured reads hit the `index.jsonl` sidecar; full-text reads
  are ripgrep.** `dbmd fm query`, `dbmd index query`, `dbmd search
  --type/--where`, the entity-dedup pre-write checks, and `dbmd graph
  backlinks` read the relevant type-folder `index.jsonl` — one
  sequential, complete read, cold-cache-proof (it replaces scanning
  frontmatter across the tree). Ad-hoc full-text body search is
  embedded ripgrep over bodies; link existence is `stat`. Never a
  full-store parse.
- **The catalog is maintained write-through — two artifacts per
  type-folder.** `dbmd write` / `dbmd fm init` / `dbmd fm set` /
  `dbmd rename` update both the human `index.md` (capped 500, recency
  browse — splice the ≤500-line file) and the machine `index.jsonl`
  (uncapped, complete, structured — O(1) append/upsert) in place,
  plus two parent counts. The catalog is always current; `dbmd index
  rebuild` is a from-scratch repair (compacting the jsonl), not a
  per-change step. Both are plain files — derived, rebuildable, no
  engine.
- **The log rotates.** `log.md` is the active timeline; older months
  roll into `log/<YYYY-MM>.md`. `dbmd log tail` / `since` reverse-read
  from the end. The active log stays small regardless of store age.

**Performance budgets** (modern laptop). Loop ops are flat in store
size; sweep ops are linear and run off the loop:

| Operation                              | Class           | 10k    | ~1M    |
|----------------------------------------|-----------------|--------|--------|
| `dbmd write` / `fm set` (+ catalog)    | loop            | <100ms | <100ms |
| `dbmd fm query <k>=<v>`                | loop (ripgrep)  | <300ms | <2s    |
| `dbmd search <query>`                  | loop (ripgrep)  | <300ms | <2s    |
| `dbmd graph backlinks <path>`          | loop (ripgrep)  | <200ms | <2s    |
| `dbmd log tail 20`                     | loop (rev-read) | <50ms  | <50ms  |
| `dbmd validate` (working set)          | loop            | <1s    | <2s    |
| `dbmd validate --all`                  | sweep           | <5s    | <60s   |
| `dbmd index rebuild` (full)            | sweep           | <10s   | <90s   |
| `dbmd stats`                           | sweep           | <5s    | <60s   |

Budgets are targets, not contractual SLAs. They pin the
implementation to the O(changed) discipline — atomic file writes,
embedded ripgrep, write-through catalog, reverse-read log — so the
agent can call `dbmd` after every write without compounding latency
into seconds.

**How much is "company scale"?** A single user indexing their entire
Gmail runs ~120 emails/day — roughly 44k files a year, ~440k over a
decade, ~1–1.5M across a heavy career: comfortably inside the native
sweet spot. A company is the larger object — even ten people cross a
million files within a few years, and a large org reaches hundreds of
millions to billions. The separated, file-per-record flavor with
ripgrep carries the individual and the small team; the packed flavor
and the engine (see [Roadmap](#roadmap)) carry the company.

**The flagship worked example is `db/`** — db.md's own knowledge kept
as a db.md store, co-located with the toolkit's source. How do you run
db.md at company scale? Read the store of how db.md itself was built:
the research grounding the design under `sources/`, every material
build decision under `records/decisions/`, and the narrative synthesis
(the scale story, the sizing model, the roadmap) under `wiki/`. It is
operated by `dbmd` as the toolkit grows — the same shape an agentic
computer ships with its store at `~/db/`.

**Two ceilings, not one.** The filesystem + ripgrep store reaches
millions, but **git over the raw store is the tighter limit**
(comfortable to ~100k files, tuning by ~500k, special tooling past
~1M). So git-as-audit-log is the individual / small-team property;
company-scale history is the packed flavor plus external snapshots — an
agentic computer on a managed VM has hourly / daily snapshots as
its real audit log. Sharding fixes per-directory growth, not the
whole-tree-walk cost that git and backups pay; the maintained engine
index is what removes that.

**Semantic recall without embeddings.** Lexical search looks like it
misses synonyms — but the agent driving `dbmd` is a language model, so
*it* supplies them: it expands a concept into its related terms
(`revenue → revenue, sales, income, ARR, top-line`) and runs them as one
search. `dbmd` stays a dumb lexical tool and computes nothing; the model
is the semantic layer — and a frontier model is a *richer* semantic model
than any embedding index. This is the whole semantic story: no vectors to
compute or store, now or ever, and nothing needed beyond the v0.1 toolkit.
(A maintained keyword index makes this a sublinear fast path at scale —
see the [Roadmap](#roadmap) — but it is a *lexical* index, never a vector
index; db.md adds no embeddings and no ANN.)

The files remain the source of truth. You *can* derive anything you
like on top — a SQLite catalog, a tantivy index, embeddings for some
other tool — but you do not *need* to: the native toolkit is the query
layer, at company scale and beyond.

## Roadmap

v0.1 is deliberately the simplest thing that already works at company
scale: plain files, YAML frontmatter, wiki-links, embedded ripgrep.
No daemon, no engine, no magic — and it carries a store to the low
millions of files. That is the floor, not the ceiling.

Where db.md is going — additively, without breaking the "it's just
files" contract or the format you read today:

- **An agent-native on-disk representation — in two flavors.** The
  same logical format and the same contract, two physical encodings a
  store can take:
  - **Separated** — plain markdown files on disk (Obsidian-compatible,
    git-diffable, maximal interop) plus an adjacent index sidecar
    holding the compiled view: typed frontmatter, wiki-link edges,
    content hash, summary. **v0.1 already ships the nascent form — the
    per-type-folder `index.jsonl`;** the roadmap deepens it (body
    keywords, richer fields) and makes its reads sublinear. The files
    are literally the source of truth; the sidecar makes reads
    sequential/O(1) and is rebuildable from them.
  - **Packed** — records, index, and links stored together in
    a database container: a SQLite file (FTS5 for full-text, B-tree for
    frontmatter — all lexical, no vector extension) or a small set of
    files. One portable store, no
    millions-of-inodes, atomic transactions, sublinear everything; the
    directory is projected from the container via the VFS.

  `dbmd` converts between the flavors losslessly (explode a container
  to a directory, pack a directory into a container) — no lock-in. A
  record is always materializable as a plain markdown file; that is
  what "files are the source of truth" means across both.
- **A virtual filesystem.** db.md mounts as an ordinary directory —
  every tool that reads files still works, and this is how the
  **packed** flavor presents as a directory — while the backing engine
  serves queries from real index structures (B-tree / LSM / inverted
  full-text index — all lexical, no vectors), not linear scans. `fm query`, `search`, and `backlinks` become
  sublinear; a store scales past the point where a literal directory
  of millions of files (or git over it) would fall down.
- **Faster lexical search — never embeddings.** *Today, model-free:* the
  agent expands a query into related terms and runs them lexically (see
  [Scale](#scale)) — the agent is the semantic layer, richer than any
  embedding model. *Next:* a maintained keyword index (an inverted index
  over `summary` + agent-supplied keywords, uncapped and contiguous) that
  a query hits first, turning the cold whole-tree scan into one sequential
  read. That is the whole semantic roadmap: `dbmd` never computes, stores,
  or searches a vector — no ANN, no embedding index, ever.
- **Continuous integrity and concurrency.** Incremental validation
  from a change journal (integrity always current, no full sweep) and
  real transactions so multiple curator agents can operate one store
  at once.
- **History that scales.** Snapshotting and audit that hold at
  millions-to-billions of records, with a git-compatible projection
  for the subset that wants it.

The contract stays: the database is the directory, the schema is the
frontmatter, files are the source of truth. The engine underneath gets
faster, smarter, and bigger — the surface you and your agent see does
not change.

## Independently usable

db.md is a self-contained standard with no external dependency. A plain
markdown vault becomes a db.md store — Obsidian users, researchers
running a topic wiki, an agentic computer keeping its company brain at
`~/db/`, any harness with a folder of markdown. No platform, no
account, no hosted service required. The spec is the contract; the
runtime is replaceable.

## Tooling

The format is the spec. The reference toolkit is one Rust binary,
`dbmd`, with subcommands for read / write / validate / extract
operations. Embeds `ripgrep` (via the `grep` crate) for fast search.
**Zero LLM dependencies**: no provider SDKs, no API keys, no model
calls anywhere in the binary. The agent runtime — Claude Code,
Codex, or any harness — is BYO and calls `dbmd` for file/data
operations. See `TOOLS.md` for the full toolkit reference.

**Agent bootstrap (two lines, any harness):**

```bash
# 1 — install the binary (one Rust binary, ~5MB, MIT/Apache)
cargo install dbmd-cli

# 2 — load the SPEC into the harness's system prompt
claude --append-system "$(dbmd spec)"                     # Claude Code
dbmd spec >> ~/.codex/instructions/db-md.md               # Codex
dbmd spec > /path/to/harness/system-prompt-fragment       # generic
```

Step 2's exact form depends on the harness; every harness with a
system-prompt mechanism works. After step 2 the agent carries the
canonical SPEC in context for every session — knows the format,
the recognized types, the curator contract, the session lifecycle,
and how to operate stores via `dbmd` subcommands. Per-store
overrides come from `DB.md` on every operation.

**Subcommand map** (grouped by session phase; full reference in
`TOOLS.md`). Every subcommand supports `--json` and `--help`; none
prompt interactively.

| Phase     | Subcommands |
|-----------|-------------|
| Open      | `dbmd spec`, `dbmd fm get DB.md <key>` |
| Warm up   | `dbmd log tail [N]`, `dbmd log since <ts>` |
| Read      | `dbmd search <q> [--type --in --where --linked-from --linked-to --updated-after --updated-before]`, `dbmd fm query <k>=<v>`, `dbmd fm get <file> <key>`, `dbmd graph <backlinks\|forwardlinks\|neighborhood\|orphans>`, `dbmd tree`, `dbmd outline <file>`, `dbmd stats`, `dbmd extract <file>`, `dbmd index show [<path>]` |
| Write     | `dbmd write <path> --type <t> [--summary --fm --body-file]`, `dbmd fm set <file> <k>=<v>`, `dbmd fm init <file>`, `dbmd link <from> <to>`, `dbmd rename <old> <new>`, `dbmd format <file>` |
| Validate  | `dbmd validate [--json]` (working set), `dbmd validate --all` (full sweep) |
| Maintain  | indexes are write-through; `dbmd index rebuild [--layer --folder --dry-run]` repairs / folds in bulk drops |
| Close     | `dbmd log <kind> <object> [-m <note>]` |

## Versioning

The spec is versioned with the repo tag (`v0.1`, `v0.2`, ...). Old
versions stay readable forever — additive changes only. New types
and new fields layer on top; tools that don't recognize them
ignore them.

## License

This spec is Apache-2.0. The reference tooling (`crates/dbmd-core`,
`crates/dbmd-cli`) is Apache-2.0. Examples are Apache-2.0.

Anyone can build tools that read or write db.md. The format is open.
