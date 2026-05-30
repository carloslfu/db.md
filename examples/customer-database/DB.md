---
type: db-md
scope: company
owner: RevOps team
computer_id: acme-revops
---

# Acme customer database

Lightweight, agent-queryable customer database. Sits alongside HubSpot
(source of truth for pipeline) and serves as the team's quick-recall
layer: who do we know at company X, what was discussed last, where are
we in their renewal cycle.

The three layers compose for that recall:

- `sources/` — raw evidence pulled in from outside: Gong call
  transcripts and HubSpot exports, preserved verbatim.
- `records/` — atomic typed data the team maintains: one `contact`
  record per person, one `company` record per account, one `call`
  record per logged conversation, cross-linked.
- `wiki/` — the curator's synthesis: one denormalized account page per
  company for fast agent context, plus account-pulse signals.

## Agent instructions

You are the curator for this store. HubSpot remains the source of truth
for the deal pipeline; this store exists for fast agent context across
customer relationships.

- When a HubSpot export lands in `sources/hubspot-exports/`, sync the
  deltas into the `contact` and `company` records under `records/`.
- When a Gong call transcript lands in `sources/transcripts/`, create a
  `call` record in `records/calls/` with attendees, topics, decisions,
  and next step. Link it to the relevant `company` and `contact`
  records, and link back to the transcript it was derived from.
- Maintain `last_touch` on `contact` records — update it on every new
  email, call, or note.
- Keep the per-account wiki page in `wiki/accounts/` current, and flag
  account-level signals (expansion, churn risk, renewal countdown) in
  `wiki/synthesis/account-pulse.md`.
- Keep `summary` fields one line and current — refresh a contact's
  summary when its role changes.

What you don't do:

- Don't push changes back to HubSpot. This is a read-side cache.
- Don't infer churn risk from a single interaction; require at least two
  signals (e.g. a drop in product usage plus an email-sentiment shift).

## Policies

### Frozen pages
- `wiki/synthesis/account-pulse.md` — RevOps-owned account signals; do not modify.

### Ignored types
- `test` — read as ambient context but never synthesised into records or wiki pages.

## Schemas

### company
- name (required, string)
- domain (required, string)
- industry (string)
- relationship (enum: customer, vendor, partner, prospect)

### contact
- name (required, string)
- email (required, email)
- company (required, link to records/companies/)
- role (string)
- first_touch (date)
- last_touch (date)

### call
- date (required, date)
- company (required, link to records/companies/)
- attendees (required, link to records/contacts/)
- duration_min (int)
- recording (url)
- next_step (string)
- category (enum: discovery, demo, technical, renewal, other)
