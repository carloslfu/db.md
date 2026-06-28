# corpus-e-agent — expected agent output (structural eval guide)

This corpus is the **end-to-end agent eval** fixture (plan Block 6, line
498; eval wiring in Block 7, line 536). Only `sources/` ships. `records/`, `index.md`, and `log.md` are **absent on purpose** — producing
them is the agent's lift. This file documents what a *correct* curator
session should derive, so the structural eval can diff the agent's output
against an expected entity set. It is the human-readable companion to the
`EXPECTED/` tree the eval harness compares byte-for-byte; when `EXPECTED/`
is authored, it must agree with this file.

The agent is given `dbmd spec` (the SPEC, in its system prompt) plus this
store's `DB.md`. Nothing else. The store-specific `## Agent instructions`,
`## Policies`, and `## Schemas` in `DB.md` are part of the lift — schema
enforcement, the invoice→expense rule, the British-English-in-conclusions rule,
the "bare role address is not a contact" rule, and the `newsletter`
Ignored-types rule are all things the agent must honour from `DB.md`, not
from the SPEC alone.

---

## The source set (what's given)

| Path | type | Signal it carries |
|------|------|-------------------|
| `sources/emails/2026/04/2026-04-09-tideform-project-intro.md` | email | Daniel Osei (Tideform, Head of Product) intro; names Mara Lindqvist (design lead); Tideform = tideform.com; $40–50k phase-one rebrand |
| `sources/emails/2026/04/2026-04-15-theo-contractor-onboarding.md` | email | Theo Vance, freelance motion designer; $95/hr, 60h cap; joining the Tideform project |
| `sources/emails/2026/04/2026-04-22-helio-type-invoice.md` | email | Helio Type Foundry invoice HT-2026-0417, $1,188 USD, due 6 May; vendor = heliotype.com; **automated `billing@` sender (not a contact)** |
| `sources/emails/2026/05/2026-05-04-northgate-coffee-enquiry.md` | email | Sofia Reyes, founder of Northgate Coffee (northgatecoffee.co); packaging-redesign prospect, ~$12k |
| `sources/emails/2026/05/2026-05-06-designweekly-digest.md` | **newsletter** | Marketing digest. `newsletter` is an **Ignored type** + tagged `transient` → produces NO record and NO conclusion |
| `sources/transcripts/2026/04/2026-04-14-tideform-kickoff.md` | transcript | Tideform kickoff call; attendees Priya, Daniel, Mara, Theo; confirms **$45k** phase-one fee, 8-week term, first review week of 5 May |
| `sources/docs/2026-04-14-tideform-sow.md` | pdf-source | Countersigned SOW; parties Lumen Labs × Tideform; **$45,000 fixed fee**, 50% on signature / 50% on delivery |
| `sources/docs/2026-05-06-helio-type-receipt.md` | pdf-source | Receipt: invoice HT-2026-0417 **paid $1,188 on 6 May** via company card; category Software / type licences |

Owner of the store (from `DB.md`): **Priya Nair**, Lumen Labs. "us" =
Lumen Labs; the studio is the perspective, not a counterparty.

---

## Expected `records/`

Paths follow the store conventions: `contacts/` and `companies/` are flat
type-folders; `meetings/`, `expenses/`, `invoices/` are **date-sharded**
(`<type>/<YYYY>/<MM>/<file>.md`), matching `Store::shard_path_for` and the
corpus-a layout. Every record carries the universal frontmatter contract
(`type`, `id`, `created`, `updated`, `summary`) plus its type fields, and
every internal reference is a **full-path** wiki-link. `(date, amount,
vendor)` etc. must be unique (no soft-collision warnings at end of run).

### Companies (`records/companies/`) — 3 expected

1. **`records/companies/tideform.md`** — `company`
   - `name: Tideform` · `domain: tideform.com` · `relationship: client`
   - `industry`: forecasting / mobile app (judgment; e.g. "Consumer mobile app")
   - links: backlinked by both Tideform contacts, the meeting, the SOW source, the invoice
2. **`records/companies/helio-type.md`** — `company`
   - `name: Helio Type Foundry` · `domain: heliotype.com` · `relationship: vendor`
   - `industry`: type foundry / software
   - links: vendor of the Helio Type invoice + expense
3. **`records/companies/northgate-coffee.md`** — `company`
   - `name: Northgate Coffee` · `domain: northgatecoffee.co` · `relationship: prospect`
   - `industry`: coffee roastery / retail
   - links: backlinked by the Sofia Reyes contact

> **Lumen Labs (own company) is optional/judgment.** A curator may create
> `records/companies/lumen-labs.md` to anchor "us". The eval treats it as
> ACCEPTABLE-EXTRA, not REQUIRED — the three counterparty companies above
> are the required set. If created, `relationship` should not be
> `client`/`vendor`/`prospect`; "us" is not a counterparty (a custom value
> or a partner-ish marker is the agent's call). It is NOT counted as a
> soft-collision or an error.

### Contacts (`records/contacts/`) — 3 required, 1 judgment

Each links to its company. `relationship`-driven `tags` encouraged.

1. **`records/contacts/daniel-osei.md`** — `contact`
   - `name: Daniel Osei` · `email: daniel.osei@tideform.com` · `role: Head of Product`
   - `company: [[records/companies/tideform]]`
   - `first_touch: 2026-04-09` (intro email) · `last_touch: 2026-04-14` (kickoff)
2. **`records/contacts/mara-lindqvist.md`** — `contact`
   - `name: Mara Lindqvist` · `email: mara@tideform.com` · `role: Design Lead`
   - `company: [[records/companies/tideform]]`
   - `first_touch: 2026-04-09` (named in intro) · `last_touch: 2026-04-14` (kickoff attendee)
3. **`records/contacts/sofia-reyes.md`** — `contact`
   - `name: Sofia Reyes` · `email: sofia@northgatecoffee.co` · `role: Founder`
   - `company: [[records/companies/northgate-coffee]]`
   - `first_touch: 2026-05-04` · `last_touch: 2026-05-04`
4. **`records/contacts/theo-vance.md`** — `contact` *(judgment — see note)*
   - `name: Theo Vance` · `email: theo.vance@gmail.com` · `role: Freelance Motion Designer`
   - `company`: **the schema makes `company` required.** Theo is an
     independent contractor with a gmail address — there is no obvious
     company. Two acceptable resolutions, both valid in the eval:
     (a) create `records/companies/lumen-labs.md` and link Theo to it as a
     contracted resource, or (b) the agent surfaces the schema tension
     (contractor has no company) rather than inventing one. The eval treats
     a Theo contact as EXPECTED; the exact `company` target is judgment, but
     it must be a real, existing wiki-link target (no dangling/short-form
     link, no plain string — that would be `SCHEMA_LINK_PREFIX_MISMATCH`).

> **NOT contacts (negative cases the eval checks):**
> - `billing@heliotype.com`, `newsletter@designweekly.email`,
>   `hello@lumenlabs.studio`, `accounts@lumenlabs.studio` — bare
>   role/no-reply/own-inbox addresses. Per `DB.md` agent instructions, a
>   bare role address with no human name is ambient routing, **not** a
>   `contact`. Creating a contact for any of these is an eval FAILURE.
> - Priya Nair (the owner) — optional. She is "us"; a self-contact is
>   acceptable-extra, never required.

### Meetings (`records/meetings/`) — 1 expected

1. **`records/meetings/2026/04/2026-04-14-tideform-kickoff.md`** — `meeting`
   - `date: 2026-04-14` · `duration_min: 48` · `location`: video call / remote
   - `attendees` (block-form wiki-links, required by schema):
     - `[[records/contacts/daniel-osei]]`
     - `[[records/contacts/mara-lindqvist]]`
     - `[[records/contacts/theo-vance]]`
     - (Priya as attendee only if a Priya contact was created)
   - body wiki-links to the transcript source
     `[[sources/transcripts/2026/04/2026-04-14-tideform-kickoff]]` and to the
     SOW source
   - **Derived from the transcript** (and confirmed by the SOW). One meeting
     record only — the kickoff. The Northgate enquiry is an *enquiry*, not a
     held meeting; no meeting record for it.

### Invoices (`records/invoices/`) — 1 expected

1. **`records/invoices/2026/04/2026-04-22-helio-type-ht-2026-0417.md`** — `invoice`
   - `date: 2026-04-22` · `amount: 1188.00` · `currency: USD`
   - `vendor: [[records/companies/helio-type]]`
   - `status: paid` (the 6 May receipt confirms payment) · `paid_at: 2026-05-06`
   - body wiki-links to both the invoice email source and the receipt source

### Expenses (`records/expenses/`) — 1 expected

Required by the `DB.md` rule: *"Whenever a vendor invoice arrives, create
BOTH an `invoice` record and a matching `expense` record, and wiki-link the
expense to the invoice it paid."*

1. **`records/expenses/2026/05/2026-05-06-helio-type-1188.md`** — `expense`
   - `date: 2026-05-06` (payment date) · `amount: 1188.00` · `currency: USD`
   - `category`: software / type licences · `vendor: [[records/companies/helio-type]]`
   - body wiki-links to the invoice record
     `[[records/invoices/2026/04/2026-04-22-helio-type-ht-2026-0417]]`

> The Tideform **$45k SOW fee** is the studio's *receivable*, not an
> expense. Whether to model it as an `invoice` (issued by Lumen) is
> judgment — the `DB.md` invoice→expense rule is scoped to *vendor invoices
> Lumen receives*, so a Tideform receivable is ACCEPTABLE-EXTRA, not
> required, and must NOT be turned into an `expense`. Turning the Tideform
> fee into an expense is an eval FAILURE (it would be a fabricated outgoing
> cost).

### Decisions (`records/decisions/`)

None required from these sources. Note the `DB.md` **Frozen page**
`records/decisions/founding-rate-card.md` does not exist in this corpus;
the policy is dormant here (nothing to refuse), present so the agent must
still parse `## Policies` without tripping.

---

## Expected conclusion records (`meta-type: conclusion`)

Synthesis records (`meta-type: conclusion`), British English (per `DB.md`). Dense full-path wiki-links
back to the records and sources. The required set is small and entity/
project-oriented; extra coherent pages are acceptable.

### People (`records/profiles/`) — bios for the substantive relationships

- **`records/profiles/daniel-osei.md`** — `profile` (`meta-type: conclusion`), `topic: Daniel Osei` —
  Tideform's Head of Product and the Lumen engagement's economic buyer.
  `derived_from`: the intro email, the kickoff transcript, the SOW. Links
  the contact + Tideform company + the kickoff meeting.
- **`records/profiles/mara-lindqvist.md`** — `profile` (`meta-type: conclusion`) — Tideform design lead,
  Lumen's day-to-day contact on the rebrand. Links contact + company +
  meeting.
- **`records/profiles/sofia-reyes.md`** — `profile` (`meta-type: conclusion`) — *judgment.* Founder of a
  prospect (Northgate); a thin bio is acceptable-extra. A curator may defer
  a profile until the prospect converts. Not required.
- *(Theo, Priya: optional bios — acceptable-extra.)*

### Projects (`records/projects/`) — 1 expected

- **`records/projects/tideform-rebrand.md`** — `project` (`meta-type: conclusion`), `topic: Tideform
  rebrand` — the flagship synthesis. Phase-one mobile rebrand + component
  library + marketing refresh + motion; **$45k fixed fee**, 8-week term,
  first review week of 5 May. `derived_from` the intro email, the kickoff
  transcript, and the SOW. Links the Tideform company, Daniel + Mara + Theo
  contacts, and the kickoff meeting. This is the densest node in the graph
  — most backlinks point here.

> A `records/projects/northgate-packaging.md` (prospect engagement) is
> ACCEPTABLE-EXTRA, not required.

### What must NOT appear in the conclusion records

- **No page derived from the `newsletter`** (`2026-05-06-designweekly-digest`).
  It is an Ignored type AND tagged `transient` — two independent reasons it
  is synthesis-excluded. A conclusion record citing it in `derived_from` is an eval
  FAILURE (and would itself raise `POLICY_IGNORED_TYPE_DERIVED`).

---

## Expected catalog (`index.md` / `index.jsonl`) — write-through, not rebuilt

The `records/` indexes must be **maintained write-through** by
the write commands as the agent works — **the agent must NOT call `dbmd index
rebuild` in the operating loop** (a rebuild call interleaved with the record
writes is a lifecycle failure; the catalog is write-through there).

The one exception is the **shipped `sources/`**: it is the store's initial
state — a *bulk external drop* in SPEC terms (`dbmd index rebuild` is "after a
bulk external drop into `sources/`"). A correct curator folds it into the
catalog **once, during warm-up, before the operating loop begins** — either a
single `dbmd index rebuild` (leaves the source files byte-untouched; the
golden harness uses this) or a `dbmd fm init` per source (also valid, but it
re-canonicalizes — i.e. rewrites — each source file's frontmatter). The
lifecycle assertion therefore allows **at most one** rebuild, and only if it
occurs *before* the first content write; zero rebuilds appear once the
operating loop has started. At end of session the hierarchy must exist and
match a from-scratch rebuild:

- **Root** `index.md` — lists the two layers with type-folder counts.
- **Layer** indexes — `sources/index.md` and `records/index.md`.
- **Type-folder** indexes (`index.md` + complete `index.jsonl` twin) for
  every non-empty folder. Given the expected set:
  - `sources/emails/index.md` (5 files inc. the newsletter — sources are
    indexed regardless of synthesis policy), `sources/transcripts/index.md`
    (1), `sources/docs/index.md` (2)
  - `records/companies/index.md` (3), `records/contacts/index.md` (3–4),
    `records/meetings/index.md` (1), `records/invoices/index.md` (1),
    `records/expenses/index.md` (1), `records/profiles/index.md` (2+),
    `records/projects/index.md` (1+)
- Every index entry **quotes the target file's `summary` verbatim**
  (`INDEX_SUMMARY_MISMATCH` if it drifts). Type-folder `index.md` entries
  are recency-ordered (newest `updated` first; ties by path ascending).
- Source type-folder indexes **aggregate across the date-shards**
  (`emails/2026/04/` + `emails/2026/05/`).

---

## Expected `log.md`

Append-only, created on the agent's first action. Must contain, at minimum,
one entry per meaningful write the agent made, using recognised kinds
(`ingest`, `create`, `update`, `link`, `validate`). Expected shape:

- An `ingest` (or read) acknowledgement of the new sources near the top.
- A `create` entry for each record produced (object = the
  full-path wiki-link to the created file).
- At least one `validate` entry in the back half of the session.
- A final closing entry.

Entries are time-ordered and well-formed (`## [YYYY-MM-DD HH:MM] <kind> |
<object>`), so `LOG_BAD_TIMESTAMP` / `LOG_UNKNOWN_KIND` / `LOG_OUT_OF_ORDER`
never fire.

---

## Session-lifecycle expectations (asserted from the harness command log)

Mirrors SPEC § "The agent session" and plan lines 500–508 / 537–544. The
eval harness records every `dbmd` invocation; these orderings must hold:

1. **Open** — behaviour obeys this store's `## Schemas` (e.g. `relationship`
   enum honoured, link fields are wiki-links not plain strings) and
   `## Policies` (newsletter excluded), proving `DB.md` was read.
2. **Warm up** — the **first** `dbmd` call is `dbmd log tail` (or
   `dbmd log since`). (Here the log is absent initially; the call returns
   empty/clean and the agent proceeds — still must be the first call.)
3. **Operate** —
   - a `dbmd fm query email=…` (or `--where`) **precedes** each
     `dbmd write` of a `contact` (pre-write dedup check #1);
   - a company-existence check precedes each company link;
   - **zero** short-form wiki-links in any write (full paths only);
   - **every** content-file write sets a non-empty `summary`;
   - a `dbmd log <kind> <object>` follows **every** `dbmd write` /
     `dbmd rename` within the session.
4. **Validate** — `dbmd validate` (working set) runs in the second half and
   returns **zero issues** at end of session.
5. **Catalog write-through** — resulting indexes equal a from-scratch
   `dbmd index rebuild`, with **zero `dbmd index rebuild` calls in the
   operating loop** (at most one rebuild is allowed, and only in warm-up
   before the first content write, to fold the shipped `sources/` — see
   "Expected catalog" above).
6. **Close** — a final well-formed `dbmd log` entry exists.

---

## Summary of the required entity set (the eval's structural core)

REQUIRED (run fails if missing or wrong):
- Companies: **tideform**, **helio-type**, **northgate-coffee** (3)
- Contacts: **daniel-osei**, **mara-lindqvist**, **sofia-reyes** (3) + a
  **theo-vance** contact with a valid (existing) `company` link
- Meeting: **2026-04-14 tideform-kickoff** (1), attendees = the Tideform
  contacts (+ Theo)
- Invoice: **helio-type HT-2026-0417**, status `paid`, `paid_at` 2026-05-06 (1)
- Expense: **helio-type $1,188 on 2026-05-06**, linked to the invoice (1)
- Conclusions (`meta-type: conclusion`): **records/projects/tideform-rebrand** + bios for
  **records/profiles/daniel-osei** and **records/profiles/mara-lindqvist**
- Full index hierarchy (root + 2 layers + every non-empty type-folder, each
  with `index.jsonl` twin) + a well-formed `log.md`
- `dbmd validate` clean

ACCEPTABLE-EXTRA (never penalised):
- `records/companies/lumen-labs.md`; Priya/Theo bios;
  `records/profiles/sofia-reyes`; `records/projects/northgate-packaging`; a Tideform
  receivable `invoice`; richer cross-links.

FAILURE (run is wrong if present):
- Any record (fact or conclusion) derived from the **newsletter**.
- A `contact` for any bare role/no-reply/own-inbox address
  (`billing@`, `newsletter@`, `hello@`, `accounts@`).
- The Tideform **$45k fee modelled as an `expense`**.
- Any short-form wiki-link, plain-string link field, missing `summary`, or
  schema violation (enum / required / link-prefix) — i.e. a non-clean
  `dbmd validate`.
- A `dbmd index rebuild` call inside the operating loop.
