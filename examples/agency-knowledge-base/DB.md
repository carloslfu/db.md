---
type: db-md
scope: agency
owner: Dana Forrester
computer_id: northshore-studio
---

# Northshore Studio agency knowledge base

Multi-client knowledge base for a 12-person creative agency. The
defining constraint is **client confidentiality**: Lumio must never see
Brightmore's work, Brightmore must never see Riverkeep's, and so on.

This store is organized by **type**, the db.md grain — not by a folder
per client. Each client gets one `client` record; everything about that
client (contacts, projects, meetings) carries a `client` link field plus
a per-client tag. Confidentiality is then a **curator discipline plus a
query filter**, not a sealed folder: to audit or extract one client, you
filter by its `client` link and walk its backlinks.

The two layers:

- `sources/` — raw evidence pulled in from outside: client meeting
  transcripts and client emails, preserved verbatim, each tagged to its
  client.
- `records/` — everything the agent authors. Atomic typed data
  (`client`, `contact`, `meeting` — `meta-type: fact`), the live state
  of each engagement (`project` — `meta-type: operational`), and the
  agency's cross-client synthesis (`playbook` — `meta-type: conclusion`).

## Agent instructions

You curate Northshore Studio's agency knowledge base. The defining
constraint is **client confidentiality.**

### The type-folder model

Maintain canonical db.md type-folders — never a folder-per-client tree:

- `records/clients/` — one `client` record per client (Lumio,
  Brightmore Group, Riverkeep). Carries `voice` (the client's tone) and
  `primary_contact`. This is `meta-type: fact`.
- `records/contacts/` — one `contact` per person, each with a `client`
  link field and a per-client tag (`#lumio`, `#brightmore`,
  `#riverkeep`). `meta-type: fact`.
- `records/projects/` — one `project` per active engagement, carrying
  `client`, `next_step`, and an `items` checklist you keep current. This
  is **operational** state (`meta-type: operational`) — the body is the
  project's current status, rewritten as it moves.
- `records/meetings/` — one `meeting` per client conversation, linked to
  its `client`, its `attendees`, and the `project` it advanced. Lead the
  body with the next step. `meta-type: fact`.
- `records/playbooks/` — agency-wide patterns, abstracted across clients
  (`meta-type: conclusion`). These NEVER name a specific client.

### Processing sources

- When a client meeting transcript lands in
  `sources/transcripts/<YYYY>/<MM>/`, create a `meeting` record in
  `records/meetings/` with attendees, decisions, and the next step. Link
  it to its `client`, its `attendees`, and the `project` it advanced, and
  link back to the transcript it was derived from.
- When a client email lands in `sources/emails/<YYYY>/<MM>/`, extract
  what it changes: update `last_touch` on the relevant `contact`, advance
  the `project`'s `next_step` and `items`, and — if it requests work
  beyond the SOW — apply the scope-creep playbook
  [[records/playbooks/scope-creep-handling]] and surface the delta.
- Keep `summary` fields one line and current. Refresh a contact's summary
  when its role changes; refresh a project's body when its state moves.

### Confidentiality discipline

- Every client-specific record (`contact`, `project`, `meeting`) carries
  a `client` link and a per-client tag. No exceptions.
- **Never cross-link a record between two clients.** A Lumio meeting
  never links a Brightmore contact; a Brightmore project never links a
  Riverkeep deliverable. The `client` link is the wall.
- To audit or extract everything for one client, **filter by its
  `client` link** — don't reach for a folder:
  - `dbmd query --type meeting --where client="[[records/clients/lumio]]"`
  - `dbmd graph backlinks records/clients/lumio.md`
- The **only** cross-client layer is `records/playbooks/`, and those stay
  anonymized — pattern plus an abstracted example, never a client name.

### Voice

Match the client's voice in any draft you produce. Each `client` record
carries a `voice` note (casual for the fintech startup, formal and
brand-guideline-bound for the enterprise retailer, mission-driven for the
nonprofit). Read it before drafting copy, emails, or deliverables for that
client.

### What you don't do

- Don't auto-deliver assets. Deliveries go through Dana's signoff.
- Don't infer a scope change unilaterally — surface it with a quantified
  delta and let the client choose the path (see the playbook).

## Policies

### Frozen pages
- `records/playbooks/scope-creep-handling.md` — signed off by Dana; the
  canonical scope process. Do not modify.

### Ignored types
- `test`, `temp` — read as ambient context but never synthesized into
  records or `meta-type: conclusion` playbooks.

## Schemas

### client
- name (required, string)
- industry (required, string)
- voice (required, string)
- engagement (enum: retainer, project)
- primary_contact (link to records/contacts/)
- unique: name
- summary_template: {industry} client on a {engagement} ({voice})

### contact
- name (required, string)
- email (required, email)
- client (required, link to records/clients/)
- role (string)
- first_touch (date)
- last_touch (date)
- unique: email

### project
- client (required, link to records/clients/)
- status (enum: active, on-hold, delivered, archived)
- next_step (string)
- unique: client, name

### meeting
- date (required, date)
- client (required, link to records/clients/)
- attendees (required, link to records/contacts/)
- project (link to records/projects/)
- duration_min (int)
- next_step (string)
- unique: date, client

