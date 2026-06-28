---
type: synthesis
meta-type: conclusion
created: 2026-05-22T09:35:00-07:00
updated: 2026-06-01T07:10:00-07:00
summary: "Contradiction and merge log; the curator flags possible duplicates here instead of merging unprompted."
topic: Duplicate and contradiction log
derived_from:
  - "[[records/contacts/elena-rodriguez]]"
  - "[[records/companies/halcyon]]"
tags: [curation]
status: active
---

# Duplicate and contradiction log

Where the curator records possible duplicate records and conflicting
facts for a human to resolve. Per `DB.md`, the curator never merges
contacts on its own — it flags the candidate here and waits.
`dbmd validate` surfaces schema-declared duplicates as `DUP_UNIQUE_KEY`
warnings (e.g. the `### expense` `unique: date, amount, vendor` key);
this page is the human-readable companion to those warnings.

## Open

_None._ No outstanding duplicates or contradictions.

## Resolved

- **2026-05-22 — Elena Rodriguez possible duplicate.** An inbound email
  arrived from `e.rodriguez@halcyon.io` while the contact on file used
  `elena.rodriguez@halcyon.io`. Flagged rather than auto-created. Priya
  confirmed it was the same person on a secondary alias; kept the single
  record [[records/contacts/elena-rodriguez]] and took no merge action.
- **2026-06-01 — No expense collisions.** The May Amex import
  [[sources/exports/amex/2026-05-statement]] produced four expense
  records with distinct `date, amount, vendor` keys; `dbmd validate`
  reported no `DUP_UNIQUE_KEY` warnings.
