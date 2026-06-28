---
type: profile
meta-type: conclusion
created: 2026-06-02T11:15:00-07:00
updated: 2026-06-02T11:15:00-07:00
summary: "How Acme runs its vendor relationships; three recurring SaaS/infra vendors plus on-demand legal counsel."
topic: Acme vendor relationships
derived_from:
  - "[[records/companies/linear]]"
  - "[[records/companies/aws]]"
  - "[[records/companies/google-workspace]]"
  - "[[records/companies/gunderson]]"
  - "[[records/contacts/tom-becker]]"
  - "[[records/decisions/adopt-linear]]"
  - "[[records/decisions/engage-gunderson-halcyon-dpa]]"
tags: [vendor, finance]
status: active
---

# Acme vendor relationships

Synthesised view of how Acme works with its vendors. The atomic billing
facts live on the per-vendor [[records/companies/index|company records]]
and the [[records/expenses/index|expense records]]; this page is the
understanding that sits on top.

## The shape of the spend

Acme keeps a deliberately small vendor roster, owned on the billing side
by [[records/contacts/tom-becker]] and reviewed monthly with
[[records/contacts/priya-nair]]. It splits cleanly into two kinds:

- **Recurring, per-seat or usage SaaS** — [[records/companies/linear]]
  (project tracking, USD 320/mo), [[records/companies/google-workspace]]
  (email and docs, USD 504/mo), and [[records/companies/aws]] (cloud
  infrastructure, the largest line at ~USD 14k/mo and the only variable
  one). These are predictable and need watching, not deciding.
- **On-demand professional services** — [[records/companies/gunderson]]
  is engaged per matter, not on a subscription. The May charge was a
  one-off for the Halcyon DPA review
  ([[records/decisions/engage-gunderson-halcyon-dpa]]).

## How vendors get adopted

Adoption is decision-backed and written down. Linear came in after a
trial with terms confirmed in writing before the workspace was flipped
to paid ([[records/decisions/adopt-linear]]). Outside counsel is brought
in only when the work is outside what the team will redline itself. The
discipline: every recurring vendor traces to a `decision` record, and
every charge traces to a row on the monthly Amex export.

## What to watch

[[records/companies/aws]] is the one moving line — May ran roughly 8%
above April on S3 egress, which is why the June review set a billing
alert. Everything else is flat. The rolled-up figures live on
[[records/synthesis/vendor-spend]].
