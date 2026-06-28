---
type: log
---

# Curator log

Append-only timeline of what the curator did, on its 15-minute schedule.
Each entry: `[timestamp] verb | object` where the object is a wiki-link.

## [2026-01-28 11:45] ingest | [[sources/emails/2026-01-28-jordan-mills-linear-trial]]
Trial recap from Jordan Mills (Linear): Standard plan, USD 8/user/mo, 40 seats, monthly billing. Filed as a vendor email.

## [2026-02-03 09:30] create | [[records/decisions/adopt-linear]]
Acme adopted Linear (Standard, 40 seats). Quoted the price/terms from the trial email as the decision evidence. Created the vendor record [[records/companies/linear]] and the AM contact [[records/contacts/jordan-mills]].

## [2026-05-22 09:20] ingest | [[sources/emails/2026-05-22-elena-rodriguez-renewal]]
Email from Elena Rodriguez (Halcyon): confirms renewal intent, seats 90 → 120; wants to move onto a 2-year term and asks to get the DPA / security review scheduled as their gate to signing.

## [2026-05-22 09:24] update | [[records/contacts/elena-rodriguez]]
Set last_touch to 2026-05-22; she is the champion driving the renewal expansion to 120 seats on a new 2-year term.

## [2026-05-22 09:27] update | [[records/companies/halcyon]]
Recorded the renewal expansion to 120 seats on a new 2-year term (renews July 2026, pending DPA + signature); refreshed summary.

## [2026-05-22 09:30] link | [[records/companies/halcyon]]
Confirmed champion link to [[records/contacts/elena-rodriguez]] and technical evaluator [[records/contacts/anh-tran]].

## [2026-05-22 09:33] contradiction | [[records/synthesis/dupes]]
Inbound used a secondary alias (e.rodriguez@) vs the on-file address. Flagged as a possible duplicate instead of auto-creating; left for a human to confirm.

## [2026-05-22 15:05] ingest | [[sources/transcripts/2026/05/2026-05-22-halcyon-security-review]]
Transcript of the 14:00 Halcyon security review landed. Attendees: Priya, Elena, Anh. Commercials settled; DPA flagged as the only blocker.

## [2026-05-22 15:10] create | [[records/meetings/2026-05-22-halcyon-security-review]]
Synthesised the meeting record from the transcript — attendees, decisions, and next step (Gunderson DPA + order form). Linked back to the transcript.

## [2026-05-22 15:25] create | [[records/decisions/approve-halcyon-renewal-terms]]
Recorded the 120-seat / 2-year-term terms. Quoted Priya locking the commercials from the transcript as evidence; linked the meeting and the renewal email.

## [2026-05-22 15:35] create | [[records/decisions/engage-gunderson-halcyon-dpa]]
Recorded the decision to engage outside counsel for the DPA. Quoted Priya's "bring in Gunderson" line from the transcript; created the vendor record [[records/companies/gunderson]].

## [2026-05-22 16:15] ingest | [[sources/emails/2026-05-22-priya-tom-gunderson]]
Internal email Priya → Tom approving the Gunderson engagement and pre-flagging a ~USD 3,500 invoice. Linked it into the engagement decision as confirming evidence.

## [2026-06-01 07:05] ingest | [[sources/exports/amex/2026-05-statement]]
May Amex statement landed: four vendor rows totalling USD 18,236.40. Created the missing vendor record [[records/companies/google-workspace]].

## [2026-06-01 07:08] create | [[records/expenses/2026-05-01-linear]]
Created the Linear expense (USD 320.00), vendor → [[records/companies/linear]], source → the Amex export. Then created [[records/expenses/2026-05-02-google-workspace]], [[records/expenses/2026-05-03-aws]], and [[records/expenses/2026-05-21-gunderson]] the same way.

## [2026-06-01 07:10] update | [[records/synthesis/dupes]]
Checked the four new expenses for date/amount/vendor collisions — none. Logged a clean state.

## [2026-06-01 08:00] update | [[records/synthesis/vendor-spend]]
Refreshed the May spend overview from the four expense records; flagged AWS as ~8% above April on S3 egress.

## [2026-06-02 10:35] update | [[records/tasklists/halcyon-renewal]]
Flipped the engagement item to done and the DPA-delivery item to in-progress after the vendor-spend review confirmed the Gunderson charge posted.

## [2026-06-02 11:00] update | [[records/profiles/elena-rodriguez]]
Refreshed the champion profile with the post-review state (terms approved, signature gated only on the DPA).

## [2026-06-02 11:33] validate
PASS — 0 errors. Schema `unique:` keys checked; no duplicate warnings.
