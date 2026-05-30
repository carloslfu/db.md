---
type: contact
id: misplaced-contact
created: 2026-05-24T10:00:00-07:00
updated: 2026-05-24T10:00:00-07:00
summary: "Valid contact deliberately filed under wiki/ — fires only LAYER_TYPE_MISMATCH"
name: Wiki Misfile
email: misfile@northstar.io
company: [[records/companies/northstar]]
role: Test Subject
status: active
---

# Wiki Misfile

A fully valid `contact` record whose only defect is its location: a
`contact` belongs under `records/`, but this file sits under `wiki/`.
The schema (name / email / company-link) is satisfied, the summary and
timestamps are valid, and the one wiki-link
([[records/companies/northstar]]) resolves — so this file fires exactly
`LAYER_TYPE_MISMATCH` (warning) and nothing else.
