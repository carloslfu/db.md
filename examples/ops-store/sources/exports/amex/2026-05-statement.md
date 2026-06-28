---
type: amex-export
created: 2026-06-01T07:00:00-07:00
updated: 2026-06-01T07:00:00-07:00
summary: "Acme company Amex statement, May 2026; four vendor charges totalling USD 18,236.40."
exported_at: 2026-06-01T07:00:00-07:00
account: "Acme Inc — Business Platinum ····3007"
period: 2026-05
tags: [internal, finance]
status: archived
---

# Amex export — May 2026 statement

Verbatim export of the Acme company card for May 2026, pulled by
[[records/contacts/tom-becker]] for the monthly reconciliation. Each row
below becomes one `expense` record under `records/expenses/`, with
`vendor` linked to the matching company and `source` pointed back here.
The card statement is the source of truth; expense records are never
written back to it.

| Date       | Merchant              | Category      | Amount (USD) |
| ---------- | --------------------- | ------------- | ------------ |
| 2026-05-01 | LINEAR.APP            | Software      | 320.00       |
| 2026-05-02 | GOOGLE WORKSPACE      | Software      | 504.00       |
| 2026-05-03 | AMAZON WEB SERVICES   | Infrastructure| 13,912.40    |
| 2026-05-21 | GUNDERSON & PARK LLP  | Legal         | 3,500.00     |

**Statement total: USD 18,236.40.**

Notes carried over from the cardholder:

- LINEAR.APP — 40 seats × USD 8/seat, monthly Standard plan.
- AMAZON WEB SERVICES — May usage; ~8% above April on higher S3 egress.
- GUNDERSON & PARK LLP — Halcyon DPA review matter; pre-approved by
  [[records/contacts/priya-nair]] in [[sources/emails/2026-05-22-priya-tom-gunderson]].
