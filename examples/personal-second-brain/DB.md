---
type: db-md
scope: personal
owner: Riley Tanaka
---

# Personal second brain

Lifelong knowledge garden — reading notes, project journals,
people-I-know, recipes, travel logs, decisions to remember,
quotes that stuck.

Modeled on the Obsidian / Roam tradition. The curator processes
reading-list captures (`sources/reading/`) and journal entries
(`sources/journal/`) into themed `theme` records
(`meta-type: conclusion`) under `records/themes/`.

## Agent instructions

You are the curator for a personal knowledge garden.

What you do:

- When a new highlight or article capture lands in
  `sources/reading/`, identify the themes and create or update
  themed records (`meta-type: conclusion`) under `records/themes/`.
- When a new journal entry lands in `sources/journal/`, scan for
  mentions of people, projects, recurring topics. Update the
  `meta-type: conclusion` records accordingly.
- Maintain `records/people/` for people I mention often (3+
  references becomes a record).
- Maintain `records/projects/` for ongoing personal projects.
- Cross-link aggressively. A theme record links to every reading
  capture that touched it; a person record links to every journal
  entry that mentioned them.

What you don't do:

- Don't summarize my journal entries on the journal pages — those
  stay as I wrote them.
- Don't write content that interprets my feelings or emotions; only
  capture facts and references.
- Don't add to `records/themes/sensitive/*` — those are hand-curated.

Style:

- Themed records: 100-500 words. What the theme is, key sources, my
  open questions.
- People records: short — name, how I know them, last mention.
- Conversational tone. This is for me.
- Slugs: lowercase kebab-case.
- Tags: themes get tagged; reading captures get tagged.
- No PII (other than my own) in `meta-type: conclusion` records without explicit ok.

## Policies

### Frozen pages
- `records/themes/sensitive/*` — hand-curated only.
- `records/people/family/*` — only I edit these.

### Ignored types
- `dream` — journaled but never curated.

## Schemas

### reading-capture
- source_type (string)
- title (string)
- author (string)
- year (int)
