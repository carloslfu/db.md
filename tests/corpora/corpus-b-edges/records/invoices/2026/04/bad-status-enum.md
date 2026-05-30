---
type: invoice
id: bad-status-enum
created: 2026-04-10T09:00:00-07:00
updated: 2026-04-10T09:00:00-07:00
summary: "Invoice whose status is outside the schema enum"
date: 2026-04-10
amount: 800.00
vendor: [[records/companies/northstar]]
status: pending
paid_at: null
tags: [invoice]
---

# Invoice INV-1002

The `status` field is `pending`, which is not in the schema's
`enum: paid, unpaid, void`. Reported as an enum violation.
