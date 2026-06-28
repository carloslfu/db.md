# corpus-e-agent / EXPECTED — the agent-eval golden tree

This directory is the **byte-for-byte golden** the Block 7 end-to-end agent
eval (`crates/dbmd-cli/tests/agent_eval.rs`) compares against. It is the
machine-checkable companion to the sibling `../NOTES.md`, which states the
*intent* in prose; this tree states the *exact output a correct curator
session produces*. The two must agree — `NOTES.md` is the human contract,
`EXPECTED/` is the executable one.

## How this golden was derived (NOT copied blindly from tool output)

The eval drives a **scripted, deterministic curator** (not an LLM — so the
test is a stable CI gate, which the plan explicitly permits: "a Claude Code
session OR scripted agent", Block 7) through the lifecycle-ordered sequence
of real `dbmd` invocations a correct agent would issue against this store's
`sources/` + `DB.md`. Every payload in that sequence — each record field,
each `summary`, each conclusion body, each full-path wiki-link — was **authored
from the source evidence and the `DB.md` schemas/policies/instructions**, the
same judgment a curator applies:

- the **invoice→expense rule** (`DB.md` agent instructions) → both an
  `invoice` and a matching `expense` for the Helio Type bill, expense linked
  to the invoice;
- the **`newsletter` Ignored-types policy** + the `transient` tag → the
  Design Weekly digest produces NO record and NO conclusion (it remains a
  source, and `dbmd validate --all` surfaces it only as an `info`-level
  `POLICY_IGNORED_TYPE_PRESENT`, never an error);
- the **"bare role address is not a contact" rule** → no contact for
  `billing@heliotype.com`, `newsletter@…`, `hello@lumenlabs.studio`,
  `accounts@lumenlabs.studio`;
- the **schema `contact.company (required, link to records/companies/)`** +
  Theo being an independent contractor → a `records/companies/lumen-labs.md`
  anchor so Theo's required `company` link resolves to a real target (the
  NOTES "resolution (a)");
- the **British-English-in-conclusions rule** → "organise" / "synthesise" in the
  conclusion bodies, verbatim source values in the fact records;
- the **$45k SOW fee is a receivable, not an expense** → it is narrated in
  the project conclusion but never modelled as an `expense`.

Reproducibility is what lets a *byte-for-byte* golden exist at all: every
`dbmd` write surface seeds `created`/`updated` (and the `log.md` header
timestamp) from `dbmd_core::now()`, which honours the `DBMD_NOW` environment
variable (see `crates/dbmd-core/src/time.rs`). The harness pins `DBMD_NOW` to
a fixed, monotonically-advancing instant per step, so the timestamps in every
record, every `index.md`/`index.jsonl`, and every `log.md` line are stable
across runs. The eval asserts the regenerated store equals this tree
file-for-file AND byte-for-byte.

## Why it fails under a plausible bug

The byte-for-byte diff catches any regression in the write / index
write-through / log-append / canonical-serialization paths. On top of that,
the eval asserts **golden-independent intent properties** (so a golden that
was itself regenerated from buggy output would still be caught):

- a record exists for **every** entity `NOTES.md` requires (3 counterparty
  companies + the studio anchor, 4 contacts, 1 meeting, 1 invoice, 1
  expense), and **none** for the negative cases (the bare-role addresses, the
  newsletter);
- the conclusion records exist and their bodies link to their evidence (records +
  sources) via **full-path** wiki-links;
- `dbmd validate --all` returns **zero errors / zero warnings** (the lone
  `info` for the ignored newsletter is asserted explicitly, not ignored);
- the recorded command log satisfies the **session lifecycle** (first call is
  `log tail`; an `fm query` precedes each contact write; full-path links
  only; a `log <kind>` follows each write; `validate` runs in the back half;
  zero `index rebuild` in the operating loop; a final `log` entry).

## What is here vs. what is an input

- **Inputs (NOT in this tree):** `../DB.md` and `../sources/**` — the store
  the agent is given. The harness copies them into a temp store and operates
  there; the committed corpus is never mutated.
- **Golden output (this tree):** everything the curator session produces —
  `records/**`, the full `index.md` hierarchy (root + every layer
  + every non-empty type-folder, each with its `index.jsonl` twin), and
  `log.md`.

### A note on the `sources/**/index.*` files

The `sources/` content ships pre-populated (the store's initial state — a
"bulk external drop" in SPEC terms). A correct curator folds it into the
catalog with a **single** `dbmd index rebuild` during warm-up (before the
operating loop) — the one SPEC-sanctioned rebuild, distinct from the
write-through the loop uses for `records/`. The resulting
`sources/index.md` + each `sources/<type>/index.md` + `index.jsonl` are
therefore agent-produced output and are pinned here too. The source *content*
files are inputs and are not duplicated into this tree.
