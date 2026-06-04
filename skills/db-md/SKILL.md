---
name: db-md
description: Operate a db.md store — the open database in plain files — with the `dbmd` CLI. Use when reading, writing, searching, validating, or curating any folder that has a DB.md at its root. Run `dbmd spec` for the full contract.
version: 0.2.0
license: Apache-2.0
---

<!-- The canonical db.md Agent Skill (source of truth: skills/db-md/SKILL.md in the db.md repo). To use it, place a copy where your harness reads skills, e.g. ~/.claude/skills/db-md/ or ~/.codex/skills/db-md/. The body points at "dbmd spec", so it never copies the SPEC and cannot drift. -->

# db.md (the `dbmd` CLI)

You have the `dbmd` binary on PATH. It operates a **db.md store**: a database
that is a plain directory — raw evidence in `sources/`, atomic typed data in
`records/`, curator-synthesized narrative in `wiki/`, all governed by a single
`DB.md` at the root. `dbmd` is deterministic file/data plumbing; **you are the
curator** — the reasoning, synthesis, and judgment are yours.

**Before anything else: load the contract once per session.**

```
dbmd spec            # prints the canonical SPEC — the single source of truth
```

`dbmd spec` is authoritative; this skill is only a pointer to it. Then read the
store's own `DB.md` for its identity, policies, and schemas — `DB.md` overrides
defaults, so read it before you write.

## Cheat sheet (grouped by session phase)

```
# Open — load the standard, then this store's rules
dbmd spec                                          # the contract (once per session)
dbmd fm get DB.md scope                             # this store's identity / policies / schemas

# Warm up — orient
dbmd tree                                           # the directory at a glance
dbmd stats                                          # counts, sizes, orphans, top types
dbmd index show                                     # the curated root catalog
dbmd log tail 20                                    # what was done lately (avoid duplicate work)

# Read — find and hydrate context (every command takes --json)
dbmd search "(renewal|contract|ARR)" --in records   # ripgrep; the regex IS your query expansion (no embeddings)
dbmd query --type contact --where company=Acme       # structured frontmatter query via the sidecar
dbmd graph neighborhood records/contacts/sarah-chen --hops 2   # context in one call
dbmd links records/contacts/sarah-chen               # who points here (blast radius)

# Write — create and connect (frontmatter is composed for you)
dbmd write records/meetings/standup.md --type meeting --summary "weekly sync"
dbmd fm set <file> <key>=<value>                     # update one field, atomically
dbmd link <from> <to>                                # append a wiki-link

# Validate — before you close
dbmd validate                                        # the working set (changed files)
dbmd validate --all                                  # full-store sweep

# Maintain / close — record what you did
dbmd index rebuild                                   # repair the catalog if needed
dbmd log <kind> <object> -m "<note>"                 # append to the store timeline
```

## Output contract (memorize)

```
--json on every command   # machine-parseable; errors print {"error":{code,message,hint}} on stderr
exit: 0 ok · 1 runtime · 2 usage · 3 not-a-store · 4 policy refusal · 5 collision · 6 validation-failed
```

The full, authoritative reference is always `dbmd spec`. This skill is a pointer,
not a copy — when in doubt, run `dbmd spec` and read the store's `DB.md`.
