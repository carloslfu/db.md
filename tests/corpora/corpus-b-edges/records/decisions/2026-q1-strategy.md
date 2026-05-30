---
type: decision
id: 2026-q1-strategy
created: 2026-01-15T09:00:00-08:00
updated: 2026-01-20T09:00:00-08:00
summary: "Q1 2026 strategy — finalized and frozen per DB.md ## Policies"
decided_by: Sarah Chen
affects:
  - [[records/companies/northstar]]
alternatives_considered: "Hold pricing flat; raise 10%; raise 20%"
tags: [decision, frozen]
status: final
---

# Q1 2026 strategy

A valid, well-formed decision record. It is listed under
`DB.md ## Policies → ### Frozen pages`, so any `dbmd write` /
`dbmd fm set` / `dbmd rename` against it is refused at write time
(`POLICY_FROZEN_PAGE`). It is NOT a `dbmd validate` issue — the file
itself is correct; the refusal is a write-time policy, captured in
`EXPECTED/policy-refusal/`.
