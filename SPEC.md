# db.md — format v0.4

`db.md` is **the open standard for databases in plain files**. Records are markdown
files with YAML frontmatter. Relationships are wiki-links. The database
is the directory; structured fields live in frontmatter; schemas live in
`DB.md`; indexes are plain derived sidecars (`index.md` / `index.jsonl`).
It is built for agents: a database a harness reads, writes, links, and
curates directly.

The bet is that agents change the default shape of software. For a large
class of semantic, evolving, workflow-heavy systems, the old stack —
database, backend, frontend — collapses into readable context, an agent
harness, and a generated surface. db.md is the persistence layer for
that shape: agent-operated files-as-database.

This is the broad middle of software: records, context, relationships,
workflows, decisions, policies, history, and a surface. What used to be
a Postgres schema, service layer, migrations, and a CRUD UI can become
files, frontmatter, wiki-links, `DB.md`, `index.md` / `index.jsonl`, and
a curator contract. An agent wants files it can reshape, not a schema to
migrate or a query language to wrap around. db.md is files. Simple and
open by design.

One directory, two folders, one config file. Raw evidence lives in
`sources/`. Everything the agent authors lives in `records/` — atomic
typed data and curator-synthesized narrative alike, separated by a
`meta-type` field, not by folder. The store's identity, agent
instructions, policies, and custom schemas all live in a single `DB.md`
file at the root.

This document is the format spec. The reference toolkit (`dbmd` CLI) ships
in this same repo. Anyone can build a db.md-aware tool — the format is
open and intentionally simple.

---

## Status

**Spec version:** `v0.4` — additive over v0.3: the universal frontmatter
contract gains a RECOMMENDED stable record `id` (a lowercase ULID,
minted by `dbmd write` when absent; a record without one stays fully
valid), and one paragraph reserves the `@brain/id` cross-store address
shape (see [Addressing (reserved)](#addressing-reserved)). No v0.3
store changes meaning or validity under v0.4.
**History: v0.3 collapsed the layout to two folders:**
`sources/` (evidence) + `records/` (everything the agent authors). The
old `wiki/` layer is removed; its synthesis role moves onto a
`meta-type: conclusion` field inside `records/`. This is a **breaking
format change** from v0.2 — but additive from v0.3 forward. There is no
migration command; an agent migrates a v0.2 store in place (see
[Versioning](#versioning)). v0.2's generic type model carries forward
unchanged: schema enforcement is solely the store's own `DB.md ##
Schemas` — the toolkit ships no built-in or implicit per-type schema,
and the example types (`contact`, `expense`, …) are illustrative, not
normative. See the [CHANGELOG](CHANGELOG.md) for the v0.2 → v0.3
migration.
**Stable:** the two-folder layout, the `DB.md` config file, and the
universal frontmatter contract are stable. From v0.3 on, the validation
vocabulary is additive.
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
│   ├── exports/
│   └── notes/               # testimonial sources — "a human told the agent X"
│       └── 2026/05/         # a growing stream, so date-sharded like the rest
└── records/                 # everything the agent authors; separated by meta-type, not folder
    ├── contacts/            # meta-type: fact · entity — flat (dedup-bounded)
    ├── companies/           # meta-type: fact · entity — flat
    ├── expenses/            # meta-type: fact · event — shards by date:
    │   └── 2026/05/         # …like sources, because event records track volume
    ├── meetings/            # meta-type: fact · event — shards by date
    ├── decisions/           # meta-type: fact · flat (no primary date field)
    ├── invoices/            # meta-type: fact · event — shards by date
    ├── profiles/            # meta-type: conclusion · synthesized entity narrative
    ├── playbooks/           # meta-type: conclusion · how-we-do-X
    ├── themes/              # meta-type: conclusion · cross-cutting patterns
    └── synthesis/           # meta-type: conclusion · cross-cutting overview pages
```

**Required:** the `DB.md` file + at minimum one of `sources/` / `records/` (most stores have both). Sub-folders by type are convention; tools may use other groupings. The synthesis narrative that lived in `wiki/` in v0.2 now lives in `records/` as files carrying `meta-type: conclusion` (see [Meta-type](#meta-type-fact-operational-conclusion)) — same store, distinguished by a queryable field, not a folder.

**Curator-maintained (optional, created on first curator action):** `index.md` (catalog of the store) and `log.md` (chronological action log). Absent at store creation; populated by the curator as it works. Each non-empty **type-folder** additionally carries an `index.jsonl` — the complete, machine-readable twin of its `index.md` (the `.md` is the capped human browse view; the `.jsonl` is the uncapped structured catalog that backs `dbmd query` / dedup). See [The `index.md` and `log.md` files](#the-indexmd-and-logmd-files).

**Filename convention:** the config file is `DB.md` (uppercase), matching README / LICENSE / NOTICE conventions for "main file in a project root" and differentiating from the standard name `db.md` (lowercase, referring to the project / spec). `index.md` and `log.md` are lowercase — they're curator-maintained content, not config.

### Two folders, two data models

A db.md store composes two folders in one directory. The hard boundary
is *evidence vs. agent-authored* — `sources/` vs. `records/`. Inside
`records/`, a `meta-type` field carries the finer distinction (atomic
data vs. synthesis) that v0.2 expressed as a separate `wiki/` folder.

- **`sources/`** — **document store.** Raw artifacts the agent did not
  author. Two kinds:
  - *Documentary* — external artifacts: emails, transcripts, exports,
    PDFs, scrapes. Preserved verbatim. Frontmatter is metadata about
    the artifact (where it came from, when it arrived); the body is the
    artifact itself.
  - *Testimonial* — a `note` source (with a `told_by` field) capturing
    "a human told the agent X." This is the evidence record for a
    chat-asserted fact that has no document behind it — written at the
    moment the testimony is given, so the assertion has a source to
    trace to.

  Sources are immutable and **carry no `meta-type`** — evidence is
  mono-role. Because sources never change after ingest, the toolkit
  processes each one once and never re-parses it; high-volume source
  folders auto-shard by date (`sources/emails/2026/05/`,
  `sources/notes/2026/05/`) so no directory grows unbounded. This is the
  layer built to reach millions of files — see [Scale](#scale).

- **`records/`** — **everything the agent authors.** One folder, two
  roles, separated by the `meta-type` frontmatter field:
  - **`meta-type: fact`** (the default) — atomic typed data points:
    expenses, meetings, decisions, invoices, contacts, companies.
    Frontmatter-heavy (the structured "row"), body-light or empty.
    Write-mostly, occasionally amended. "Relational but not that much,
    it's still markdown."
  - **`meta-type: operational`** — operating state the agent maintains:
    a running counter, a task list, a status board, a config the agent
    edits. Atomic like a fact, but mutated as state rather than appended
    as a data point.
  - **`meta-type: conclusion`** — curator-synthesized narrative with
    dense cross-references: the old `wiki/` layer. Body-heavy markdown
    with wiki-links to records and sources. The "understanding" that
    emerges from atomic records and raw sources. Rewrite-and-grow, and
    **single-voice** (see [Writers and readers](#writers-and-readers)).

The pattern: *sources are evidence; `fact`/`operational` records are
data; `conclusion` records are understanding.* Same store, two folders,
one cross-cutting field.

### How an agent uses db.md — four moves, in order

1. **Discover.** A skill-aware harness (Claude Code, Codex) surfaces the db.md
   skill by its description, or a human/manager points the agent at the store.
   The skill is only the doorway — it carries no contract of its own.
2. **Contract.** `dbmd spec` prints this document (bundled into the binary): the
   format, the curator contract, the session lifecycle, the validation codes,
   and the full subcommand surface. The agent reads it once per session — this
   is the single source of truth.
3. **Store config.** `DB.md` at the store root: identity (frontmatter) +
   per-store overrides (`## Agent instructions`, `## Policies`, `## Schemas`).
   Read on every operation on this store; it overrides the defaults, so read it
   before writing.
4. **Operate.** The store itself — `sources/` and `records/` — driven via
   `dbmd` subcommands. See [The agent session](#the-agent-session) for the loop.

## The universal frontmatter contract

Every markdown file in a db.md store carries YAML frontmatter with at
minimum:

```yaml
---
type: <type>          # required — what kind of thing this is
meta-type: <role>     # records only — fact | operational | conclusion (absent ⇒ fact)
id: <ulid>            # recommended — stable record id (lowercase ULID); minted on write
created: <RFC3339>    # required for content files; auto-set on create
updated: <RFC3339>    # required for content files; auto-maintained
summary: <one-line>   # required for content files; the catalog line
status: active        # optional; lifecycle state
tags: [tag1, tag2]    # optional; categorical labels
---
```

Type-specific fields layer on top. A store declares any it wants enforced
in `DB.md ## Schemas`; see [Example types](#example-types) for illustrative
patterns to copy.

**Content files** = everything under `sources/` and `records/`. **Meta files** = `DB.md`, `index.md`, `log.md` (these have their own contracts; they do not need `summary`, `created`, or `updated`).

### Meta-type: fact, operational, conclusion

`meta-type` is a **records-only, closed-enum** field that separates the
roles a `records/` file can play. It is the field that carries what the
v0.2 `wiki/` folder used to carry as a separate layer.

```yaml
meta-type: fact          # atomic typed data — the default
meta-type: operational   # operating state the agent maintains
meta-type: conclusion    # curator-synthesized narrative (the old wiki/)
```

- **Closed set.** The only valid values are `fact`, `operational`,
  `conclusion`. Any other value is a hard error
  (`FM_BAD_META_TYPE`). This is the one closed vocabulary in the
  frontmatter contract — `type` stays open, `meta-type` does not.
- **Default `fact`.** Absent ⇒ `fact`. The *effective* value (defaulted)
  is what the index records and what `--where meta-type=fact` matches —
  so an un-annotated record is a fact for every query, not a special
  case. `operational` and `conclusion` are always explicit.
- **Field, not folder.** Folders stay organized by `type` (the open
  noun: `contacts`, `playbooks`, `synthesis`). `meta-type` is a
  cross-cutting queryable field that can apply to any type. By
  convention a store keeps its conclusion types in their own folders
  (`records/synthesis/`, `records/profiles/`), but that is convention,
  not enforcement: `meta-type`, not the path, is what makes a file a
  conclusion.
- **Sources have no `meta-type`.** Evidence is mono-role; the
  documentary-vs-testimonial distinction inside `sources/` is a `type`
  distinction (`email`, `pdf-source`, `note`), not a `meta-type`.
- **`conclusion` is single-voice.** Records with `meta-type: conclusion`
  carry the single-writer synthesis contract that v0.2 attached to the
  `wiki/` layer; see [Writers and readers](#writers-and-readers) for
  what that means now that it is a per-file property inside the
  many-writer `records/` folder.

### The `id` field — stable identity, recommended

`id` is a content file's stable identity: the one value that survives rename
and reorganization, where filename identity does not. It is
**RECOMMENDED, not required** — a record with no `id` is fully valid
(hand-written stores stay legal), and identity then falls back to the
file path, which remains what wiki-links target.

The recommended form is a **lowercase ULID** — 26 characters of
Crockford base32, e.g. `01j5qc3v9k4ym8rwbn2tqe6f7d`. Why ULID:
time-sortable (a natural fit for a store whose event records already
order by time), compact and YAML-clean (one short unquoted token, no
special characters), offline-mintable with zero coordination (any
writer can mint one — no registry, no counter, no network),
collision-safe at any realistic write rate, and widely understood.
Lowercase is the recommended form: it reads like the rest of the
frontmatter and stays shell-friendly.

- **Minted by tooling on write.** `dbmd write` mints a lowercase ULID
  when the new file carries no `id` (an explicit `--fm id=…` wins).
  `dbmd fm init` does not retrofit one — adding ids to existing files
  is the agent's call, not a side effect.
- **Absent = valid.** No validation code fires on a missing `id`.
- **Uniqueness scope = the store.** Two files in one store declaring
  the same `id` is a hard error (`DUP_ID`). Nothing is claimed about
  ids across stores — see [Addressing (reserved)](#addressing-reserved).
- **Filename identity stays the link fallback.** Wiki-links target
  store-relative paths, not ids; `id` layers stable identity on top of
  path identity and replaces nothing.
- **Hand-authored opaque ids stay legal.** `id` is an opaque token; the
  ULID form is the recommendation, not a gate. `dbmd validate` warns
  (`FM_BAD_ID`) only on an id that cannot work as an identifier at all
  — a non-scalar value, an empty value, or one containing whitespace.
- **Queryable like any field.** `id` rides the type-folder
  `index.jsonl` with the rest of the frontmatter, so
  `dbmd query --where id=<id>` is the lookup.

**The `summary` field is canonical and required on every content file.** It is the **single source of truth** for what the file is about. Every hierarchical `index.md` reads this field directly to populate its catalog entries — no extraction rules, no recomputation. The agent writes a thoughtful summary when creating files (the curator's judgment), `dbmd fm init` writes a deterministic default if the agent doesn't (the type's `summary_template` from `DB.md ## Schemas`, or the file's first paragraph), and the agent can always override via `dbmd fm set <file> summary='...'`.

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
- `id` is recommended, not required (see
  [The `id` field](#the-id-field--stable-identity-recommended)); a
  file without one is identified by its store-relative path — the
  same identity wiki-links target.
- Timestamps are full RFC3339 date-times
  (`2026-05-27T08:00:00-07:00`); a bare date fails validation
  (`FM_BAD_TIMESTAMP`).
- Unknown fields pass through. Tools that don't recognize a field
  treat it as ambient context.

## Example types

db.md has **no built-in type vocabulary.** `type` is a free-form label;
every type is the store's own. The table below is an **illustrative
example domain** (a company / CRM brain) — copy what fits into your
`DB.md ## Schemas`, ignore the rest, invent your own. The only structural
types the toolkit knows are the three meta files (`db-md`, `index`,
`log`); every content type is yours.

**Every content type (everything below except `db-md`, `index`, `log`) requires `summary` in frontmatter** — see the [universal frontmatter contract](#the-universal-frontmatter-contract). The "Type-specific fields" column lists fields *in addition to* the universal contract (`type`, `id`, `created`, `updated`, `summary`, `status`, `tags`). The `meta-type` column applies to `records/` only (`sources/` rows have none); absent ⇒ `fact`.

| `type`         | Layer    | `meta-type`  | Default location         | Type-specific fields (in addition to the universal contract)          |
|----------------|----------|--------------|--------------------------|-----------------------------------------------------------------------|
| `db-md`        | root     | —            | `DB.md` (the file)       | `scope`, `owner`, `computer_id` (if any). *Meta file: no `summary`.*  |
| `index`        | any      | —            | `index.md` (root / per-layer / per-type-folder) | `scope: root\|layer\|type-folder`, `folder: <path>` (on layer + type-folder). *Meta file: no `summary`.* |
| `log`          | root     | —            | `log.md` (single, global)| (none — body is the timeline). *Meta file: no `summary`.*             |
| `email`        | sources  | —            | `sources/emails/`        | `from`, `to`, `date`, `subject`, `thread`, `in_reply_to`              |
| `transcript`   | sources  | —            | `sources/transcripts/`   | `recorded_at`, `attendees`, `duration_min`, `language`                |
| `pdf-source`   | sources  | —            | `sources/docs/`          | `received_from`, `received_at`, `doc_type`                            |
| `note`         | sources  | —            | `sources/notes/`         | `told_by`, `told_at` — testimonial source ("a human told the agent X")|
| `contact`      | records  | `fact`       | `records/contacts/`      | `name`, `email`, `company` (link → `records/companies/`), `role`, `first_touch`, `last_touch`|
| `company`      | records  | `fact`       | `records/companies/`     | `name`, `domain`, `industry`, `relationship`                          |
| `expense`      | records  | `fact`       | `records/expenses/`      | `date`, `amount`, `currency`, `category`, `vendor` (link → `records/companies/`), `contact` (link → `records/contacts/`)|
| `meeting`      | records  | `fact`       | `records/meetings/`      | `date`, `attendees`, `location`, `duration_min`, `expense` (link → `records/expenses/`)|
| `decision`     | records  | `fact`       | `records/decisions/`     | `decided_by`, `affects`, `alternatives_considered`                    |
| `invoice`      | records  | `fact`       | `records/invoices/`      | `date`, `amount`, `vendor` (link → `records/companies/`), `status`, `paid_at`|
| `tasklist`     | records  | `operational`| `records/tasklists/`     | `items`, `owner` — operating state the agent mutates                  |
| `concept`      | records  | `conclusion` | `records/concepts/`      | `derived_from` (list of record/source links)                          |
| `profile`      | records  | `conclusion` | `records/profiles/`      | `derived_from` (list of record/source links)                          |
| `playbook`     | records  | `conclusion` | `records/playbooks/`     | `derived_from` (list of record/source links)                          |
| `theme`        | records  | `conclusion` | `records/themes/`        | `derived_from` (list of record/source links)                          |
| `synthesis`    | records  | `conclusion` | `records/synthesis/`     | `derived_from` (list of record/source links)                          |
| `account`      | records  | `conclusion` | `records/accounts/`      | `derived_from` (list of record/source links)                          |

The `meta-type: conclusion` types (`concept`, `profile`, `playbook`,
`theme`, `synthesis`, `account`) are the v0.2 `wiki-page` split into real
types. `wiki-page` is **retired**: synthesis now carries a domain `type`
in `records/` plus `meta-type: conclusion`, not a single generic
`wiki-page` type in a `wiki/` folder.

**Reading rules:**

- Every type passes through. The toolkit recognizes no type specially; a
  reader that doesn't know `type: proposal` (or `type: contact`) reads the
  file as ambient context.
- The folder layout is convention, not enforcement. A `type: contact`
  in `sources/foo/` is valid (though unusual). `meta-type`, not the
  folder, is what makes a record a fact, operational state, or a
  conclusion.
- A single entity (e.g. a person) can have both a `records/contacts/`
  data row (`meta-type: fact`) AND a `records/profiles/` narrative page
  (`meta-type: conclusion`). The record is the atomic fact; the profile
  is the synthesis that cross-references it. Both live under `records/`;
  the `meta-type` field, not a separate folder, tells them apart.
- A testimonial fact — something a human asserted in chat with no
  document behind it — is captured as a `note` source under
  `sources/notes/` with a `told_by` field, then the `records/` data it
  drives can trace back to it. This is how a chat-asserted fact gets a
  source (see [The curator contract](#the-curator-contract) →
  source-first capture).
- Every content type requires `summary` — the field is universal across
  content files, whatever the `type`.
- **No type carries a built-in schema.** Field requirements, shapes, link
  prefixes, and uniqueness are enforced *only* where the store's
  `DB.md ## Schemas` declares them (see [The `DB.md` file](#the-dbmd-file)).
  An example type above becomes enforced the moment you copy its schema
  into `## Schemas` — and not before. So a field like `contact.company` is
  a plain label until a `### contact` schema declares it
  `link to records/companies/`.

**Worked example — a `contact` record (note `summary` in frontmatter):**

```yaml
---
type: contact
id: 01j2vs3f8gq0h6x2m9t4kcyrbw
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

**Deterministic default `summary`** (what `dbmd fm init` / `dbmd write`
write when the agent doesn't): the type's `summary_template` from
`DB.md ## Schemas` if one is declared, else the file's first non-heading
paragraph, truncated to ≤200 chars. A `summary_template` interpolates
`{field}` placeholders from frontmatter — so a `### contact` schema with
`summary_template: {role} at {company} (last_touch: {last_touch})`
reproduces a contact's default line, now as the store's own declaration
rather than a built-in. A `{field}` that is a wiki-link renders its
display-or-leaf text; a list field renders comma-joined; an absent field
renders empty.

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
[[records/profiles/sarah-chen|Sarah]]
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

**Soft collisions (warnings; schema-declared uniqueness):**
- Two records of a type that share a `DB.md ## Schemas` `unique:` key.
  A `unique:` directive names one or more fields (a compound key when
  more than one); records of that type whose combined values match
  collide. A list-valued key field collapses to a sorted set, so order
  never matters (e.g. a meeting's attendee set). A record missing any
  key field (or leaving it empty) is **skipped** — an incomplete key
  never collides (SQL's `NULLS DISTINCT` rule). A key built on an
  optional field therefore silently stops checking the records that
  omit it: **build `unique:` keys from `required` fields.**
  `dbmd validate` warns (`DB_MD_SCHEMA_FIELD`) when a key names a field
  the schema does not mark `required`.

No type carries a built-in dedup key — the store opts in, per type. A
`### contact` schema with `unique: email` warns on two contacts sharing
an email; `### expense` with `unique: date, amount, vendor` warns on a
re-entered expense; `### meeting` with `unique: date, attendees` warns on
the same meeting logged twice regardless of attendee order.

Soft collisions don't fail validation; they emit warnings the agent
reads (machine-parseable via `dbmd validate --json`) and decides
how to resolve — usually by `dbmd rename` to merge or `dbmd link` to
cross-reference. The toolkit detects; the agent decides.

Each collision maps to a structured issue code (`DUP_ID` for the
universal `id` field, `DUP_UNIQUE_KEY` for a schema-declared `unique:`
key); see [Validation](#validation) for the complete code vocabulary.

A reader that doesn't speak wiki-links treats them as text — no
breakage.

## Addressing (reserved)

db.md reserves one cross-store address shape: **`@brain/id`** — a store
handle, a slash, and a record `id` (e.g.
`@acme-ops/01j5qc3v9k4ym8rwbn2tqe6f7d`). It names a record in another
store the way a wiki-link names one in this store. **v0.4 reserves the
shape only.** How a handle is registered, resolved, verified, or
fetched is explicitly out of scope for db.md; resolution belongs to a
future interconnect spec, link.md. Within a store nothing changes:
wiki-links remain the only reference form the toolkit parses,
validates, and rewrites, and a db.md tool encountering `@brain/id`
treats it as plain text. Stores that want stable cross-store addresses
someday are the reason `id` is recommended today.

## The `DB.md` file

Every db.md store has a `DB.md` file at its root. Presence of `DB.md`
(uppercase) is the canonical signal that a folder is a db.md store.
The casing is deliberate: `DB.md` matches the README / LICENSE /
NOTICE convention for "main file in a project root" and visually
differentiates the file from the standard name `db.md`.

**If `DB.md` is absent**, the directory is not a db.md store. Every
store-walking `dbmd` subcommand (`validate`, `search`, `graph`,
`query`, `index rebuild`, `stats`, ...) exits non-zero with
structured error code `NOT_A_STORE` rather than guessing.
**Creating a store is the agent's job, not a tool command.** `DB.md` is
operator/agent-authored — you write it. There is deliberately **no `dbmd
init`**, no scaffold, no template: `dbmd` is plumbing (it validates, indexes,
queries, links), and a capable agent authors what a tool would otherwise
generate. To make a fresh store, create the folders and write a `DB.md`:

```bash
mkdir -p mystore/{sources,records}
# then write mystore/DB.md yourself — minimally:
#   ---
#   type: db-md
#   scope: company        # company | personal | research | <custom>
#   owner: <name>
#   ---
```

**The store is version-controlled by default.** This is a design choice, not a
preference: a db.md store is plain files whose value is that Git or a sync
service can save, version, audit, and carry them, so a store defaults to a save
boundary and never to a bare, unversioned path. Inside a git repo whose data
this is, the store lives in the repo (`<repo>/db/`) and rides its history; with
no repo, the store is its own git repo (`git init`) or lives in a synced folder.
A machine-global alias like `~/db` is fine as a symlink to the real, versioned
store, not as the store's bare home. An agent sets this up by default; the
operator opts out explicitly for a throwaway store. Do not move repo-owned data
out to an external location without the operator's explicit confirmation.

The file carries identity in frontmatter and optional per-store overrides in
sections. **Required frontmatter: `type: db-md`, `scope`, and `owner`** — a
store missing `scope` or `owner` fails `dbmd validate` with
`DB_MD_MISSING_FIELD`. Optional: any of the standard sections below.

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
British English in `meta-type: conclusion` records. When a vendor
invoice arrives, also create an `expense` record linked to the invoice.
Don't synthesize conclusion records from sources tagged `transient`.

## Policies

### Frozen pages
- `records/decisions/2026-q1-strategy.md` — finalized, do not modify.
- `records/synthesis/2026-annual-plan.md` — signed-off plan.

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
- unique: email
- summary_template: {role} at {company} (last_touch: {last_touch})

### expense
- date (required, date)
- amount (required)
- currency (default USD)
- category (string)
- vendor (required, link to records/companies/)
- receipt (link to sources/)
- unique: date, amount, vendor
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
    derived `meta-type: conclusion` records, no new records).
- **`## Schemas`** — the store's type definitions. This is the **only**
  source of schema enforcement; the toolkit ships no built-in or implicit
  per-type schema. Parseable and enforced by `dbmd validate`.

  Each schema is a `### <type>` heading followed by field and directive
  bullets. A **field** is `- <field-name> (<modifiers>)` (one per line;
  modifiers comma-separated inside parens; a bullet without parens is a
  free-form optional field of any shape). A **directive** is
  `- <keyword>: <value>` with a reserved keyword; `unique`,
  `summary_template`, and `shard` are reserved and can't be used as field
  names.

  **Recognized field modifiers:**
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

  **Directives:**
  - `unique: <field>[, <field> ...]` — a uniqueness constraint over the
    listed field(s) (compound when more than one). Two records of this
    type whose values collide warn as `DUP_UNIQUE_KEY`. Repeat the
    directive for independent constraints. A wiki-link field compares by
    target; a list field compares as a sorted set. A record missing any
    key field (or leaving it empty) is skipped — an incomplete key never
    collides — so **every key field should be `required`**;
    `dbmd validate` warns (`DB_MD_SCHEMA_FIELD`) otherwise.
  - `summary_template: <template>` — the `{field}`-interpolation pattern
    `dbmd fm init` / `dbmd write` use to compose this type's default
    `summary` (see [Example types](#example-types)).
  - `shard: by-date | flat` — whether records of this type are date-sharded
    on disk (`records/<type>/<YYYY>/<MM>/…`, keyed off the type's primary
    date field) or kept flat. This is the generic-model way to declare
    sharding: it overrides the toolkit's built-in default for the type, so a
    custom event type opts into sharding with `shard: by-date`, and any type
    can force flat with `shard: flat`. An unrecognized value is ignored.

    **Built-in shard defaults** (absent a `shard:` directive): db.md
    date-shards the source types (`email`, `transcript`, `pdf-source`,
    `note`) and the event record types (`expense`, `invoice`, `meeting`, plus
    the recognized custom event types `order`, `ticket`, `transaction`). Every
    other type is flat by default — entity records (`contact`, `company`,
    `decision`), conclusion records (`profile`, `concept`, `synthesis`), and
    any unlisted custom type. A custom event type outside that set opts into
    sharding with `shard: by-date`.

  Unknown modifiers are ignored (read as ambient context, no error). A
  type with no `### <type>` block is unconstrained — any frontmatter is
  valid for it.

  `dbmd validate` emits structured `Issue`s (codes
  `SCHEMA_MISSING_REQUIRED`, `SCHEMA_SHAPE_MISMATCH`,
  `SCHEMA_LINK_PREFIX_MISMATCH`, `SCHEMA_ENUM_VIOLATION`,
  `DUP_UNIQUE_KEY`) so the agent can read and remediate them via `--json`.
- **`## Folders`** — optional display overrides for the generated rollup
  `index.md` files (the root and layer levels). Bullets sit directly under the
  H2 (no `### <type>` sub-sections), one per type-folder:
  `- <folder-path>[|<display name>][ — <description>]`. The optional
  `|<display>` overrides the rollup's derived folder name (the wiki-link
  `|display` convention); the text after the first em-dash (`—`, or a ` - `
  fallback) is a one-line description. Surrounding backticks on the path are
  tolerated. `dbmd index` reads these to fill each rollup entry's name and
  description; absent, the name is derived from the folder basename and no
  description is shown.

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
└── records/
    ├── index.md              # LAYER
    ├── contacts/
    │   ├── index.md          # TYPE-FOLDER — every contact record (meta-type: fact)
    │   └── (.md files)
    ├── companies/
    │   ├── index.md
    │   └── ...
    ├── profiles/
    │   ├── index.md          # TYPE-FOLDER — every bio (meta-type: conclusion)
    │   └── ...
    └── synthesis/
        ├── index.md          # TYPE-FOLDER — cross-cutting overviews (meta-type: conclusion)
        └── ...
```

**The three levels:**

- **Root `index.md`** — exists whenever the store has any files. Lightweight: lists each layer + each type folder under it with counts. One entry per type folder; does NOT enumerate every file. Wiki-links target the layer indexes.
- **Layer `index.md`** (`sources/index.md`, `records/index.md`) — exists whenever that layer has any files. Lists each type folder under the layer with counts and brief summaries. Wiki-links target type-folder indexes.
- **Type-folder `index.md`** — exists whenever the type folder has any files. The **human / recency browse view**: lists files in the type-folder, **across date-shards**, with a one-line summary, **capped at 500 entries** selected by recency (newest first by the frontmatter `updated` field — clone-stable, unlike filesystem mtime, which `git clone` resets — ties broken by store-relative path ascending). Above the cap it lists the 500 most-recent and ends with a `## More` section pointing to `dbmd query --type <t> --in <layer>` (the complete twin below) for full enumeration. The cap keeps the browse view inside an LLM context budget and bounded in the write loop; completeness lives in the `index.jsonl` twin, not here.
- **Type-folder `index.jsonl`** — the complete, **uncapped** machine twin of `index.md`: one JSON object per file in the folder (across date-shards), `{path, type, summary, tags, links, created, updated, <other frontmatter fields>}` — where **`tags` and `links` are the document's expansion** (`tags` = the LLM's flat semantic labels; `links` = wiki-links to concept pages + related records). Same kind of artifact as `index.md` — a derived, write-through, rebuildable **plain file** (JSONL, so it stays git-diffable line-by-line and ripgrep-able), not a database engine. The current toolkit keeps it compacted on write for byte-for-byte rebuild equivalence; readers also tolerate un-compacted append-style lines by applying last-write-wins by path. It is the **backing for structured reads**: `dbmd query`, `dbmd search --type/--where`, the dedup pre-write checks, and `dbmd graph backlinks` read it (one sequential, complete read per type-folder — cold-cache-proof) instead of scanning frontmatter across the tree. This is what makes the catalog complete *and* fast with no engine; ad-hoc full-text body search stays ripgrep. **Tags ≠ concepts:** a tag is a flat label (the agent filters/aggregates it on demand from this sidecar; no page of its own); a concept is a `meta-type: conclusion` record the doc links to (`links`), navigated via `graph backlinks`. Both are LLM-authored, never inferred — they are the *doc-side* of query expansion, so the agent's expanded query and the document's tags/concepts meet lexically here, with no embeddings. (The root stays a markdown-only rollup. A layer is a markdown-only rollup too in the common case — its `.jsonl` twin appears only when content files live *directly* at the layer root with no type-folder between them; see **Loose files** below.)

**Empty folders have no `index.md`.** Folders below the type-folder level (sub-sub-folders, if an operator creates them) are operator territory — not part of the canonical hierarchy, no auto-indexing.

**Loose files (content directly at a layer root).** A content file MAY live directly at a layer root — `records/<file>.md` or `sources/<file>.md` — with no type-folder between it and the layer. Folder layout is convention, not enforcement (§ Layers), so this is a valid store: `dbmd write` never produces it (it routes every type to a canonical type-folder), but a bulk import or a hand-edit can. Such *loose* files are catalogued in the **layer's own `index.jsonl`** — the same complete, uncapped structured twin a type-folder carries, anchored at the layer dir — so structured reads (`dbmd query`, `dbmd search --type`, the dedup pre-write checks, `dbmd graph`) see a loose file exactly as they see a canonical one, with no whole-store walk. The layer `index.md` stays a type-folder rollup and does NOT list loose files (the layer `index.jsonl` is their catalog); a layer with no loose files carries no `index.jsonl`, so canonical stores are byte-unchanged. The layer `index.jsonl` is maintained write-through and rebuilt by `dbmd index rebuild`, byte-identically. `dbmd validate --all` reports `INDEX_JSONL_MISSING` when a loose file is absent from its layer `index.jsonl` (and `INDEX_JSONL_DESYNC` / `INDEX_JSONL_STALE` for a sidecar out of sync with the files), so a loose file is never *silently* missing from the catalog. (The canonical home for a record is still its type-folder; loose placement is supported, not encouraged — `dbmd rename`-ing a loose file into its type-folder removes the layer sidecar once the layer has no loose files left.) A layer — or an entire store — whose only content is loose files has **no rollup `index.md`** at the layer or root level: there are no type-folders to summarise, so the layer `index.jsonl` is the whole catalogue, and `dbmd validate` does not require a rollup that would have nothing to roll up.

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
- [[records/contacts/index|Contacts]] (27 files) — people we've interacted with · meta-type: fact
- [[records/companies/index|Companies]] (12 files) — vendor and customer orgs · meta-type: fact
- [[records/meetings/index|Meetings]] (34 files) · meta-type: fact
- [[records/profiles/index|Profiles]] (15 bios) · meta-type: conclusion
- [[records/themes/index|Themes]] (3) · meta-type: conclusion
- [[records/synthesis/index|Synthesis]] (5) · meta-type: conclusion
```

**Example — folder `index.md` (e.g. `records/profiles/index.md`):**

```markdown
---
type: index
scope: type-folder
folder: records/profiles
updated: 2026-05-27T10:00:00Z
---

# records/profiles

- [[records/profiles/sarah-chen]] — Renewal-champion bio; Q2 timeline
- [[records/profiles/elena-rodriguez]] — Acme VP; engineering relationship
- [[records/profiles/marcus-okafor]] — New Northstar contact (May 2026)
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
  Use `dbmd query --type email --in sources` for the complete catalog.
  ```
- **Indexes are maintained write-through, not rebuilt in the loop.** The write commands (`dbmd write` / `dbmd fm init` / `dbmd fm set` / `dbmd rename`) update the affected entries as the agent works — bounded to the affected type-folder: splice the ≤500-entry `index.md`, read/update/rewrite that folder's compact `index.jsonl`, plus refresh the parent counts. The catalog is always current; there is no full-store rebuild step in the normal session. `dbmd index rebuild` is the from-scratch repair — after a bulk external drop into `sources/`, or to recover a damaged index — walking the store once, rewriting all three levels (both `index.md` and the complete `index.jsonl`, compacting the jsonl), deleting stale indexes. Never edited by hand.

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

## [2026-05-27 10:08] ingest | sources/notes/2026/05/sarah-told-cto-name.md
Sarah told the agent the CTO is Marcus Okafor (no document yet); told_by: Sarah.

## [2026-05-27 10:10] update | records/companies/northstar
Seat count 120 → 175 (pending signature).

## [2026-05-27 10:15] update | records/profiles/elena-rodriguez
Added Q2 renewal context. Linked records/meetings/2026-05-22-renewal-call.

## [2026-05-27 10:20] validate
PASS — 0 errors, 2 warnings (unknown type `proposal` in records/proposals/x.md; orphan records/themes/draft.md).
```

**Conventions:**
- Entry header: `## [YYYY-MM-DD HH:MM] <kind> | <object>` (object optional for store-wide actions like `validate`).
- Recognized kinds: `ingest`, `create`, `update`, `delete`, `rename`, `link`, `validate`, `index-rebuild`, `contradiction`. Custom kinds are valid; `dbmd validate` warns on unrecognized kinds without failing.
- Body (one or more lines) explains what happened.
- Append-only. The curator never rewrites past entries; if a finding is wrong, append a corrective entry below it.
- Parseable with `grep "^## \[" log.md | tail -5` or any similar pipeline (or `dbmd log tail`).
- **Rotation.** `log.md` is the active timeline; `dbmd log` automatically rolls older months into `log/<YYYY-MM>.md` on append. The full history is the archives plus the active file — one timeline, paginated so the active file (and every read of it) stays small no matter how old the store gets. `dbmd log tail` / `dbmd log since` reverse-read from the active file and cross into archives only when the requested range does.
- **Concurrent-clone merges.** A single-writer store (one agent, one clone — the standing contract; see [Writers and readers](#writers-and-readers)) never has a merge. When two git clones of a store both append (multi-machine sync, a shared repo), git's line merge conflicts on the shared end-of-file region. Resolution is the agent's: a curator with this SPEC in context semantically merges — keep both entries, order by timestamp. For merges where no agent is in the loop (a human, CI), set `log.md merge=union` in `.gitattributes`: because every entry is timestamped, the union driver keeps both sides (never drops one) and a later agent pass reorders. The derived `index.md` needs no merge logic at all — on conflict, regenerate it with `dbmd index rebuild`.

## The curator contract

The "curator" is a **role**, not a binary. Any agent (Claude Code,
Codex, a custom harness) operating a db.md store plays the curator
role. The spec defines the behavior contract; the agent runtime is the
user's choice. **db.md ships no LLM runtime and no API keys.**

**The agent acting as curator:**

1. **Knows the SPEC** (this document — carried by the harness from
   bootstrap, whether as an installed skill the agent discovers or
   piped into the system prompt via `dbmd spec`; see
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
5. **Extracts atomic facts from new sources into `records/`** as
   `meta-type: fact` records (e.g. an email becomes a `meeting`
   record + a `contact` record). **Every created content file gets a
   `summary` in its frontmatter** — thoughtful summary if the agent
   has context for one; `dbmd fm init` writes a deterministic default
   otherwise.
6. **Captures ephemeral testimony as a source, source-first.** When a
   fact arrives only in conversation — a human asserts something with
   no document behind it — the agent writes a `note` source under
   `sources/notes/` (with `told_by`) *at the moment of the create or
   update it drives*, so the asserted fact has an evidence record to
   trace to. This is a curator **discipline**, not a toolkit-enforced
   invariant: the toolkit does not refuse a source-less record. The
   one thing that genuinely must be captured at write time is the
   testimony itself, because — unlike a persistent document — an
   unsaved conversation cannot be reconstructed later. See
   *source-first provenance* below.
7. **Synthesizes conclusion records** (`meta-type: conclusion`) —
   `profile`, `concept`, `playbook`, `theme`, `synthesis`, `account`
   records reflecting entities, projects, and themes — across records
   and sources, with dense wiki-links. These are the old `wiki/`
   pages, now real types under `records/`. **Same summary contract:
   every conclusion record has `summary` in frontmatter.** Conclusion
   records are **single-voice** — one curator reconciles them (see
   [Writers and readers](#writers-and-readers)).
8. **Refreshes `summary` whenever the content meaningfully changes**
   — e.g. if a contact's role changes, the agent updates both the
   `role` field and the `summary` field. Stale summaries are an
   anti-pattern the curator keeps in check by re-reading and
   refreshing the `summary` alongside any substantive body edit.
9. **Maintains cross-references** (a `profile` conclusion about a
   person links to the contact record, the company record, and
   meeting records).
10. **Reconstructs provenance on demand.** Every record *should* trace
    to a source — documentary or testimonial — but db.md does **not**
    materialize that as a mandatory per-record link. The agent
    reconstructs the chain when it is needed (audit, contradiction,
    review) by matching record to source *by meaning* — which is what
    the agent is good at and what makes per-record provenance links
    premature. Persistent documentary sources are always
    reconstructable; only ephemeral testimony must be captured up
    front (step 6). This is a **discipline that keeps provenance
    reconstructable**, not a mechanical guarantee that every record is
    grounded — see [the honesty note below](#a-note-on-what-is-and-isnt-enforced).
11. **Flags contradictions** (two sources disagree on a contact's
    employer) without silently picking a winner. Canonical
    mechanism: append a `## Open questions` section to the relevant
    conclusion record with both candidate facts cited via wiki-links
    to the conflicting sources, then `dbmd log contradiction <object>
    -m "<short description>"`. Surface the disagreement; let the
    operator (or a later session with more evidence) resolve.
12. **Relies on write-through indexes.** The write commands
    (`dbmd write` / `dbmd fm init` / `dbmd fm set` /
    `dbmd rename`) keep the hierarchical `index.md` catalog (root,
    layer, type-folder) current as the agent works — there is no
    rebuild step in the normal loop. After a bulk external drop
    into `sources/` (rsync, mbsync), the agent runs `dbmd index
    rebuild` once to fold the new files in. See [Scale](#scale).
13. **Appends to `log.md`** on every action — ingest, create,
    update, delete, rename, link, validate, contradiction
    (`dbmd log <kind> <object> -m <note>` is the canonical
    append).
14. **Respects `## Policies` in `DB.md`** — the toolkit refuses
    writes to `### Frozen pages`, so the agent doesn't have to
    remember the list; the agent's part is knowing the policy
    exists and choosing alternate paths or escalating to the
    operator when blocked. `### Ignored types` are never
    synthesized into derived `meta-type: conclusion` records.

**The agent does not (in its curator role):**

- Delete files in `sources/`. Sources are evidence; the operator
  deletes them explicitly.
- Edit `DB.md`. That's operator-owned.
- Rewrite past `log.md` entries. The log is append-only; corrections
  go on the end.
- Bypass the contract by editing the store out from under it —
  hand-patching frontmatter, indexes, the log, or wiki-links in ways
  that break the invariants this document defines. Drive store
  operations through a conforming db.md tool: `dbmd` is the reference
  implementation, and its subcommands are the canonical verbs this
  contract is written against. (`dbmd` is replaceable, not mandatory —
  anyone can build a db.md-aware tool; the contract is the format and
  these invariants, not the binary.) The harness can do anything it
  wants outside the store.

### Source-first provenance

The discipline behind steps 6 and 10, stated once:

- **Every record should trace to a source** — documentary (an email, a
  PDF, an export under `sources/`) or testimonial (a `note` under
  `sources/notes/`). Records are the agent's distillation of evidence;
  the evidence is what makes a record defensible.
- **Provenance is reconstructed, not linked.** db.md does not require a
  per-record link back to its source, and ships no check that one
  exists. The reconstruction tool is the agent: it matches a record to
  its source by meaning when provenance is actually needed. A persistent
  document is always there to be re-found, so materializing the link at
  write time is premature.
- **Testimony is the exception that must be captured.** An unsaved
  conversation cannot be reconstructed. So when a fact arrives only in
  chat, the agent writes the `note` source *coupled to* the create or
  update it drives — source-first, at write time, before the testimony
  is gone.

#### A note on what is and isn't enforced

"Every record traces to a source" is a **curator discipline, not a
mechanically enforced invariant.** With no required link and no shipped
check, the toolkit cannot itself tell a grounded record from an
ungrounded one — `dbmd validate` will not fail a record that has no
source. The discipline keeps provenance *reconstructable*; it does not
*prove* groundedness. Treat it as a contract the curator upholds, not a
guarantee the format gives you. (A future opt-in `RECORD_UNGROUNDED`
info-level check could surface obviously source-less records, but the
spec ships no such check by default; see [Roadmap](#roadmap).)

### Importing existing data

Bringing an existing knowledge base in — a folder of notes, an Obsidian
vault, a Notion export, another wiki — follows four rules on top of the
source-first discipline above:

- **Provenance, not polish, decides the layer.** An artifact you did not
  author is a *source*, however finished it looks. A polished external wiki is
  evidence: it lands under `sources/`, and the `meta-type: conclusion` records
  are the synthesis *your* agent writes *from* it (reconstructable by meaning,
  per above). Don't file someone else's synthesis directly as your conclusions
  — that claims an authorship the store can't defend.
- **Reference vs. replace.** *Reference*: the external base stays the source of
  truth — import it under `sources/` and synthesize records from it. *Replace*:
  the store becomes its living home — the content becomes `records/`, but only
  for content **this store's curator** authored and is lifting (its own prior
  synthesis → `meta-type: conclusion`). The **operator's** own pre-existing
  notes are testimony, not the curator's synthesis: they land as testimonial
  `note` sources (`told_by`), and records are distilled from them. Either way
  the raw export is kept under `sources/` as the frozen provenance/rollback
  copy. When unsure, default to reference; promote to records when the store is
  meant to own the content.
- **Don't port the source system's folder tree.** Its hierarchy was built for
  humans clicking folders. Reorganize by `type` + `meta-type`, wiki-links, and
  the derived index; the old folder names become `type`/`tags`, not nested
  directories. Map by meaning, not by mirroring.
- **Rewrite the source system's link syntax at ingest.** "Preserved verbatim"
  is about content, not link syntax: an imported body's internal references
  (`[[Sarah]]`, an Obsidian short link) must become full store-relative
  wiki-links (`[[records/contacts/sarah-chen]]`) — or plain text when the
  target was not brought in — otherwise the store fails its first
  `dbmd validate` sweep (`WIKI_LINK_SHORT_FORM` / `WIKI_LINK_BROKEN`).
  Rewriting the reference is not editing the evidence.

### Pre-write checks

Before `dbmd write`, `dbmd link`, or `dbmd fm set`, the agent
should:

1. **Search for existing entities** to avoid soft collisions —
   `dbmd query --where email=<addr> --in records/contacts` before creating
   a `contact`; `dbmd query --where domain=<host> --in records/companies`
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
   `meta-type: conclusion` record (a `concept` page) rather than a tag —
   tags are flat labels, concepts are pages.

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

1. **Open** — the harness already carries the SPEC from bootstrap
   (an installed skill, or `dbmd spec` in the system prompt; see
   [Tooling](#tooling)); if it doesn't, run `dbmd spec` and load it
   now. Read the store's `DB.md` for identity, agent instructions,
   policies, and schemas.
2. **Warm up** — `dbmd log tail 20` to learn what was done lately;
   `dbmd log since <last-session-time>` for a precise diff.
3. **Operate** — read with `dbmd search` / `dbmd query` /
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
  "file": "records/profiles/sarah-chen.md",
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
store size. If the default call has no logged changed objects to
inspect (fresh store, missing log, or external edits not recorded in
`log.md`), it falls back to a per-file content sweep so validation
never passes vacuously. `dbmd validate --all` walks the entire store —
every link, every index, and the entity-dedup collisions (`DUP_*`),
which the working-set pass leaves to the pre-write checks and to
`--all`. Both
modes emit the same issue vocabulary below.

**Canonical issue codes** (the complete vocabulary the agent will
see; grouped by category):

| Code | Severity | Meaning / remediation |
|------|----------|-----------------------|
| `NOT_A_STORE` | error | path has no `DB.md`; not a db.md store |
| `DB_MD_BAD_TYPE` | error | the store's `DB.md` is not `type: db-md` |
| `DB_MD_MISSING_FIELD` | error | the store's `DB.md` frontmatter lacks `scope` or `owner` |
| `DB_MD_UNKNOWN_SECTION` | warning | `DB.md` has an `##` section other than `Agent instructions` / `Policies` / `Schemas` / `Folders` |
| `DB_MD_SCHEMA_FIELD` | warning / info | a `DB.md ## Schemas` declaration is malformed (empty or duplicate field name → warning), carries an unrecognized modifier (→ info), or a `unique:` key names a field not marked `required` (→ warning; incomplete keys are silently skipped) |
| `FM_MISSING_TYPE` | error | content file has no `type:` |
| `FM_MISSING_CREATED` | error | content file has no `created:` timestamp — run `dbmd fm init` or set RFC3339 manually |
| `FM_MISSING_UPDATED` | error | content file has no `updated:` timestamp — run `dbmd fm init` or set RFC3339 manually |
| `FM_UNREADABLE` | error | content file can't be read (not valid UTF-8, or an I/O error) |
| `FM_MALFORMED_YAML` | error | frontmatter block isn't valid YAML |
| `FM_BAD_TIMESTAMP` | error | `created` or `updated` isn't ISO-8601 |
| `FM_BAD_META_TYPE` | error | a record's `meta-type` is not one of `fact` / `operational` / `conclusion` |
| `FM_BAD_ID` | warning | `id` is present but unusable as an identifier (non-scalar, empty, or contains whitespace); the recommended form is a lowercase ULID |
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
| `DUP_UNIQUE_KEY` | warning | two records of a type share a `DB.md ## Schemas` `unique:` key |
| `SCHEMA_MISSING_REQUIRED` | error | `DB.md` schema requires a field that's absent |
| `SCHEMA_SHAPE_MISMATCH` | error | value doesn't match the schema's shape modifier |
| `SCHEMA_LINK_PREFIX_MISMATCH` | error | `link to <prefix>/` field has a plain or wrong-prefix value |
| `SCHEMA_ENUM_VIOLATION` | error | value not in the schema's `enum` |
| `POLICY_FROZEN_PAGE` | error | write attempted on a `### Frozen pages` path (write-time) |
| `POLICY_IGNORED_TYPE_PRESENT` | info | a file with an `### Ignored types` type exists |
| `POLICY_IGNORED_TYPE_DERIVED` | warning | a `meta-type: conclusion` record derives from an ignored-type record |
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
| `ASSET_MANIFEST_MALFORMED` | error | a line in `assets.jsonl` isn't a valid asset record — run `dbmd assets scan` |
| `ASSET_UNDECLARED` | error | a content file's `asset`/`assets` path has no record in `assets.jsonl` — run `dbmd assets scan` |
| `ASSET_WRAPPER_BROKEN` | error | an `assets.jsonl` record names a wrapper file that doesn't exist |
| `ASSET_MANIFEST_ORPHAN` | warning | an `assets.jsonl` record's path is referenced by no wrapper |
| `ASSET_PATH_IS_CONTENT` | warning | an `asset`/`assets` path points at a tracked markdown content file |

The `ASSET_*` codes are checked by the full sweep (`dbmd validate --all`), not
the working set — like the `DUP_*` dedup codes — and are text-only: validation
never hashes a byte, so a fresh clone whose assets have not been restored still
passes. Byte presence and hash correctness are `dbmd assets verify`. See
[Assets](#assets).

v0.2 reworked the type-driven codes — it dropped the six type-specific
`DUP_*` collisions and `LAYER_TYPE_MISMATCH`, and added the generic
`DUP_UNIQUE_KEY`. v0.3 added `FM_BAD_META_TYPE` (the closed-enum check on
`meta-type`) and retargeted `POLICY_IGNORED_TYPE_DERIVED` from the retired
`wiki-page` type to `meta-type: conclusion` records. From v0.3 on the
vocabulary is additive (new codes layer on; existing codes keep their
meaning). v0.4 added `FM_BAD_ID` (warning — a structurally unusable
`id` never blocks validation, so v0.3 stores keep passing; see
[The `id` field](#the-id-field--stable-identity-recommended)).
Errors block; the agent resolves warnings and info at its
discretion — usually via `dbmd rename`, `dbmd link`, `dbmd fm set`, or
`dbmd index rebuild`.

**`DB.md` structure.** The store's `DB.md` is the identity file, so its
shape is checked directly (not as a content file — it carries no
`summary`). Its frontmatter MUST declare `type: db-md`
(`DB_MD_BAD_TYPE` otherwise, including when `type:` is absent or
malformed) and MUST carry both `scope` and `owner`
(`DB_MD_MISSING_FIELD`, one issue per absent field). Its body MAY
contain only the four recognized `##` sections — `Agent instructions`,
`Policies`, `Schemas`, `Folders` (optional display overrides for the
generated rollup `index.md` files); any other `##` heading is a likely typo or
misplacement and surfaces as `DB_MD_UNKNOWN_SECTION` (warning — the
parser ignores it, so it does not corrupt the config, but it signals
the operator wrote a section the toolkit will never read). Recognized
`###` sub-headings inside `Policies` / `Schemas` (e.g. `Frozen pages`,
`Ignored types`, a `### <type>` schema block) are not flagged.

## Why files

The database has been a service for decades — a daemon, a wire
protocol, a schema migration tool, an admin UI — because useful
software over data had to be built as database, backend, frontend. The
database held state, the backend encoded rules, and the frontend exposed
fixed views and actions.

Agents make another shape possible: markdown files with wiki-links, an
agent harness, and a generated surface. A modern computer can ripgrep a
million files in seconds. An LLM reads markdown directly. Git gives
curated plain-file layers a durable, inspectable history.

db.md inverts the shape:

- **The database is the directory.** No daemon, no port, no
  migration tool.
- **Structured fields are frontmatter.** Type-tagged, additive,
  optional; store-specific schemas live in `DB.md`.
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

Most software is not Google-scale. It is records, context,
relationships, workflows, decisions, policies, history, and a surface:
a trip planner, baby tracker, migraine log, reading system, CRM,
knowledge base, ops tracker, contract register, decision log, internal
admin panel, or SaaS product that is a database with a UI bolted on.
The old default was to put those records in Postgres, freeze a schema,
wrap it in an app, and migrate every time reality moved. db.md replaces
the database for that broad middle — and the app over it when the
surface is agent-built — because the agent reads and relates the records
directly and builds the surface on demand.

The genuinely hard remainder is real: high write concurrency, ACID
transactions, sub-millisecond reads, aggregates over billions of rows.
A real engine still earns its place there today, and that is where the
[roadmap](#roadmap) takes db.md next — the packed engine (SQLite-class,
projected through a VFS) under this same contract: the directory is the
database, the files are the source of truth. Until then the two compose
cleanly — write to both, treat db.md as the canonical, human-readable
layer. Postgres is for authoritative machinery. db.md is for living
context. The long bet still points one way: more of this territory moves
into agent-readable files, and never by adding vectors.

## Writers and readers

By design, db.md is **many-writer for `sources/` and for `fact` /
`operational` records, single-writer for `meta-type: conclusion`
records**. Anything can drop files into `sources/` (rsync, mbsync,
manual cp). Anything can append atomic facts to `records/` (the agent,
the operator via `dbmd write`, scripts). But conclusion records — the
synthesis the old `wiki/` layer held — have a single voice. One curator
agent reconciles them. Multiple agents writing conclusion records
concurrently is an anti-pattern.

**This is now a per-file, frontmatter-conditional rule — and that is a
real weakening from v0.2.** In v0.2 the single-writer boundary was the
`wiki/` *folder*: a path either was under `wiki/` or it wasn't, so the
boundary was mechanically checkable and a tool could refuse a write by
path. In v0.3 the synthesis lives *inside* the many-writer `records/`
folder, distinguished only by `meta-type: conclusion` in frontmatter. So
the single-voice contract is **prose, not a path check**: nothing in the
layout stops an external script that appends to `records/` from touching
a conclusion record, and `dbmd validate` does not enforce single-writer.
We accept this knowingly. The mitigant is that db.md's contract is
already **single-agent-per-store** (below): with one curator agent and
one writer in practice, the realistic blast radius is narrow — an
external script writing into `records/` is the only way to violate it,
and that script is the operator's own. (If a mechanical guard is wanted
later, the existing `### Frozen pages` machinery, or a `conclusion`-aware
write refusal, can reintroduce a path-independent lock; the spec does
not ship one today.)

Files dropped into `sources/` by an external tool join the catalog
when the agent next seeds them with `dbmd fm init` (write-through) or
folds the whole drop in with one `dbmd index rebuild`. Until then they
are on disk and findable by `dbmd search` (ripgrep doesn't need the
catalog), but not yet listed in `index.md`. The agent reconciles a
bulk drop once, not file-by-file in the loop.

**Single-agent-per-store is the standing contract.** db.md does not
coordinate multiple curator agents writing to the same store
concurrently. The operator runs one curator at a time. If multiple
agents need to operate, give each its own store (and link the
stores externally) or serialize via the operator's own tooling.
Multi-agent coordination — locks, leases, conflict resolution —
is out of scope today. This contract is also what keeps the
prose-only single-writer rule above tolerable in practice.

## Scale

db.md scales to **millions of files natively** — no embeddings index,
no vector store, no external catalog required. The store *is* the
database; the filesystem, embedded ripgrep, and a write-through
catalog are the engine. One rule makes this hold:

**The interactive loop is designed around O(changed), not O(store).** Every
operation the agent runs in its write loop — search, frontmatter
lookup, backlinks, the pre-write dedup checks, the per-write catalog
update, the post-write validate — should cost in proportion to what changed,
not to how large the store is. Whole-store passes exist (`dbmd
validate --all`, a full `dbmd index rebuild`, `dbmd stats`) but they
are repair/audit operations, off the interactive path.

Four properties deliver it:

- **Sources and event-type records are date-sharded; entity and
  conclusion records stay flat.** Raw evidence never changes after
  ingest, so the toolkit parses each source once and never again. When a
  conforming writer uses `dbmd write`, high-volume source and event
  types auto-partition by date (`sources/emails/2026/05/…`,
  `sources/notes/2026/05/…`, `records/expenses/2026/05/…`) so no
  directory holds an unbounded number of entries and only the current
  shard is ever "hot."
  **Sharding is a property of the type, not the layer or the
  meta-type:** event-driven types (`email`, `transcript`, `note`,
  `expense`, `invoice`, `meeting`, + custom event types) carry a primary
  date field and shard; dedup-bounded *entity* types (`contact`,
  `company`) stay flat because the entity set itself is bounded;
  `meta-type: conclusion` records stay flat (curation-bounded). This is
  what lets a company's event records — expenses, invoices, orders,
  which track business volume, not curation effort — scale the same way
  sources do. The type-folder catalog (`records/expenses/index.md`)
  aggregates across shards; the shards themselves are storage, not
  catalog levels.
- **Structured reads hit the `index.jsonl` sidecar; full-text reads
  are ripgrep.** `dbmd query`, `dbmd search
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
  (uncapped, complete, structured) for the affected type-folder, plus
  refresh parent counts. The current toolkit keeps `index.jsonl`
  compacted on write, so write cost is O(type-folder jsonl), not O(store);
  `dbmd index rebuild` is a from-scratch repair, not a per-change step.
  Both are plain files — derived, rebuildable, no engine.
- **The log rotates.** `log.md` is the active timeline; older months
  roll into `log/<YYYY-MM>.md`. `dbmd log tail` / `since` reverse-read
  from the end. The active log stays small regardless of store age.

**Performance budgets** (modern laptop). These are implementation targets. The
10k tier is measured by the default perf gate; the 1M tier is opt-in
(`#[ignore]`) and should be read as the scale target until explicitly run on a
given release. Sidecar read ops are flat in store size; sweep ops are linear and
run off the loop. Current write paths are O(type-folder jsonl) and documented as
marginally over the tight 100ms target at 10k in `tests/PERF.md`.

| Operation                              | Class           | 10k    | ~1M    |
|----------------------------------------|-----------------|--------|--------|
| `dbmd write` / `fm set` (+ catalog)    | loop            | target <100ms; measured near target | target <100ms |
| `dbmd query --where <k>=<v>`           | loop (ripgrep)  | <300ms | <2s    |
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

**How much data is this?** A single user indexing their entire Gmail
runs ~120 emails/day — roughly 44k files a year, ~440k over a decade,
~1–1.5M across a heavy career: comfortably inside the native sweet spot.
A shared operating store is larger — even ten people can cross a
million files within a few years, and a large org reaches hundreds of
millions to billions. The separated, file-per-record flavor with
ripgrep carries the individual and the small team; the packed flavor
and the engine (see [Roadmap](#roadmap)) carry the larger end.

**Worked examples ship under `examples/` and `tests/corpora/`.** The
examples are small, complete stores for research, operations, personal
software, agency knowledge, and customer data. The corpora are the
executable proof surface: canonical stores, edge cases, format fixtures,
scale generation, and the agent-eval expected output all exercise the
same contract a real agent-built tool or agentic computer uses.

**Two ceilings, not one.** The filesystem + ripgrep store reaches
millions, but **git over the raw store is the tighter limit**
(comfortable to ~100k files, tuning by ~500k, special tooling past
~1M). So git-as-audit-log is the individual / small-team property; very
large history is the packed flavor plus external snapshots — an agentic
computer on a managed VM has hourly / daily snapshots as its real audit
log. Sharding fixes per-directory growth, not the whole-tree-walk cost
that git and backups pay; the maintained engine index is what removes
that.

**Semantic recall without embeddings.** Lexical search looks like it
misses synonyms — but the agent driving `dbmd` is a language model, so
*it* supplies them: it expands a concept into its related terms
(`revenue → revenue, sales, income, ARR, top-line`) and runs them as one
search. `dbmd` stays a dumb lexical tool and computes nothing; the model
is the semantic layer, working over text the operator can read instead of
vectors they cannot. This is the whole semantic story: no vectors to
compute or store, now or ever, and nothing needed beyond what the
toolkit already ships.
(A maintained keyword index makes this a sublinear fast path at scale —
see the [Roadmap](#roadmap) — but it is a *lexical* index, never a vector
index; db.md adds no embeddings and no ANN.)

The files remain the source of truth. You *can* derive anything you
like on top — a SQLite catalog, a tantivy index, embeddings for some
other tool — but you do not *need* to for the separated plain-file
flavor's sweet spot. The packed engine is the path past that.

## Assets

Most db.md content is text and belongs in the store's version control. Raw
binary evidence does not: a signed PDF, a meeting recording, or a large export
can be far heavier than Git should carry, and git over the raw tree is the
store's tighter ceiling (see [Scale](#scale) — "two ceilings"). The asset layer
keeps that evidence in the store's logical model and out of the VCS path, with a
manifest that proves a local copy is complete.

This layer **tracks and verifies** assets; it does not move bytes. Uploading
evidence to durable storage and restoring it onto a fresh machine is a storage
layer built on top, keyed off the manifest's `sha256`. The contract db.md owns
is the portable proof: *these assets belong to this store, and this is how to
prove the local copy is complete.*

### Wrapper and asset

A **wrapper** is an ordinary content file that declares, in frontmatter, a
binary it is about. Usually a source under `sources/`, but a `records/` entry (a
receipt on a `fact` expense, or a `meta-type: conclusion` record citing a signed
plan) may declare one too — the asset layer spans both layers. The wrapper
carries the universal frontmatter and the agent-usable text (extracted text, a
transcript, notes); it is small and version-controlled like any other content
file.

A **raw asset** is the binary the wrapper is about, at a real store-relative
path so the working copy is genuinely complete and openable by any tool. It is
recorded in the manifest and kept out of the VCS path. An asset is never a
wiki-link target: wiki-links are edges between markdown nodes and a binary is
not a node, so assets never appear in `graph` / `rename` / `backlinks`.

Declare assets with an `asset:` (single) or `assets:` (list) frontmatter
key (the examples elide the universal `id` / `created` / `updated`
fields for brevity):

```yaml
---
type: pdf-source
summary: "Acme MSA, countersigned 2026-06-15"
asset: sources/docs/2026/06/acme-msa.pdf
---
```

```yaml
---
type: recording
summary: "Kickoff call; transcript in records/meetings"
assets:
  - sources/recordings/2026/06/kickoff.mp4
  - { path: sources/recordings/2026/06/kickoff.vtt, required: false }
---
```

A bare path is required by default. The object form `{ path, required }` marks
an asset optional — a regenerable or nice-to-have artifact the store does not
need to be byte-complete. The manager writes these keys on ingest; operators
don't write frontmatter by hand.

### The `assets.jsonl` manifest

A single root-level file, one JSON object per asset — the asset analog of the
type-folder `index.jsonl`: a derived, write-through, rebuildable plain file
(JSONL, so it stays git-diffable line-by-line and ripgrep-able), not a database.
Every field is derivable from the wrappers plus the files on disk, so a scan
where the bytes are present reproduces it byte-for-byte.

```json
{"path":"sources/docs/2026/06/acme-msa.pdf","sha256":"9f2c4e…","bytes":12483910,"media_type":"application/pdf","wrappers":["sources/docs/2026/06/acme-msa.pdf.md"],"required":true}
```

- `path` — store-relative, forward-slash, with extension. The record key. Two
  records may share a `sha256` (identical bytes at two paths); the record keys
  on path, a storage layer dedupes on hash.
- `sha256` — lowercase-hex SHA-256 of the bytes: the integrity check, and the
  stable key a storage layer addresses the blob by.
- `bytes` — size.
- `media_type` — best-effort MIME from the extension (deterministic, so it does
  not break rebuild equivalence).
- `wrappers` — the content file(s) that declare the asset, sorted; usually one.
- `required` — whether the asset is needed for byte-completeness (default
  `true`; `false` only when every declaration marks it optional).

The manifest records **no** local-presence flag (that is machine state, computed
on demand) and **no** storage location or provider URI (a storage layer derives
that from the `sha256`). Those omissions are deliberate: the manifest stays
portable and provider-agnostic, and its Git diff stays stable across machines.
Records are sorted by path; on a concurrent-clone merge, set `assets.jsonl
merge=union` in `.gitattributes` (the same floor `log.md` uses) and let a later
`dbmd assets scan` recompact.

### Keeping bytes out of the VCS

Assets are recorded in the manifest and excluded from version control. **db.md
does not write a `.gitignore` and never runs git** — a store may be carried by
Git or by a sync service, so the toolkit stays VCS-neutral. `dbmd assets paths`
prints the asset paths, one per line, for whatever ignore mechanism the store
uses; maintaining the ignore list — and, for Git, ensuring no asset was
committed before it was ignored (a tracked binary stays in history) — is the
operator's or harness's job, not the format's.

### Path safety

Every declared and recorded asset path must be store-relative, forward-slash,
with no absolute prefix and no `..` component, and must resolve under the store
root. `dbmd` enforces this wherever it reads the manifest, so a malformed or
hostile declaration can never make a scan hash, or a restore write, a file
outside the store.

### Operations

`dbmd assets` has four leaves; none runs git or touches the network:

- `dbmd assets scan` — read every content file's `asset`/`assets`, hash the
  present files, and rewrite `assets.jsonl`. The manifest is a projection of the
  declarations: a path no longer declared drops out, and a path whose bytes are
  absent but were previously cataloged is preserved (the eviction case — it
  cannot be re-hashed). Scan is the from-scratch and bulk-drop reconciliation,
  the asset analog of `index rebuild`. Scanning needs the bytes present (to
  hash); `status`/`verify` read the committed hashes and work without them.
- `dbmd assets verify` — the byte-completeness gate: every required asset (plus
  optional under `--include-optional`) is present locally and matches the
  manifest. `--quick` checks presence and size; the default re-hashes. Exits
  non-zero when anything is missing or corrupt. A SWEEP, not a loop op.
- `dbmd assets status` — a non-failing report of present / missing and bytes to
  restore.
- `dbmd assets paths` — the path list above.

### Validation

`dbmd validate --all` cross-checks the manifest against wrapper declarations as
part of the full sweep, the same way it checks entity dedup and index sync. The
check is text-only — it never hashes a byte or reads an asset's contents — so a
fresh clone whose bytes have not been restored still passes `validate`. Byte
presence and hash correctness are `dbmd assets verify`, not `validate`. The
codes are `ASSET_MANIFEST_MALFORMED`, `ASSET_UNDECLARED`, `ASSET_WRAPPER_BROKEN`,
`ASSET_MANIFEST_ORPHAN`, and `ASSET_PATH_IS_CONTENT` (see
[Validation](#validation)).

## Roadmap

The format is deliberately the simplest useful separated flavor: plain
files, YAML frontmatter, wiki-links, embedded ripgrep, two folders + a
`meta-type` field. No daemon, no engine, no magic. It is built for the
low millions of files; the packed engine is the path beyond the point
where literal directories, whole-tree walks, or git over the raw store
stop being the right physical shape.

Where db.md is going — additively, without breaking the "it's just
files" contract or the format you read today:

- **An agent-native on-disk representation — in two flavors.** The
  same logical format and the same contract, two physical encodings a
  store can take:
  - **Separated** — plain markdown files on disk (Obsidian-compatible,
    git-diffable, maximal interop) plus an adjacent index sidecar
    holding the compiled view: typed frontmatter, wiki-link edges,
    content hash, summary. **v0.2 already ships the nascent form — the
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
  full-text index — all lexical, no vectors), not linear scans. `query`, `search`, and `backlinks` become
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
markdown vault becomes a db.md store — an agent-built internal tool, an
Obsidian vault, a research wiki, a customer database, an agentic
computer keeping its operating store at `~/db/`, any harness with a
folder of markdown. No platform, no account, no hosted service
required. The spec is the contract; the runtime is replaceable.

## Tooling

The format is the spec. The reference toolkit is one Rust binary,
`dbmd`, with subcommands for read / write / validate / extract
operations. Embeds `ripgrep` (via the `grep` crate) for fast search.
**Zero LLM dependencies**: no provider SDKs, no API keys, no model
calls anywhere in the binary. The agent runtime — Claude Code,
Codex, or any harness — is BYO and calls `dbmd` for file/data
operations. See `TOOLS.md` for the full toolkit reference.

**Agent bootstrap — the installer is text.** db.md is installed and
integrated by reading plain markdown and acting on it; a capable agent is
the installer. Two layers, both reachable as text — the repo-root
`llms.txt` is the agent-readable entry point:

```bash
# 1 — get the binary (one ~6MB binary, no toolchain; brew or cargo also work)
curl -fsSL https://raw.githubusercontent.com/carloslfu/db.md/main/scripts/install.sh | sh

# 2 — load the contract (the single source of truth)
dbmd spec
```

`dbmd spec` prints this document — the format, the curator contract, the
session lifecycle, the validation codes, and the full subcommand surface. An
agent that has read it can operate any db.md store immediately; per-store
overrides come from the store's `DB.md` on every operation.

**Make it persistent (optional).** To have your agent reach for db.md
automatically on every future session, place a skill where your harness reads
skills — the open [Agent Skills](https://agentskills.io) format (a
`db-md/SKILL.md` folder with `name`/`description` frontmatter whose body is a
thin pointer that runs `dbmd spec`). The canonical skill file ships in the repo
at `skills/db-md/SKILL.md`:

- Claude Code → `~/.claude/skills/db-md/SKILL.md`
- Codex → `~/.codex/skills/db-md/SKILL.md`
- Any other harness → its own skills directory, or load `dbmd spec` into the
  system prompt.

Placing it is generic file work, not a db.md command: copy the file, use your
harness's own skill installer (Codex's `skill-installer`, a Claude Code plugin),
or just tell your agent to set itself up from this contract. db.md ships no
per-harness installer — the mechanism is generic text and a capable model. The
skill never copies the SPEC (it points at `dbmd spec`), so it cannot drift.

**Subcommand map** (grouped by session phase; full reference in
`TOOLS.md`). Every subcommand supports `--json` and `--help`; none
prompt interactively.

| Phase     | Subcommands |
|-----------|-------------|
| Open      | `dbmd spec`, `dbmd fm get DB.md <key>` |
| Warm up   | `dbmd log tail [N]`, `dbmd log since <ts>` |
| Read      | `dbmd search <q> [--type --in --where --linked-from --linked-to --updated-after --updated-before]`, `dbmd query [--type --in --where --updated-after --updated-before --created-after --created-before --limit]` (frontmatter filter over the sidecar; paths by default, `--json` = full records — the dedup/`--where` lookup folds in the former `fm query`, `--json` the former `index query`), `dbmd fm get <file> <key>`, `dbmd graph <backlinks\|forwardlinks\|neighborhood\|orphans>`, `dbmd tree`, `dbmd outline <file>`, `dbmd stats`, `dbmd extract <file>`, `dbmd index show [<path>]` |
| Write     | `dbmd write <path> --type <t> [--summary --fm --body-file]`, `dbmd fm set <file> <k>=<v>`, `dbmd fm init <file>`, `dbmd link <from> <to>`, `dbmd rename <old> <new>`, `dbmd format <file>` |
| Validate  | `dbmd validate [--json]` (working set), `dbmd validate --all` (full sweep) |
| Maintain  | indexes are write-through; `dbmd index rebuild [--layer --folder --dry-run]` repairs / folds in bulk drops |
| Close     | `dbmd log <kind> <object> [-m <note>]` |

## Versioning

**The format and the toolkit version independently.** This document
carries the format version (`v0.1` … `v0.4` — the number in the title);
the `dbmd` toolkit carries its own semver in `Cargo.toml`, and **repo
tags track toolkit releases, not format versions** — tag `v0.4.0` was a
toolkit release that implemented format v0.3, and format v0.4 shipped
in toolkit 0.6.0. The [CHANGELOG](CHANGELOG.md) records both axes and
names, for every toolkit release, the format version it implements.

v0.2 generalized the type model (schema enforcement is solely the
store's `## Schemas`; the example types are illustrative) and reworked
the type-driven validation codes.

**v0.3 is a breaking change** — the first since v0.2. It collapsed the
three-folder layout to two (`sources/` + `records/`), removed the
`wiki/` layer, and moved its synthesis role onto the closed-enum
`meta-type` field (`fact` / `operational` / `conclusion`) inside
`records/`. A v0.2 store does not validate unchanged against a v0.3
toolkit: `wiki/` is no longer a recognized layer, and `type: wiki-page`
is retired. **From v0.3 forward, changes are additive again** — old v0.3
stores stay readable, new fields and codes layer on top, and tools that
don't recognize them ignore them.

**v0.4 is additive** — the first release under that rule. Two changes,
both retrofit-expensive to skip and cheap to carry: the universal
frontmatter contract gains a RECOMMENDED stable record `id` (a
lowercase ULID; `dbmd write` mints one when absent; a record without
one stays fully valid, and filename identity remains the link
fallback — see
[The `id` field](#the-id-field--stable-identity-recommended)), and one
paragraph reserves the `@brain/id` cross-store address shape without
defining resolution (see
[Addressing (reserved)](#addressing-reserved)). One validation code is
added (`FM_BAD_ID`, warning). A v0.3 store validates unchanged under a
v0.4-conformant toolkit.

**There is no migration command.** db.md is plumbing, not a scaffolder;
migrating a v0.2 store is an agent's job, not a `dbmd` verb. A capable
agent with this SPEC in context performs the in-place migration:

- Move every `wiki/<topic>/*` file into `records/<type>/*`, assigning a
  real `type` by topic (`people → profile`, `playbooks → playbook`,
  `themes → theme`, `synthesis → synthesis`, concept pages → `concept`,
  accounts → `account`) and setting `meta-type: conclusion` on each.
- Leave `sources/` and `fact`/`operational` records as they are
  (`meta-type` absent reads as `fact`); optionally annotate them
  explicitly.
- Update the store's `DB.md` — drop `wiki/` from any `### Frozen pages` /
  schema paths, repoint them at `records/`.
- Run `dbmd index rebuild` once to regenerate the catalog at the new
  paths, then `dbmd validate --all`.

The quick-start prompt that ships with the repo drives exactly this
sequence; point an agent at it and the store migrates itself.

## License

This spec is Apache-2.0. The reference tooling (`crates/dbmd-core`,
`crates/dbmd-cli`) is Apache-2.0. Examples are Apache-2.0.

Anyone can build tools that read or write db.md. The format is open.
