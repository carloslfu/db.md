---
type: log
---

# Curator log

## [2026-05-01 09:00] ingest | [[sources/emails/2026/05/2026-05-22-renewal-a]]
First well-formed entry. Recognized kind, parseable timestamp, in order.

## [2026-05-02 10:00] create | [[records/contacts/sarah-chen]]
Second well-formed entry. Still ascending in time.

## [2026-13-99 99:99] update | [[records/companies/northstar]]
This header's timestamp is 2026-13-99 99:99 — month 13, day 99, hour 99.
Unparseable, so this entry trips the bad-timestamp log error.

## [2026-05-03 11:00] frobnicate | [[records/contacts/sarah-chen]]
The kind "frobnicate" is not in the recognized-kind set
(ingest/create/update/delete/rename/link/validate/index-rebuild/contradiction),
so this entry trips the unknown-kind warning. The timestamp is fine and
in order.

## [2026-05-02 08:00] validate
This validate entry is dated 2026-05-02 08:00, which is BEFORE the
previous parseable entry (2026-05-03 11:00). Entries must be in
non-decreasing time order, so this trips the out-of-order warning
(a signal that the append-only log was rewritten).
