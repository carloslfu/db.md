---
type: log
---

# Curator log

## [2026-04-03 08:05] ingest | [[sources/emails/2026/04/2026-04-03-figma-renewal-notice]]
Figma annual-renewal notice received; flagged for an expense + invoice once the charge posts.

## [2026-04-15 16:50] create | [[records/meetings/2026/04/2026-04-15-northstar-quarterly-review]]
Logged the Q1 review with Sarah Chen. Renewal-expansion intent noted.

## [2026-04-18 08:10] create | [[records/invoices/2026/04/2026-04-18-figma-annual]]
Figma annual charge posted; created the invoice record and the matching expense ledger entry.

## [2026-04-30 07:35] ingest | [[sources/docs/2026-04-30-aws-invoice]]
AWS April invoice PDF dropped into sources/docs; extracted text and reconciled.

## [2026-05-02 09:05] update | [[records/invoices/2026/04/2026-04-30-aws-april]]
Marked the AWS April invoice paid (paid_at 2026-05-02).

## [2026-05-12 13:45] create | [[records/contacts/marcus-okafor]]
New sender on the renewal thread; created a contact (default summary), linked to Northstar.

## [2026-05-22 09:20] ingest | [[sources/emails/2026/05/2026-05-22-elena-renewal]]
Elena confirms the 120 → 175 seat expansion by email.

## [2026-05-22 11:05] create | [[records/meetings/2026/05/2026-05-22-northstar-renewal-call]]
Logged the renewal call; 175 seats agreed, volume discount confirmed at 150.

## [2026-05-22 12:30] update | [[wiki/projects/northstar-renewal]]
Refreshed the renewal narrative with the call outcome and the confirming email.

## [2026-05-23 09:00] link | [[wiki/synthesis/2026-renewal-plan]]
Cross-linked the signed-off renewal plan from the project page; page is now frozen.

## [2026-05-31 10:00] index-rebuild | -
Full `dbmd index rebuild` after the May expense drop; root + layer + type-folder catalogs reconciled.

## [2026-05-31 10:05] validate
PASS — 0 errors, 0 warnings. Store is clean across all three layers.
