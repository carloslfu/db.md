---
type: process
id: invoicing
created: 2026-03-02T09:00:00-08:00
updated: 2026-03-02T09:00:00-08:00
summary: "Monthly invoicing process; the jsonl twin records a stale summary for this file"
tags: [process]
status: active
---

# Invoicing process

Valid, well-formed record. The file frontmatter (here) is the source of
truth; the `index.jsonl` twin carries an out-of-date `summary` for this
path, which is the single breakage in this folder
(`INDEX_JSONL_STALE`). The `index.md` browse entry, by contrast, matches
this `summary` exactly, so no `INDEX_SUMMARY_MISMATCH` fires.
