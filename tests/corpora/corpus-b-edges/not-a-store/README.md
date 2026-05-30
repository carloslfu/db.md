# not-a-store

This directory is intentionally NOT a db.md store: it has no `DB.md` at
its root. It exists so that running

    dbmd validate tests/corpora/corpus-b-edges/not-a-store

emits a single `NOT_A_STORE` error and exits non-zero, instead of
guessing. See ../EXPECTED/not-a-store.json for the hand-derived
expected output.

It is deliberately OUTSIDE the validate scope of corpus-b-edges itself:
`dbmd validate --all tests/corpora/corpus-b-edges` validates the store
rooted at corpus-b-edges/DB.md and must not descend into this sibling
non-store. NOT_A_STORE is only producible by pointing `dbmd` directly
at this path.
