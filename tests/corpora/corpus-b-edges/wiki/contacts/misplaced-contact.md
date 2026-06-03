---
type: contact
id: misplaced-contact
created: 2026-05-24T10:00:00-07:00
updated: 2026-05-24T10:00:00-07:00
summary: "Valid contact deliberately filed under wiki/ — v0.2 enforces no type/layer rule, so it fires no issue"
name: Wiki Misfile
email: misfile@northstar.io
company: [[records/companies/northstar]]
role: Test Subject
status: active
---

# Wiki Misfile

A fully valid `contact` record filed under `wiki/` rather than the
conventional `records/`. The schema (name / email / company-link) is
satisfied, the summary and timestamps are valid, and the one wiki-link
([[records/companies/northstar]]) resolves. In v0.2 the three-layer
layout is convention, not enforcement (`LAYER_TYPE_MISMATCH` was
removed), so this file is fully valid and fires no issue — it guards
that a misplaced type no longer trips a validation code.
