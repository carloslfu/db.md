---
type: log
---

# Curator log

Append-only timeline of what the curator did. Each entry:
`[timestamp] verb | object` where the object is a wiki-link. Every
client-specific entry stays inside one client's records — confidentiality
is a line we never cross, even here.

## [2026-05-19 15:20] create | [[records/meetings/2026-05-19-brightmore-campaign-review]]
Synthesized the Brightmore campaign review from the transcript [[sources/transcripts/2026/05/2026-05-19-brightmore-campaign-review]]. Eleanor approved the "Gather" concept; led the record with the next step (key-art batch to brand + legal). Linked client, attendee, and project.

## [2026-05-19 15:30] update | [[records/projects/brightmore-holiday-campaign]]
Flipped the concept item to done, set next_step to the key-art layout delivery, refreshed the body to "in production."

## [2026-05-20 10:15] ingest | [[sources/emails/2026/05/2026-05-20-brightmore-approval]]
Filed Eleanor's written approval of the "Gather" concept as the approval of record. Linked it from the meeting and the project; updated last_touch on [[records/contacts/eleanor-whitfield]].

## [2026-05-20 14:00] create | [[records/meetings/2026-05-20-riverkeep-report-kickoff]]
Synthesized the Riverkeep annual-report kickoff from [[sources/transcripts/2026/05/2026-05-20-riverkeep-report-kickoff]]. Captured the concrete-first narrative direction and the firm print deadline; linked client, both attendees, and the project.

## [2026-05-20 14:10] update | [[records/projects/riverkeep-annual-report]]
Marked field stories and figures collected; set next_step to the first narrative layout (due 2026-06-02). Noted Grace holds final sign-off on donor-facing language.

## [2026-05-21 11:30] ingest | [[sources/transcripts/2026/05/2026-05-21-lumio-brand-review]]
Lumio brand-review transcript landed. Attendees: Maya, Theo. Maya narrowed the logo to routes B and C; Theo asked for the marketing site on the new brand.

## [2026-05-21 11:32] create | [[records/meetings/2026-05-21-lumio-brand-review]]
Created the meeting record, led with the next step (Maya's logo pick by 2026-05-26). Linked client [[records/clients/lumio]], attendees, and project [[records/projects/lumio-brand-identity]]. Flagged the marketing-site ask as outside the SOW.

## [2026-05-21 11:40] update | [[records/projects/lumio-brand-identity]]
Recorded routes B/C as the live options, set next_step to the logo pick, added the marketing-site line to items as pending added scope.

## [2026-05-22 09:55] ingest | [[sources/emails/2026/05/2026-05-22-lumio-scope-change]]
Theo's email formally requests the marketing site (homepage + pricing at minimum) on the new brand, and asks for the hours/timeline delta. This is a scope change against [[records/projects/lumio-brand-identity]].

## [2026-05-22 10:00] link | [[records/playbooks/scope-creep-handling]]
Applied the scope-creep playbook: marketing site is beyond the brand-identity SOW. Did NOT start the work. Quantified the delta and surfaced it for Maya to choose change-order vs. de-prioritize; linked the request into [[records/projects/lumio-brand-identity]]. Held within Lumio's records only.

## [2026-05-22 10:05] validate
PASS — 0 errors. Schema `unique:` keys checked across client/contact/project/meeting; no duplicate warnings. Confidentiality spot-check: no record links two clients.
