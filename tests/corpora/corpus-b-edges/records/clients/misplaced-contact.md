---
type: contact
id: misplaced-contact
created: 2026-05-24T10:00:00-07:00
updated: 2026-05-24T10:00:00-07:00
summary: "Valid contact deliberately filed under records/clients/ — v0.2 enforces no type/layer rule, so it fires no schema or layer issue"
name: Client Misfile
email: misfile@northstar.io
company: [[records/companies/northstar]]
role: Test Subject
status: active
---

# Client Misfile

A fully valid `contact` record filed under a non-canonical records
folder (`records/clients/`) rather than the conventional
`records/contacts/`. The schema (name / email / company-link) is
satisfied, the summary and timestamps are valid, and the one wiki-link
([[records/companies/northstar]]) resolves. In v0.2 the records folder
layout is convention, not enforcement (`LAYER_TYPE_MISMATCH` was
removed), so this file is fully valid and fires no schema or layer
issue — it guards that a misplaced type no longer trips a validation
code. Its sibling `index.jsonl` is deliberately stale on `company`.
