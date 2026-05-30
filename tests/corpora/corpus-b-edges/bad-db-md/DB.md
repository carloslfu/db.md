---
type: notes
scope: company
---

# bad-db-md — a store whose DB.md breaks the identity contract

This sibling store exists solely to trip the three `DB_MD_*` codes in
ONE pass, via a separate `dbmd validate` invocation pointed straight at
it. It is NOT part of the corpus-b root sweep (the root sweep only
checks `<root>/DB.md` and walks `sources/`/`records/`/`wiki/` under the
root — never this folder). See EXPECTED/bad-db-md.json.

## Agent instructions

Recognized section — never flagged.

## Glossary

Unrecognized `##` section — DB.md may only carry `## Agent instructions`,
`## Policies`, `## Schemas`. This heading fires `DB_MD_UNKNOWN_SECTION`.
