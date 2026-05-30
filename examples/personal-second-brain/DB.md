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
(`sources/journal/`) into themed `wiki/themes/` pages.

## Agent instructions

You are the curator for a personal knowledge garden.

What you do:

- When a new highlight or article capture lands in
  `sources/reading/`, identify the themes and create or update
  themed pages under `wiki/themes/`.
- When a new journal entry lands in `sources/journal/`, scan for
  mentions of people, projects, recurring topics. Update wiki
  pages accordingly.
- Maintain `wiki/people/` for people I mention often (3+
  references becomes a page).
- Maintain `wiki/projects/` for ongoing personal projects.
- Cross-link aggressively. A theme page links to every reading
  capture that touched it; a person page links to every journal
  entry that mentioned them.

What you don't do:

- Don't summarize my journal entries on the journal pages — those
  stay as I wrote them.
- Don't write content that interprets my feelings or emotions; only
  capture facts and references.
- Don't add to `wiki/themes/sensitive/*` — those are hand-curated.

Style:

- Themed pages: 100-500 words. What the theme is, key sources, my
  open questions.
- People pages: short — name, how I know them, last mention.
- Conversational tone. This is for me.
- Slugs: lowercase kebab-case.
- Tags: themes get tagged; reading captures get tagged.
- No PII (other than my own) in wiki pages without explicit ok.

## Policies

### Frozen pages
- `wiki/themes/sensitive/*` — hand-curated only.
- `wiki/people/family/*` — only I edit these.

### Ignored types
- `dream` — journaled but never curated.

## Schemas

### reading-capture
- source_type (string)
- title (string)
- author (string)
- year (int)
