---
type: db-md
scope: company
owner: Priya Nair
computer_id: lumen-ops
---

# Lumen Labs operations knowledge base

Company-scale institutional memory for Lumen Labs, a five-person
product-design studio: clients, vendors, the people we deal with, the
meetings we run, and the bills we pay, plus the synthesis wiki the
curator maintains on top of them.

This is the **agent-eval store**. Only `sources/` is populated — a
handful of emails, one call transcript, and two documents. `records/`,
`wiki/`, `index.md`, and `log.md` are absent on purpose. A correct
curator session, given `dbmd spec` and this `DB.md`, derives the
records and wiki pages, populates the index hierarchy write-through,
and leaves the store passing `dbmd validate`. See `NOTES.md` for the
entities the structural eval expects.

## Agent instructions

Use British English spelling in `wiki/` pages (e.g. "organise",
"synthesise", "centre"). `records/` field values stay verbatim from
the source.

Whenever a vendor invoice arrives (an `email` or `pdf-source` that
states an amount due), create BOTH an `invoice` record and a matching
`expense` record, and wiki-link the expense to the invoice it paid.

Prioritise creating a `contact` record from every new human sender or
named attendee, and link each `contact` to its `company` record (create
the company first if it does not yet exist). A bare email address with
no human name attached (e.g. `billing@`, `no-reply@`, `notifications@`)
is NOT a contact — treat it as ambient routing on the company, not a
person.

Do not synthesise wiki pages from any source whose `tags` include
`transient`. Keep every `summary` to one line and current.

## Policies

### Frozen pages
- `records/decisions/founding-rate-card.md` — the studio's signed rate card; never modify if it already exists.

### Ignored types
- `newsletter` — read as ambient context but never synthesise into wiki pages or records.

## Schemas

### contact
- name (required, string)
- email (required, email)
- company (required, link to records/companies/)
- role (string)
- first_touch (date)
- last_touch (date)

### company
- name (required, string)
- domain (required, string)
- industry (string)
- relationship (required, enum: client, vendor, partner, prospect)

### meeting
- date (required, date)
- attendees (required, link to records/contacts/)
- location (string)
- duration_min (int)

### invoice
- date (required, date)
- amount (required, currency)
- vendor (required, link to records/companies/)
- status (required, enum: paid, unpaid, void)
- paid_at (date)

### expense
- date (required, date)
- amount (required, currency)
- currency (default USD)
- category (string)
- vendor (required, link to records/companies/)
