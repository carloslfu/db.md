---
type: db-md
scope: company
owner: Edge Cases
computer_id: corpus-b
---

# corpus-b-edges — the designed-to-fail store

This store is **broken on purpose**. Every content file, index, log
entry, schema, and policy here exists to trip exactly one (or a few)
`dbmd validate` issue codes. It is the backbone correctness fixture for
the validation engine: `EXPECTED/validate.json` is the hand-derived,
complete set of issues `dbmd validate --all --json` must emit, and
`EXPECTED/policy-refusal/<scenario>.json` documents the write-time
`POLICY_FROZEN_PAGE` refusals.

Nothing here should ever be "fixed" in place — the breakage IS the
fixture. See `EXPECTED/README.md` for the per-code map.

## Agent instructions

Curator behaviour is irrelevant for this store; it exists only to be
validated, never operated. Do not synthesise, do not repair.

## Policies

### Frozen pages
- `records/decisions/2026-q1-strategy.md` — finalized, do not modify.
- `records/synthesis/2026-annual-plan.md` — signed-off plan.

### Ignored types
- `test`, `temp` — read but never synthesise.

## Schemas

### contact
- name (required, string)
- email (required, email)
- company (required, link to records/companies/)
- role (string)
- first_touch (date)
- last_touch (date)
- unique: email

### expense
- date (required, date)
- amount (required, currency)
- currency (default USD)
- category (string)
- vendor (required, link to records/companies/)
- unique: date, amount, vendor

### invoice
- date (required, date)
- amount (required, currency)
- vendor (required, link to records/companies/)
- status (required, enum: paid, unpaid, void)
- paid_at (date)
- unique: vendor, date, amount

### company
- name (required, string)
- domain (required, string)
- industry (string)
- relationship (enum: customer, vendor, partner, prospect)
- unique: domain

### meeting
- date (required, date)
- attendees (required, link to records/contacts/)
- location (string)
- duration_min (int)
- unique: date, attendees

### email
- unique: from, subject, date
