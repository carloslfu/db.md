---
type: synthesis
meta-type: conclusion
created: 2026-06-01T08:00:00-07:00
updated: 2026-06-02T11:20:00-07:00
summary: "May 2026 vendor-spend overview; USD 18,236.40 across four vendors, AWS the line to watch."
topic: Vendor spend
derived_from:
  - "[[records/expenses/2026-05-01-linear]]"
  - "[[records/expenses/2026-05-02-google-workspace]]"
  - "[[records/expenses/2026-05-03-aws]]"
  - "[[records/expenses/2026-05-21-gunderson]]"
  - "[[sources/exports/amex/2026-05-statement]]"
  - "[[records/meetings/2026-06-02-vendor-spend-review]]"
tags: [finance, vendor]
status: active
---

# Vendor spend

Rolled-up view of Acme's vendor spend, refreshed when an Amex export
lands and reconciled at the monthly review
[[records/meetings/2026-06-02-vendor-spend-review]]. The atomic charges
are the [[records/expenses/index|expense records]]; this page is the
overview the team reads.

## May 2026

| Vendor             | Category       | Amount (USD) | Cadence    |
| ------------------ | -------------- | ------------ | ---------- |
| [[records/companies/aws]]              | Infrastructure | 13,912.40 | usage      |
| [[records/companies/gunderson]]        | Legal          | 3,500.00  | one-off    |
| [[records/companies/google-workspace]] | Software       | 504.00    | per-seat   |
| [[records/companies/linear]]           | Software       | 320.00    | per-seat   |
| **Total**          |                | **18,236.40** |        |

Source of truth: [[sources/exports/amex/2026-05-statement]].

## What moved

- [[records/companies/aws]] is ~8% above April on higher S3 egress — the
  only variable line and the one worth watching. The June review set a
  USD 15,000/month billing alert rather than re-architecting now.
- [[records/companies/gunderson]] is a one-off for the Halcyon DPA
  ([[records/decisions/engage-gunderson-halcyon-dpa]]); it will not recur.
- The two per-seat SaaS lines ([[records/companies/linear]],
  [[records/companies/google-workspace]]) are flat.

The narrative behind these relationships is on
[[records/profiles/vendor-relationships]].
