---
type: expense
id: bad-date-shape
created: 2026-05-07T12:00:00-07:00
updated: 2026-05-07T12:00:00-07:00
summary: "Expense whose date field is not ISO-8601"
date: May 7th, 2026
amount: 55.00
currency: USD
category: meals
vendor: [[records/companies/northstar]]
tags: [expense]
status: active
---

# Lunch — May

The `date` field is `May 7th, 2026`, which violates the schema's
`date (required, date)` shape modifier. Because `DB.md` declares an
explicit `expense` schema with a `date` modifier, this is reported as a
schema shape mismatch (the schema check owns the field), not a generic
frontmatter timestamp error.
