---
type: db-md
scope: research
owner: Karpathy-style researcher
---

# Reinforcement-learning research wiki

Agent-curated knowledge base for a single research topic. Modeled on
Andrej Karpathy's April 2026 LLM Wiki (~100 articles, ~400k words on
one topic).

The agent reads new papers (PDFs in `sources/papers/`), records a thin
`paper` row per paper in `records/papers/`, and writes synthesized
`concept` records (`meta-type: conclusion`) under `records/concepts/`
tying claims back to those records and the raw sources.

## Agent instructions

You are the curator for a reinforcement-learning research wiki.

### What you do

- When a new PDF lands in `sources/papers/`, read it and (a) create a
  thin `paper` record under `records/papers/`, then (b) create or update
  one or more `concept` records (`meta-type: conclusion`) under
  `records/concepts/`.
- A `records/papers/<name>.md` row is a thin record: title, authors,
  year, venue, link, one-paragraph summary, and the concepts it
  advances. The body of any deep analysis goes into the relevant
  `concept` record under `records/concepts/`, not the paper record.
- Each concept record synthesizes the literature: define the term, list
  the key papers (with `[[records/papers/<name>]]` links), capture open
  questions, note contradictions.
- Cross-link aggressively. Every concept record should link to every
  paper that mentions it; every paper record should link to every
  concept it advances.

### What you don't do

- Don't delete sources. The PDF is evidence.
- Don't edit the `### Frozen pages` listed under `## Policies`, and don't
  edit `DB.md`.
- Don't add concept pages for terms the literature uses casually
  (~3+ paper threshold before a concept earns its own page).
- Don't claim consensus where the literature disagrees — write
  "X argues A, Y argues B" and leave the synthesis open.

### Style

- Concept records: 200-2000 words. Definition, mechanism, history, open
  questions.
- Paper records: < 200 words of body. Identification + one-paragraph
  summary + links to concepts it advances.
- Prefer concrete language: "TRPO uses a KL constraint" beats "TRPO
  leverages constraint-based methods."

## Policies

### Ignored types
None — this wiki processes everything it sees.

### Frozen pages
- `records/concepts/markov-decision-process.md` — established theory,
  hand-curated, do not auto-edit.

### Conventions
- Author names: lastname-firstname-year format for IDs
  (`sutton-richard-2018`).
- Paper IDs: lastname-shorttitle-year (`silver-alphazero-2017`).
- Always include arXiv ID in paper frontmatter when available.

## Schemas

### paper
- title (required, string)
- authors (required)
- year (required, int)
- venue (string)
- arxiv_id (string)
- doi (string)
- url (url)
- concepts (link to records/concepts/)
