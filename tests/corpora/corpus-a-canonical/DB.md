---
type: db-md
scope: company
owner: Sarah Chen
computer_id: acme-ops
---

# Acme operations knowledge base

Company-scale institutional memory for Acme: customers, vendors,
meetings, invoices, and the expense ledger, plus the synthesis
conclusions the curator maintains on top of them.

This is the canonical happy-path store — the two layers
(`sources/` + `records/`) are fully populated, every content
file carries a `summary`, the index hierarchy is complete (root + each
layer + each type-folder, both `index.md` and `index.jsonl`), and
`dbmd validate` reports zero issues.

## Agent instructions

Use British English in `conclusion` records. When a vendor invoice arrives,
also create an `expense` record linked to the invoice. Prioritise
creating `contact` records from new-sender emails, and link each
contact to its `company` record. Do not synthesise conclusion records from
sources tagged `transient`. Keep `summary` fields one line and current
— refresh a contact's summary when its role changes.

## Policies

### Frozen pages
- `records/synthesis/2026-renewal-plan.md` — signed-off renewal plan; do not modify.

### Ignored types
- `test` — read as ambient context but never synthesised into conclusion records.

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
- relationship (enum: customer, vendor, partner, prospect)

### expense
- date (required, date)
- amount (required, currency)
- currency (default USD)
- category (string)
- vendor (required, link to records/companies/)

### meeting
- date (required, date)
- attendees (required)
- location (string)
- duration_min (int)

### invoice
- date (required, date)
- amount (required, currency)
- vendor (required, link to records/companies/)
- status (required, enum: paid, unpaid, void)
- paid_at (date)
