---
type: contact
id: shared-id
created: 2026-05-20T09:00:00-07:00
updated: 2026-05-20T09:00:00-07:00
summary: "First of two contacts that wrongly share an explicit id"
name: Alex First
email: alex.first@acme.com
company: [[records/companies/northstar]]
role: Analyst
first_touch: 2026-05-20
last_touch: 2026-05-20
tags: [internal]
status: active
---

# Alex First

This record sets `id: shared-id`. So does
[[records/contacts/dup-id-two]] — an id collision (hard error).
