# TodoMVC on db.md

A proof of concept: full-parity [TodoMVC](https://todomvc.com) (React 19 +
TypeScript + Vite) whose database is a **db.md store** — and whose backend
is **no code at all**. Every todo is one markdown record in
[`db/records/todos/`](db/records/todos/); the page talks straight to
`dbmd api`, the db.md toolkit's own local app server, which executes the
same `dbmd` verbs an agent runs in a shell. The app is one client of the
store; an agent (or you, in a text editor) is another, on equal footing.

The page is **one store, six views** — the classic todo list is just the
first lens, and the others are the store showing through: full-text
**search** (embedded ripgrep), a **checklist** rollup of every `- [ ]`
line across record bodies, the store's own **activity** timeline
(log.md), an **ask** view (natural language over the same files — the
toolkit's embedded harness running YOUR model, its only tools the dbmd
verbs the other views mirror), and a **store** view (live health,
stats, and the schema the UI itself is built from). Each view ends with
the CLI one-liner it mirrors, because every view is the same-named dbmd
verb — and in the ask view, every tool call the model makes renders as
the exact one-liner it executed.

```
┌─────────────────────────────────────────────────────────┐
│  React components (classic TodoMVC, todomvc-app-css)    │
│  views: todos · search · checklist · activity · ask ·  │
│         store                                           │
│      └── useTodos() — reducer + optimistic dispatch     │
│      └── apiClient.ts — every operation as fetch()      │
│               │  same action vocabulary as the official │
│               │  React TodoMVC (ADD_ITEM, TOGGLE_ALL…)  │
├───────────────┼─────────────────────────────────────────┤
│     HTTP JSON + SSE (CORS, http://127.0.0.1:3261)       │
├───────────────┼─────────────────────────────────────────┤
│  dbmd api — the store's OWN app server (no app code):   │
│  every route executes the same-named dbmd verb, with    │
│  the store's schema checks, frozen-page policy, locks,  │
│  and write-through indexes; /v1/events streams the      │
│  change feed (dbmd watch) as SSE → UI refetch           │
├─────────────────────────────────────────────────────────┤
│  db/ — the database: a plain-file db.md store           │
│      DB.md (contract + ### todo schema)                 │
│      records/todos/*.md (one record per todo)           │
│      index.jsonl / index.md / log.md (dbmd-maintained)  │
│           ▲                                             │
│           └── any other writer: dbmd CLI, an agent,     │
│               your editor — the change feed carries     │
│               their edits into the open app live        │
└─────────────────────────────────────────────────────────┘
```

The analogy: `dbmd` is to a `db/` store what `sqlite3` is to a `.db` file —
except the file format is human-readable markdown, the schema is declared in
prose-adjacent YAML inside the store itself, any agent with a shell can
operate it natively, and `dbmd api` serves that whole surface to
applications over loopback HTTP. Vite serves static files and, in dev,
spawns `dbmd api` beside itself; there is no `server/` directory in this
demo.

## Run it

```bash
npm install
npm run dev        # http://localhost:5183
```

Requires the `dbmd` binary, 0.11.0+ for all six views (0.10.0 runs
everything but ask; `~/.dbmd/bin` and PATH are probed; set
`DBMD_BIN` to override) — `npm run dev` starts vite and spawns
`dbmd api --dir db` on `127.0.0.1:3261` beside it. The store in `db/` is
committed ready-to-run with seed todos. To run the built page with no vite
at all: `dbmd api --dir db`, then serve `dist/` from any static host (the
page finds the API at `127.0.0.1:3261`, or wherever `?api=` points it).

While the app is open, try operating the database from a terminal:

```bash
cd db
dbmd write records/todos/from-the-cli.md --type todo \
  --summary "Written from the CLI" --fm status=active --fm meta-type=operational
dbmd fm set records/todos/from-the-cli.md status=completed
dbmd query --type todo --where status=active
dbmd validate --all
dbmd log tail
```

Each write shows up in the open app without a reload — the server watches the
store directory and pings the browser over SSE.

## The ask view (the embedded harness)

The **ask** view needs no install beyond `dbmd` itself: `npm run dev` starts
`dbmd api` with `--ask --do`, and the toolkit's embedded harness (`dbmd ask` /
`dbmd do`, toolkit 0.11+) runs whatever model you point it at — the tools are
the same dbmd verbs as every other view, so `ask` is read-only by
construction and `do`'s writes ride the full store contract (schema checks,
link-aware deletes, the transaction lock, the activity log).

Model config, in order of ceremony:

```bash
# zero config — a running Ollama / LM Studio / llama.cpp is autodetected
ollama pull qwen3:8b   # any tool-calling model

# or a cloud preset + its usual key env var
DBMD_LLM_PROVIDER=anthropic ANTHROPIC_API_KEY=… npm run dev
DBMD_LLM_PROVIDER=openrouter OPENROUTER_API_KEY=… DBMD_LLM_MODEL=… npm run dev

# or a logged-in vendor CLI (experimental delegation; no key at all)
DBMD_LLM_PROVIDER=claude-code npm run dev
```

Non-secret knobs (`llm_provider`, `llm_model`, `llm_base_url`,
`llm_protocol`) can live in `db/.dbmd/config` instead; the key is
environment-only, always. The same conversation works from a shell —
`dbmd ask "what's overdue?"` in `db/` — and over HTTP for any app
(`POST /v1/ask` / `/v1/do`, SSE; each opt-in on `dbmd api`).

## Local agent companion (`./pi`)

Where the ask view proves the toolkit is *complete* (one binary, end to
end), `./pi` proves the habitat is *open*: any external agent moves into the
store natively, with its own tools, and the app follows along live. The two
are not competing. The embedded harness is deliberately minimal — dbmd verbs
only, no shell — so it answers and edits the store; pi has bash, read, edit,
and write, so it can do the things a real coding agent does. The demo
ships a fully local replacement for `codex exec`-style store
operation: the [pi coding agent](https://github.com/earendil-works/pi)
(`@earendil-works/pi-coding-agent`, read/bash/edit/write tools) driving
Ollama's Qwen3.8-27B at high reasoning effort, with the store as its working
directory. Current pi releases need Node 22.19 or newer, so `package.json`
pins this folder to Node 24.18.0 through Volta; without Volta, use any Node
22.19+ here. pi's older `legacy-node20` line has known security advisories.

```bash
./pi "change todo Taste db.md to 'taste db.md POWER 56'"   # one-shot, like codex exec
./pi                                                       # interactive session
PI_THINKING=low ./pi "quick task"                          # effort: low | medium | high (default high)
PI_RAW=1 ./pi "task"                                       # raw JSON event stream
```

One-shot runs show the whole process live — the model's thinking (dim), every
tool call with its command (cyan), each command's output, then the final
answer — by piping pi's `--mode json` event stream through
[`.pi/render.mjs`](.pi/render.mjs).

No cloud, no API key. Everything is repo-local: `.pi/agent/models.json` wires
pi to Ollama's OpenAI-compatible endpoint (`reasoning_effort` maps to the
model's thinking levels; `high` lands on its top `xhigh` tier),
`.pi/agent/APPEND_SYSTEM.md` appends the dbmd playbook to pi's system prompt,
and `.pi/agent/Modelfile` defines the `qwen3.8-27b-32k` alias (32k context +
Qwen sampling params) that the wrapper creates in Ollama on first run. The
wrapper runs the agent from `db/`, so its bash calls operate the store
directly — and the open app follows along live. Requires `ollama` with
`hf.co/unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M` pulled, plus `npm install` for the
pi dependency.

## The data model

TodoMVC's entire model maps 1:1 onto db.md's **universal frontmatter
contract** — no custom fields were needed beyond what the standard already
defines:

| TodoMVC concept | db.md field | Notes |
| --- | --- | --- |
| id | `id` | lowercase ULID, minted by `dbmd write` — time-sortable |
| title | `summary` | the canonical one-line "what this file is" field |
| completed | `status` | `active` \| `completed`, enum-enforced by the schema |
| insertion order | `created` | RFC3339 with µs precision; ULID breaks ties |

A todo record in full:

```markdown
---
type: todo
meta-type: operational
id: 01m134mswwsd682ztz60h5qh67
created: 2026-08-28T02:54:26.716801+00:00
updated: 2026-08-28T02:54:47.353536+00:00
summary: Taste db.md
status: completed
---
```

The **record body** below the frontmatter is the todo's long description —
plain markdown (notes, checklists), empty when unneeded. The ❯ chevron on
each row (dark when a description exists) expands a **WYSIWYG markdown
editor** (Milkdown Crepe — ProseMirror + remark, chosen because its
internal model is markdown-isomorphic, so the file stays the truth with no
lossy conversion). Checkbox clicks, checklist drag-reordering via the block
handle, and typing all funnel into one markdown stream, debounced into
saves (600ms, flushed on blur); the serializer is pinned to `-` bullets so
editor saves don't churn agent-written lines. External body edits stream in
live when the editor is unfocused and are deferred-then-applied on blur
when it is (local edits win conflicts, last-writer-wins). On save the page
PUTs `/v1/body` — `dbmd body set` stores the bytes verbatim, re-stamps
`updated`, and updates the type-folder index write-through.

**Reordering** is drag-and-drop at both levels. Todo rows drag by the grip
that appears on hover (dnd-kit, keyboard-accessible), persisted as an
integer `order` rank in frontmatter — gaps of 100, midpoint insertion so a
drop usually rewrites only the moved record, renormalizing when a gap
closes. Records without `order` sort last by `created`, so agent-created
todos land at the end exactly like classic TodoMVC, and an agent reorders
with `dbmd fm set <file> order=<int>`. Checklist items inside a
description drag with Crepe's block handle and land as reordered `- [ ]`
lines in the record body.

Todos carry `meta-type: operational` — they are operating state the app
mutates, per the spec's fact/operational/conclusion split. The store is
records-only (no `sources/` layer): app state has no evidence documents.

The schema lives in the store, not in app code — [`db/DB.md`](db/DB.md)
declares:

```markdown
### todo
- status (required, enum: active, completed)
- priority (enum: low, medium, high)
- order (int)
```

and `dbmd validate --all` enforces it (a bogus `status` fails with
`SCHEMA_ENUM_VIOLATION`, exit 6). The same `DB.md` carries agent
instructions, so a store-aware agent knows how to operate the todos without
reading this app's source at all.

**The schema drives the UI.** The app reads `GET /v1/schema` live and
renders a cycling color-dot control on each row for every enum field the
store declares (besides `status`, which the checkbox owns) — that's where
the priority picker comes from. Add another enum field to `DB.md` and a
control appears with zero app changes; the store view re-reads the schema
the moment `DB.md` changes.

**Wiki-links are enforced edges.** A description can reference another
todo with a store-root wiki-link (`[[records/todos/write-the-readme]]`).
The expanded panel shows both directions as clickable chips ("links to" /
"linked from", derived client-side from the link edges the emit already
carries), and `dbmd rm` refuses to delete a record that others still link
to — the app turns that 409 (`RM_LINKED`, backlinks in the error details)
into a confirm with an explicit "delete anyway" (`force=1`), after which
the store view's health check reports the broken link it caused.

## Six views over one store

| View | What it renders | The same thing in a shell |
| --- | --- | --- |
| **todos** | The classic list — plus schema-driven priority dots and link chips | `dbmd query --type todo` |
| **search** | Full-text regex search with highlights, grouped per todo; smart case (`(?i)` added to all-lowercase queries); alternation like `(milk\|demo)` is the query-expansion path | `dbmd search '(?i)milk' --type todo` |
| **checklist** | Every `- [ ]` line across every record body, grouped by todo; ticking a box rewrites that exact line in the body | plain `- [ ]` lines in the files |
| **activity** | log.md rendered newest-first — this page's actions, an agent's CLI writes, and manual `dbmd log` notes interleaved on one timeline | `dbmd log tail` |
| **ask** | Natural language over the store: the embedded harness streams its loop — each tool call as the dbmd one-liner it executes, then the answer. Read-only by default; “allow changes” switches to the write registry, and every mutation rides the store contract and lands on the activity timeline | `dbmd ask '…'` · `dbmd do '…'` |
| **store** | The store about itself: server + version, live `validate --all` health (green check or issue rows with the store's own suggestions), stats, and the declared schema | `dbmd validate --all` · `dbmd stats` · `dbmd schema` |

Views are hash-routed like the classic filters (`#/search`, `#/checklist`,
`#/activity`, `#/store`; the filters stay `#/`, `#/active`, `#/completed`
inside the todos view). Any view can point back at a todo — a search hit,
a checklist group, a log row, a link chip — which jumps to the list,
scrolls the row into view, opens its description, and flashes it once.

## Operation mapping

| UI action | `dbmd api` call | verb it executes |
| --- | --- | --- |
| load / reconcile | `GET /v1/emit` | `dbmd emit --json` — every record with frontmatter AND body in one request |
| add | `POST /v1/write` | `dbmd write records/todos/<slug>.md --type todo --summary … --fm status=active --fm meta-type=operational`, then `GET /v1/fm?key=id` for the minted ULID |
| rename (edit) | `PUT /v1/fm?file=…&key=summary` | `dbmd fm set <path> summary=…` |
| toggle | `PUT /v1/fm?file=…&key=status` | `dbmd fm set <path> status=completed\|active` |
| edit description | `PUT /v1/body?file=…` | `dbmd body set <path>` — verbatim bytes, `updated` re-stamp, index write-through |
| delete | `DELETE /v1/rm?path=…` | `dbmd rm <path>` — link-aware, catalog row dropped write-through |
| toggle all | `PUT /v1/fm` per differing record | `fm set` each |
| set priority (any schema enum field) | `PUT /v1/fm?…&key=priority` | `dbmd fm set <path> priority=<value>` |
| reorder | `PUT /v1/fm?…&key=order` | `dbmd fm set <path> order=<int>` (materializes ranks on first use; renormalizes when gaps close) |
| clear completed | `DELETE /v1/rm` each | `dbmd rm` each — still-linked records refuse and are reported as kept |
| delete a linked todo | `DELETE /v1/rm?…&force=1` after the 409 confirm | `dbmd rm <path> --force` |
| search | `GET /v1/search?q=…&type=todo` | `dbmd search '<regex>' --type todo` (embedded ripgrep) |
| activity | `GET /v1/log/tail?n=200` | `dbmd log tail 200` |
| health | `GET /v1/validate?all=1` | `dbmd validate --all` (a failing store answers 422 — with the full report as the body) |
| stats / schema | `GET /v1/stats` · `GET /v1/schema` | `dbmd stats` · `dbmd schema` |
| live updates | `GET /v1/events` (SSE) | `dbmd watch --json`, each event re-framed as an SSE `data:` line → structured `{event, path}` batches → selective refetch |

Every mutation also appends to the store's own timeline
(`POST /v1/log` → `dbmd log <create|update|delete> <object> -m <note>`) —
`dbmd log tail` reads back the session history.

## Design notes

- **There is no backend.** Browsers can't spawn binaries or touch the
  filesystem — so the store serves itself: `dbmd api` is the db.md
  toolkit's own app server, and every route executes the same-named verb
  with the store's full contract (schema enforcement, frozen pages, locks,
  write-through indexes). This demo ships zero server code; delete the app
  and the database is still fully legible and operable.
- **One writer at a time.** Concurrent mutations serialize on the store's
  own cross-process transaction lock — the same flock every `dbmd`
  invocation takes, so the page, an agent, and your editor can all write at
  once safely. Every mutation response reconciles from a fresh canonical
  list.
- **The hook mirrors the official React TodoMVC.** `useTodos()` is
  `useReducer` with the official action vocabulary (`ADD_ITEM`,
  `UPDATE_ITEM`, `TOGGLE_ITEM`, `TOGGLE_ALL`, `REMOVE_ITEM`,
  `REMOVE_COMPLETED_ITEMS`). Actions apply optimistically, persist in
  dispatch order, and a `SYNC` action reconciles from the server's canonical
  list once nothing is in flight. New todos get a temp id that is remapped to
  the dbmd-minted ULID when the write returns.
- **Filename ≠ identity.** Files get a readable slug at birth
  (`buy-oat-milk.md`); identity is the ULID `id`, so retitling a todo never
  renames the file. Path collisions retry with a numeric suffix (`dbmd write`
  refuses collisions with exit 5 rather than overwriting).
- **External writers are first-class.** Any change to the store — agent,
  CLI, editor — reaches open browsers through the store's own change feed
  (`/v1/events`, i.e. `dbmd watch` over SSE) within about a second. Delete
  records with `dbmd rm` (link-aware, index maintained); if you bypass
  `dbmd` with a raw `rm`, run `dbmd index rebuild` afterward.
- **The feed is structured, and consumed selectively.** Events carry
  `{event: created|modified|removed, path}`; the client batches them and
  each consumer refetches only for the paths it cares about (the list for
  `records/*` and `DB.md`, the schema for `DB.md`, the store view for
  anything). `dbmd watch` covers the semantic store — DB.md + records —
  not log.md itself, so the activity view refreshes on any batch (log
  entries co-travel with record writes) plus a slow poll while it's open,
  which catches a bare `dbmd log` append too.
- **One stream per page, released when the tab is backgrounded.** SSE over
  HTTP/1.1 competes for Chrome's six-per-host connection budget, and a
  quiet stream is never noticed dead by either side. The client closes its
  EventSource on `pagehide` and when a tab is backgrounded, reopens with a
  full resync on return, and (in dev) closes the old stream when vite
  hot-replaces the module — leaked streams from stale tabs otherwise
  starve every fetch to the API. It always opens the stream on subscribe,
  including for a document that is *already* hidden when it renders:
  `visibilitychange` fires only on a transition, so gating the open on
  visibility left background-loaded pages with no stream, no resync, and
  no offline banner — live-looking and frozen.
- **Unreachable ≠ broken.** If the API can't be reached, a banner says so
  and the page keeps rendering its last-known state; the EventSource
  doubles as the liveness probe and everything resyncs on reconnect.

## Verified behaviors (TodoMVC spec parity)

Adding (Enter, trimmed, input cleared) · toggling · toggle-all in both
directions with correct checked state · double-click to edit with focus and
caret at end · Enter/blur commits · Escape cancels · committing empty text
destroys the todo · destroy button on hover · counter with singular/plural
("1 item left" / "2 items left") · Clear completed shown only when something
is completed · `#/`, `#/active`, `#/completed` hash routing with selected
highlighting · main and footer hidden when the list is empty · persistence
across reload and server restarts (it's files) — plus the db.md extras: live
external edits over SSE, `dbmd validate --all` green after every operation,
and the store log as an audit trail.

And the view extras, each verified end-to-end in a real browser: priority
dots cycling and landing in frontmatter · link chips in both directions ·
`RM_LINKED` refusal → "delete anyway" → the broken link surfacing in the
store view's health, then healing live when the record returns · search
with smart case, regex alternation, per-line highlights, and jump-to-todo ·
checklist ticks rewriting the exact body line · CLI log entries appearing
on the open activity timeline · the offline banner appearing when the
server dies and clearing on its own when it returns.
