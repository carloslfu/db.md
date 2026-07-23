# Changelog

All notable changes to db.md are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); db.md uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Two things version independently:

- **The format** (`SPEC.md`) — **v0.4** (v0.1 was the first tagged release).
- **The toolkit** (the `dbmd` binary, `crates/`) — versioned in
  `Cargo.toml`, currently **v0.8.0**.

## [0.8.0] — 2026-07-23

### Added

The link.md interconnect client grows its identity and federation surface
(the reference client for [link.md](https://github.com/carloslfu/link.md) v0):

- **`dbmd key generate`** — mint an Ed25519 agent/brain keypair locally; the
  PKCS#8 secret is written 0600 and never leaves the machine, and the public
  `multikey` + `publicKeySpki` are printed for hub registration.
- **`DBMD_AGENT_KEY_FILE`** — sign every authenticated request with a
  `LinkMD-Sig` proof of possession instead of sending a bearer, so nothing
  reusable ever crosses the wire or lands in a log or transcript. The agent
  key outranks the bearer when both are set.
- **`DBMD_BRAIN_KEY_FILE`** — self-custody push: `sync --push` signs each
  wire-profile-v1 feed entry locally and ships it through the pack flow; the
  hub verifies and stores the exact bytes but holds no key.
- **`dbmd key rotate`** — rotate a self-custodied brain key (link.md §9.1):
  the new key is signed by the old one, and pre-rotation history keeps
  verifying.
- **`dbmd mirror`** — replicate a brain with full chain verification (every
  signature, hash, and link), pinning its identity trust-on-first-use.
- **`dbmd serve`** — re-serve a mirror read-only over the hub HTTP binding
  (a std-only reference node); a downstream `dbmd` re-verifies the original
  signatures with no hub in the loop.
- **Key grantees** — `dbmd grant issue @brain <publicKeySpki>` grants to a
  bare multikey holder (a cross-party person or agent with no hub account).
- **Brain-addressed propose** — a ULID `propose` target posts to the brain's
  own inbox, authenticating opportunistically for a larger actor-class budget.

## [0.7.2] — 2026-07-19

### Added

- **`dbmd emit` now carries `link_spans`** — every wiki-link occurrence in a
  file's body, in document order, each with the byte range `[start, end)` it
  covers in `body`, plus the canonical `target`, the `|alias`, and the inner
  text `raw`.

  `links` answers "what does this file link to" (a deduped set — the graph's
  view). `link_spans` answers "where exactly are the tokens" — the view a
  RENDERER needs, because turning `[[…]]` into markup is a splice at a
  position, not a set operation. Without it, every host that renders db.md
  re-implements bracket scanning and, inevitably, fence tracking; the two
  implementations then disagree. (The motivating bug, found in a hosting hub:
  its renderer rewrote wiki-links inside fenced code blocks into live links,
  corrupting the examples on exactly the pages that documented the syntax.)

  Body-only by design: a `[[…]]` in a frontmatter value is a real edge and
  appears in `links`, but it is field data rather than markdown rendered in
  place, so it has no span. A `#fragment` stays inside `target` — fragments
  are not in the format, so splitting one is a host convention.

  Additive: existing `emit` consumers are unaffected. Format (SPEC v0.4)
  untouched.

## [0.7.1] — 2026-07-19

### Fixed

- **Reading a file no longer fails on a non-RFC3339 `created`/`updated`.** The
  parse now keeps an unparseable timestamp verbatim (the typed accessor stays
  absent, the value round-trips byte-for-byte) instead of erroring, matching
  how every other universal key already degrades — a non-scalar `type` or
  `summary` is preserved, never destroyed.

  This made `dbmd format` — and `fm`, `link`, `rename`, all of which read
  through the same path — **fail outright on any store carrying a date-only
  stamp** (`created: 2026-04-10`), the single most common legacy spelling in
  migrated stores. A store could not be canonicalized precisely because it was
  imperfect, which is backwards: canonicalization is what imperfect stores
  need. Reported by a host running `format` over real brains.

  The read/write asymmetry is deliberate and now explicit: **reading tolerates
  what a store already contains; `Frontmatter::set` still refuses to author a
  malformed value**, and `validate` still reports `FM_BAD_TIMESTAMP` (it reads
  the raw YAML, so leniency in the parser costs no enforcement). A value is
  never silently "repaired" — guessing a time zone for a date-only stamp is a
  data decision, not a formatting one.

## [0.7.0] — 2026-07-19

### Added

- `dbmd emit` — the whole-store structured dump: every content file plus
  `DB.md` as one JSON document under `--json`, each file carrying its parsed
  frontmatter (values verbatim), derived fields (layer, `type`, effective
  `meta-type` — records only, absent ⇒ `fact` — title from `name`/`title`/the
  first `#` heading, `summary`, `created`/`updated`), the verbatim body, the
  normalized wiki-link targets (fence-aware, alias stripped, `.md` appended,
  deduped; dangling targets kept), and the SHA-256 of the file bytes. The
  host-integration surface: a hosting hub or indexer ingests a store as a
  pure consumer of `dbmd` output instead of reimplementing the parse.
  Read-only and lenient (a malformed file degrades to body-only, never aborts
  the dump); text mode prints the would-be-emitted paths. The format is
  untouched — this is a toolkit read surface, not a SPEC change.

## [0.6.5] — 2026-07-17

### Format prose — the grounding doctrine, stated precisely (format unchanged: v0.4)

- SPEC.md now distinguishes **re-grounding** (what the curator does on
  demand: answering *"what supports this record now?"* against
  everything the store holds, including evidence the writer never saw)
  from **write-time lineage** (*"which inputs the writer read"* —
  unrecoverable unless recorded, which is exactly why unsaved testimony
  is the one mandated capture). Curator step 10 is now "Re-grounds
  records on demand"; the Source-first provenance section carries the
  full statement. Declared lineage stays a per-store `## Schemas`
  choice; a declared link is the author's assertion, checkable for
  shape and existence, never proof of causal truth.
- `meta-type: fact` now states that the name marks the *shape* (an
  atomic assertion recorded as data), not a truth warrant; the
  two-folders section states that evidence is what was received or
  said, not adjudicated truth.
- The speculative `RECORD_UNGROUNDED` roadmap tease is removed,
  replaced by the reason no groundedness check ships: a mechanical
  check can only detect *absent links*, and an absent link is not a
  defect under the discipline — groundedness is a judgment over
  meaning, the curator's job. The validation vocabulary is untouched.
- Toolkit code unchanged; this release re-bundles the spec so
  `dbmd spec` prints the current text.

## [0.6.4] — 2026-07-14

### Toolkit — resilient link transport (format unchanged: v0.4)

- Link verbs now retry bounded DNS, TCP, proxy-connect, and TLS-handshake
  failures that occur before any HTTP request reaches the hub. Mid-stream I/O
  is deliberately not guessed or replayed.
- The same pre-request retry contract covers content-addressed presigned pack
  uploads and downloads. A deterministic regression starts the loopback hub
  after the first refused connection and proves the next bounded attempt wins.

## [0.6.3] — 2026-07-14

### Toolkit — the link.md client verbs (format unchanged: v0.4)

`dbmd` now speaks the link.md CLIENT against a hub — one binary, two specs,
the git precedent (one binary carries both the object format and the wire
protocol). Five new subcommands, all in `dbmd-core`'s new `linkmd` module
(cargo feature `link`, default-on; a format-only library consumer drops it
and its HTTP/TLS closure with `default-features = false`):

- `dbmd resolve @brain[/<id>]` — the brain card, or a record by its
  db.md ULID `id` (the reserved `@brain/id` address shape; a
  `@brain/<store-path>.md` form also resolves) with frontmatter + body.
- `dbmd sync @brain [--out DIR]` — pull the granted slice as plain files
  (path-safety-gated before the first write; never deletes local files —
  divergence is reported; rebuilds the local index catalog after) and
  `--push`, which sends the local store as a whole-store snapshot (content
  `.md` + `DB.md` + `assets.jsonl`; derived catalogs and local history stay
  local). Small snapshots use the direct JSON lane. Larger snapshots use a
  deterministic ZIP pack uploaded through a short-lived object-store URL,
  then committed by hash. Both lanes enforce 100,000 files and 512 MB
  uncompressed; packs cap at 256 MB compressed.
- `dbmd grant issue|list|revoke` — the capability model, owner-side: read
  or write to a principal (email in v0), optional store-path-prefix scope,
  optional `--until` expiry.
- `dbmd propose <site> --app <slug> --body/--body-file` — write without
  trust: evidence into a published site's inbox, landing in the owner's
  `sources/inbox/` for their curator. Unauthenticated by design; the
  credential is verifiably never sent through this door.
- `dbmd subscribe @brain [--once] [--since N] [--interval S]` — fetch and
  locally verify the current Ed25519-signed feed head against the brain card;
  emit one event line per advance (NDJSON under `--json`).

Configuration is explicit and neutral — **no default hub is baked in**: the
hub URL resolves `--hub` > `DBMD_HUB_URL` > `hub = <URL>` in the store-local
`.dbmd/config` (a hidden toolkit file every store walk already skips); the
credential is the `DBMD_HUB_KEY` env var only, never a file in the store.
Non-HTTPS hubs are refused (loopback exempt for local hub development), and
error surfaces are agent-parseable: stable machine codes (`NO_HUB`,
`NO_CREDENTIAL`, `HUB_NOT_HTTPS`, `HUB_UNREACHABLE`, `HUB_ERROR`,
`HUB_NOT_JSON`, `BAD_ADDRESS`, `UNSAFE_PATH`, `PUSH_TOO_LARGE`, …) on the
existing exit-code contract (no new exit numbers).

A pre-release adversarial pass hardened the client surface before it ships:
every caller-supplied ref that travels as a URL path segment — the brain
ref on `sync` / `grant` / `subscribe`, the site handle on `propose`, the
grant id on `grant revoke` — is shape-validated at the library entry, so a
ref carrying `/`, `..`, `?`, or `#` is refused (`BAD_ADDRESS`,
`BAD_GRANT_ID`) before any request exists; the HTTPS-or-loopback guard
matches the scheme case-insensitively (an `HTTPS://` hub no longer draws a
misleading non-HTTPS refusal); hub-sourced strings (record bodies, names,
grant fields, error messages) are stripped of ANSI/C0 terminal control
sequences in text output — `--json` stays byte-verbatim; and
`propose --body-file` fails from file metadata, before the read, when the
body exceeds the hub's 16 KB inbox cap (`PROPOSE_TOO_LARGE`), the same
fail-before-upload contract as the push caps.

The same pass closes the large-store and trust-boundary failure modes. Hub
URLs with userinfo are refused so credentials cannot be smuggled into an
authority. Presigned upload/download URLs must be HTTPS, never receive the hub
bearer, and cannot redirect. Store packs are hash-bound, file-count and expanded-size
bounded, duplicate-path-free, and rejected on absolute paths, traversal,
non-regular entries, and symlinks before any local write. Pull extraction is
staged and containment-checked. Feed heads are accepted only when their brain
identity, sequence, pack hash, and Ed25519 signature all verify locally.

The FORMAT is untouched: SPEC.md still reserves only the `@brain/id` shape,
a store never needs link.md to be valid db.md, and the toolkit still never
phones home on its own — network I/O happens solely when a verb is invoked.

Dependencies: `ureq` (default-features off; `tls` + `gzip`) with the rustls
stack — pure-Rust TLS, no OpenSSL, no system TLS, so the released static
binaries carry their own trust store. Two new (permissive) license
identifiers entered the allowlist with it — ISC (ring, rustls-webpki,
untrusted) and CDLA-Permissive-2.0 (webpki-roots, the CCADB root-bundle
data) — recorded with verbatim call-outs in THIRD_PARTY_NOTICES and pinned
by the `license_policy` test. Also: `crossbeam-epoch` lockfile-bumped
0.9.18 → 0.9.20 (RUSTSEC-2026-0204, a pre-existing advisory via `ignore`;
compatible-version update, no manifest change).

## [0.6.2] — 2026-07-03

### Toolkit — `validate` catches a second frontmatter block in the body (format unchanged: v0.4)

A new `FM_IN_BODY` warning. It fires when a content file's body opens with a
second `---…---` frontmatter block — the classic import artifact: a source
file that carried its own frontmatter (every Obsidian note) was embedded
verbatim as the record body, so the record now has the canonical frontmatter
`dbmd` wrote at the top AND a leftover block opening its body. The file still
parses (the real frontmatter is valid), so nothing flagged it before; a
migration could silently produce a store full of double-frontmatter files that
validated clean. The check reuses the format's own fence-splitting and fires
only when the leading block parses as a non-empty YAML **mapping**, so a `---`
thematic-break rule or a fenced ` ```yaml ` example never false-fires.

The `dbmd write --body-file` primitive stays verbatim by design (predictable,
no silent byte-mutation); the validator is the honest backstop, and `SPEC.md`'s
"Importing existing data" section now states the strip-the-source-frontmatter
rule (with the `FM_IN_BODY` reference). Surfaced by a cold migration rehearsal
against the public docs. Format is unchanged — a v0.4 store validates the same,
plus this one advisory warning where a body genuinely carries a stray block.

## [0.6.1] — 2026-07-02

### Toolkit — search containment gate amortized + quick-xml security bump (format unchanged: v0.4)

Perf + dependency-security release; no format change, no new behavior. (The
first `v0.6.1` tag died at the release preflight's cargo-deny gate when the
quick-xml advisories published mid-release-day, with nothing shipped; this is
the re-cut with the bump folded in.)

- **`dbmd search` whole-store scans back at the ripgrep floor.** The 0.3.9
  security pass added a per-candidate containment gate
  (`ensure_path_within_store`) that re-canonicalized the store root and
  walked the candidate's whole parent chain via `realpath` on every file —
  at a 10k-file scan set that overhead tripled the scan (measured ~375 ms
  free-text vs the ~150 ms `rg -j1` floor; typed ~140 ms vs ~45 ms). The new
  `StoreContainment` helper (dbmd-core) keeps the identical acceptance /
  rejection semantics — the poisoned-sidecar regression tests are unchanged
  and an equivalence test pins fast path == single-shot gate across every
  candidate class — but canonicalizes the root once per search and memoizes
  parent-dir resolution, so the common candidate costs one `lstat(2)`.
  Symlink leaves, missing files, and other corners still take the full
  peel-resolution slow path.
- **CI now times the free-text scan.** `perf_budget.rs` gained a
  `BUDGET_SEARCH_FREETEXT` assertion (zero-hit unscoped search, 300 ms
  budget × the CI slack) — the regression above was invisible precisely
  because only `--type`-scoped search was budgeted.
- `tests/perf.py` — the repeated-timing driver behind `tests/PERF.md` — is
  now committed (it was a throwaway `/tmp` script when the 0.3.5 numbers
  were taken); `tests/PERF.md` re-measured on 0.6.1.

#### Security

- **Bumped `quick-xml` 0.40 → 0.41 to clear
  [RUSTSEC-2026-0194](https://rustsec.org/advisories/RUSTSEC-2026-0194) and
  [RUSTSEC-2026-0195](https://rustsec.org/advisories/RUSTSEC-2026-0195)** — two
  denial-of-service advisories against `quick-xml` < 0.41: a quadratic
  duplicate-attribute-name check on start tags (0194) and unbounded `NsReader`
  namespace allocation (0195). The exposed path was `dbmd extract` on an
  untrusted docx/epub — 0194 only in practice, since extraction uses the plain
  `Reader` (0195 affects `NsReader`, which nothing here constructs). The second,
  transitive `quick-xml` copy (0.39.4 via `calamine`, the xlsx/ods path) has no
  fixed release to move to yet; it is accepted with intent in `deny.toml` until
  `calamine` ships on quick-xml ≥ 0.41. No API or extraction-output change.

## [0.6.0] — 2026-07-01

### Format — v0.4 (additive: recommended `id` + reserved `@brain/id` address shape)

The first additive format release under the from-v0.3-forward rule. Exactly two
conventions ride in, chosen because they are the only retrofit-expensive pieces
of cross-store addressing; everything with a trust boundary or a wire (keys,
signing, feeds, grants, resolution) stays out of db.md by design.

- **`id` joins the universal frontmatter contract as RECOMMENDED, not
  required.** The recommended form is a **lowercase ULID** — 26 chars of
  Crockford base32: time-sortable, compact and YAML-clean, offline-mintable
  with zero coordination, collision-safe, widely understood. `dbmd write`
  mints one when the new file carries no `id` (an explicit `--fm id=…` wins);
  `dbmd fm init` never retrofits one. A record without an `id` stays fully
  valid — hand-written stores remain legal — and filename identity stays the
  link fallback (wiki-links still target paths). Uniqueness scope is the
  store (`DUP_ID`, unchanged). SPEC § The `id` field.
- **`@brain/id` reserved as the cross-store address shape** — a store handle,
  a slash, a record id. SPEC § Addressing (reserved) reserves the shape
  ONLY: registration, resolution, verification, and fetch are explicitly out
  of scope for db.md and belong to a future interconnect spec (link.md). A
  db.md tool encountering `@brain/id` treats it as plain text.
- **One new validation code: `FM_BAD_ID` (warning).** Fires only on an id
  that is structurally unusable as an identifier — non-scalar, empty, or
  containing whitespace (the non-scalar shape also silently escaped
  `DUP_ID`'s scalar read). The recommended ULID form is deliberately NOT a
  gate: a v0.3 store with hand-authored slug ids (e.g.
  `examples/research-wiki`) validates unchanged under v0.4, pinned by a
  regression test that sweeps every shipped example store.

### Toolkit

- New `dbmd_core::ulid` module: `mint()` (48-bit ms timestamp + 80 random
  bits, lowercase Crockford base32) and `is_ulid()`. Std-only — randomness
  comes from OS-entropy-seeded `RandomState` hasher keys mixed with the
  clock, the PID, and a per-process counter; zero new dependencies (the id's
  contract is store-scoped uniqueness with `DUP_ID` as the backstop, not
  unguessability).
- `dbmd write` mints the id on every content-file create. The write-through
  sidecar catalogs it like any other frontmatter field, so
  `dbmd query --where id=…` resolves through the existing generic filter
  path — regression-tested end to end (mint shape, explicit-id-wins,
  `--where id=` lookup, `fm init` never minting).
- The corpus-e agent eval now pins ids explicitly (`--fm id=…`, the
  documented explicit-id-wins path) — a random mint cannot be pinned by a
  byte-for-byte golden. Its EXPECTED tree gains exactly those `id` lines
  (14 record files + their `index.jsonl` projections; verified id-only).

Crate versions bump 0.5.1 → **0.6.0** (a new public module and new
write-path output).

## [0.5.1] — 2026-06-30

### Toolkit

Dogfooding a real store surfaced a silent dedup gap: a `unique:` key that
names a field the schema does not mark `required` stops checking any record
missing that field — `dedup_key` skips incomplete keys by design (SQL's
`NULLS DISTINCT` rule), so a vendorless re-entered expense sailed past
`unique: date, amount, vendor` unflagged. The skip behavior is correct; the
key declaration was the defect, and nothing surfaced it. No format change
(SPEC stays v0.3; no new validation code — the lint reuses
`DB_MD_SCHEMA_FIELD`).

- **`validate` lints `unique:` keys at the declaration.** A key field that is
  not marked `required` (or is never declared) in its `### <type>` schema now
  warns as `DB_MD_SCHEMA_FIELD`, one issue per field, anchored to the
  `### <type>` heading, with the remediation in the suggestion ("mark
  `<field>` `required`, or build the `unique:` key from required fields
  only"). Regression-tested: declared-but-optional, undeclared, and
  all-required-stays-silent.
- **SPEC documents the skip rule.** "A record missing any key field (or
  leaving it empty) is skipped — an incomplete key never collides" was
  implemented but stated nowhere an agent could read it. It is now in both
  places that define `unique:` (§ Linking → Collision detection and § The
  `DB.md` file → Schemas directives), with the discipline stated: build keys
  from `required` fields.
- **The SPEC's own example taught the foot-gun.** The canonical `### expense`
  example keyed `unique: date, amount, vendor` while leaving `vendor`
  optional — the exact shape the dogfood store copied. The example now marks
  `vendor` `required`.
- **corpus-b seeds the new warning.** `bad-db-md/` (the DB.md-structure
  fixture) gains a `### expense` schema tripping both message variants
  (`amount` undeclared, `vendor` declared but optional; 5 issues total in
  `bad-db-md.json`), and the main store's `### email` schema now declares its
  key fields `required` so the root sweep stays `DB_MD_*`-clean. Coverage:
  39 of the 48 SPEC codes seeded.

## [0.5.0] — 2026-06-29

### Toolkit (breaking — CLI read surface)

Collapsed the structured-read surface to **three primitives, one per data
model** — `query` (frontmatter fields), `search` (body text / ripgrep), `graph`
(wiki-link edges) — folding three redundant verbs into them. **Lossless:** every
capability survives, just reached through one command.

- **`dbmd index query` → `dbmd query`.** `query` already printed paths (default)
  or full records (`--json`); it now also carries `index query`'s time-window
  filters (`--updated/created-after/-before`). `index query` is removed; `index`
  keeps `rebuild` / `show`.
- **`dbmd fm query <k>=<v>` → `dbmd query --where <k>=<v>`.** The pre-write dedup
  lookup is now a `query` filter; the dedup *pattern* is documented in the SPEC
  rather than reified as its own command. `fm query` is removed; `fm` keeps
  `get` / `set` / `init`.
- **`dbmd links <target>` → `dbmd graph backlinks <target>`.** The top-level
  `links` was a byte-identical alias of `graph backlinks` (incoming wiki-links);
  removed in favor of the single `graph` axis.

Rationale: an agent generates its tool calls fresh at read-time against the
current interface — and `dbmd spec` reloads the standard into its system prompt
each run — so a pre-1.0 surface change is cheap. Collapse to the minimal,
principled shape now rather than carry compatibility cruft. `search`, the
`fm`/`index` write & maintain verbs, and the navigate verbs (`tree`, `outline`,
`sections`, `stats`, `index show`, `fm get`) are unchanged.

The `## More` footer written into an overflowing type-folder `index.md` now
points at `dbmd query --type <t> --in <layer>` (was `dbmd index query …`).

### Format

No on-disk format change: `SPEC.md` documents the three-read surface and the
dedup-via-`--where` pattern; the store schema is untouched. Format stays **v0.3**.

## [0.4.6] — 2026-06-29

### Toolkit

Third adversarial-review pass (continued). 0.4.5 shipped the loose-only
`validate` parity fix from this pass; 0.4.6 adds the remaining confirmed defects
— 30 across two parallel review rounds (find → reproduce → triple-skeptic
double-check), each fixed with a regression test. No format change (SPEC stays
v0.3).

Correctness & data-integrity:

- **Concurrent write-through.** Two concurrent writes to different type-folders
  under the same layer no longer lose a rollup row: `update_parents` now
  serializes the shared layer/root `index.md` rewrite under a store-root lock,
  and `FolderLock` waits-and-stale-breaks instead of silently degrading to
  no-lock under contention (which had reintroduced the lost update).
- **Wiki-link scanning is frontmatter-aware everywhere.** A code fence inside a
  frontmatter value no longer leaks into the body scan and swallows real
  wiki-links — fixed in `stats`, `search_by_link`/backlinks/forwardlinks/`links`
  (`extract_edge_targets`), and `rename`'s `rewrite_links_to`.
- **`rename` stays inside the content layers.** It no longer rewrites
  wiki-links in files outside `sources/`+`records/` (e.g. archived/verbatim
  copies, goldens).
- **Unicode-normalized link comparison.** NFC/NFD spellings of the same target
  now resolve as one edge in the graph (backlinks/forwardlinks/orphans), via
  `unicode-normalization` (permissive, no AI/LLM deps).
- **Query ordering.** `fm query` / `index query --limit` now path-sort before
  truncating, matching `dbmd query`, so loose files coexisting with type-folders
  return the correct subset.
- **Sharding & validation.** Single-digit-month dates (`2026-1-15`) shard to the
  correct month; wrong-case wiki-links are flagged consistently across
  case-insensitive (APFS) and case-sensitive (Linux) filesystems; `write --fm
  summary=` is honored; `parser` round-trips oversized integers inside YAML flow
  collections.
- **Log rotation.** Distinct same-minute entries are preserved across the
  active/archive boundary on both the read path (`tail`/`since`) and a fresh
  rotation that finds a lingering `.rotating` marker.

Security & robustness on untrusted input:

- **`extract` is bounded.** Wide-table HTML/EPUB/docx no longer amplify into
  multi-GB output / OOM, and a truncated `.ods` no longer hangs forever — both
  now fail fast with a typed error.
- **Frozen-page globs can't ReDoS.** A `DB.md` frozen-page pattern with many
  `**` segments no longer triggers catastrophic backtracking that hung every
  write/rename.
- **Containment.** `graph neighborhood` no longer discloses files outside the
  store through a symlinked path component; `assets paths` no longer emits
  store-escaping (`..`/absolute) recorded paths.

Other:

- `assets scan` recompacts a duplicate-line manifest (the documented
  `merge=union` recovery), and `format` no longer retypes a genuine nested
  array into a scalar wiki-link string.

### Docs

- SPEC § Validation now lists `## Folders` among the recognized `DB.md`
  sections (it was documented elsewhere and already accepted by the validator).

## [0.4.5] — 2026-06-28

### Toolkit

Fix: a store whose only content is loose files — a layer with no type-folder at
all — reported two false `INDEX_MISSING` errors from `dbmd validate --all`, even
though `dbmd index rebuild` had just produced that store and the loose file was
query-visible. The root and layer `index.md` are type-folder rollups, written
only when type-folders exist; with none, rebuild writes no rollup, but `validate`
still demanded one. `validate` now requires the root / layer `index.md` only
when that scope has type-folders — a loose-only layer's catalogue is its
`index.jsonl`, which has nothing to roll up. (Predates 0.4.4; the 0.4.4
loose-file work surfaced it by making such a store reachable and queryable for
the first time. SPEC § Loose files gains a sentence; format stays **v0.3**.)

## [0.4.4] — 2026-06-28

### Toolkit

Loose files — content placed directly at a layer root (`records/<file>.md`,
`sources/<file>.md`) with no type-folder between it and the layer — are now
catalogued in the layer's own `index.jsonl`, so structured reads see them.

Folder layout is convention, not enforcement (§ Layers), so a loose file is a
valid store state: a bulk import or a hand-edit can produce one, though
`dbmd write` never does (it routes every type to a canonical type-folder). The
index was previously built only at type-folder granularity, so a loose file was
**silently absent** from every structured surface — `dbmd query` / `index query`
could not return it, the dedup pre-write checks and `dbmd graph` did not see it,
and `dbmd validate --all` did not flag it — while `dbmd search` (ripgrep) and
`dbmd stats` (a walk) *did* see it, so the surfaces silently disagreed.

- **`dbmd index rebuild` and write-through** (`fm init` / `fm set` / `rename`)
  now catalogue loose files in a layer-level `index.jsonl` — the same complete,
  uncapped structured twin a type-folder carries, anchored at the layer dir, and
  byte-identical between the loop and sweep paths. The layer `index.md` stays a
  type-folder rollup; a layer with no loose files carries no `index.jsonl`, so
  canonical stores are byte-for-byte unchanged.
- **`dbmd query` / `index query` / `search --type` / dedup / `graph`** now see
  loose files exactly as canonical ones, with no whole-store walk (the
  sidecar-only read contract holds — `find_type_index_files_in` already walks
  the whole layer for `index.jsonl` sidecars).
- **`dbmd validate --all`** reports `INDEX_JSONL_MISSING` for a loose file
  absent from its layer `index.jsonl` (and `INDEX_JSONL_DESYNC` /
  `INDEX_JSONL_STALE` for a sidecar out of sync), so a loose file is never
  *silently* uncatalogued.
- `dbmd fm set` / `fm init` / `rename` on a loose file no longer fails with
  "file is not inside a layer/type-folder".

### Format

`SPEC.md` gains a **Loose files** subsection specifying the layer `index.jsonl`
(the corner was previously undefined). Additive and backward-compatible — no
existing store becomes invalid — so the format stays **v0.3**.

## [0.4.3] — 2026-06-28

### Toolkit

#### Changed

- **MSRV raised to Rust 1.88.** The `pdf-extract 0.12` security update
  (RUSTSEC-2026-0187, shipped in 0.4.1) pulls in `lopdf 0.42`, which uses stable
  let-chains and no longer compiles on Rust 1.85 — so the declared
  `rust-version = "1.85"` had become inaccurate (a source build on 1.85 failed).
  The declaration now matches reality (verified: 1.87 fails to compile `lopdf`,
  1.88 builds the workspace clean), and the MSRV CI job checks 1.88. Prebuilt
  binaries, `brew`, and `cargo install` on stable are unaffected.

## [0.4.2] — 2026-06-28

### Toolkit

A second parallel adversarial-review pass over the toolkit. Every defect below
was reproduced against the real binary and confirmed by an independent
refute-by-default panel, each with a regression test.

#### Fixed (security / containment)

- **`rename` now contains the `<old>` SOURCE and every rewritten linker, not
  only `<new>`.** An `<old>` (or an incoming linker) reached through an in-store
  symlink leaving the store root passed the lexical gate, so `fs::rename` MOVED
  an out-of-store file into the store (unlinking the original) and link rewrites
  wrote bytes outside the root. Both paths now pass `ensure_path_within_store`
  (completing the prior containment fix, which guarded only the destination).
- **`dbmd validate` no longer reads files OUTSIDE the store via a `..` log
  object.** A `records/../../leaky` object in a `log.md` header was inserted into
  the working set verbatim, so the default validate read and frontmatter-
  reported a file above the store root (a containment escape + existence oracle +
  field disclosure). The log-derived changed set now passes the same
  path-safety gate every other validator path uses.

#### Fixed (data loss / correctness)

- **`write` / `rename` / `link` reject a dot-prefixed INTERMEDIATE directory.**
  `records/.hidden/c.md --type contact` was honored as the type-folder; the
  record and its write-through sidecars were then invisible to every
  `.hidden(true)` sweep — silent primary-data loss that `validate --all` and
  `index rebuild` could not heal. The path gate now rejects any dot-prefixed
  component, not just the leaf.
- **`format` / `fm set` / `link` preserve large integer frontmatter.** A bare
  integer beyond `i64`/`u64` range was silently truncated to a lossy float
  (`999…9` → `1e39`) or rejected as malformed (the `(u64, u128]` band). Oversized
  literals now round-trip verbatim as strings (the import path for big numeric
  IDs).
- **Log rotation preserves genuinely-distinct same-minute entries.** A backdated
  entry byte-identical (at minute precision) to one already archived was dropped
  by a set-membership re-roll dedup. A crash-recovery marker now gates the dedup,
  so a true re-roll is still idempotent while a distinct repeat survives.
- **Log rotation no longer erases lines before the first VALID entry header.** A
  `## [`-shaped line `parse_header` rejects (a merge orphan) ahead of the first
  real entry was dropped on re-emit; it is now folded into the preserved header.
- **`log tail` / `log since` return distinct same-minute entries.** A global
  content key over-reached from the active↔archive crash-retry overlap to
  same-file duplicates; the dedup is now scoped to that overlap only.
- **`query` / `fm query` / `search --where` match float fields by value.** The
  sidecar's canonical-`f64` render discards the file spelling (`1234.00` →
  `1234.0`, `1e3` → `1000.0`), so a textual compare missed; float fields now
  compare numerically (integers keep exact matching).
- **`INDEX_SUMMARY_MISMATCH` no longer false-positives on internal whitespace.**
  A valid one-line summary with a double space / tab collapses in the rendered
  `index.md` but was compared against the raw file value — a permanent,
  rebuild-immune error. Both sides now normalize identically.
- **`extract` bounds EPUB spine amplification.** A few-KB `.epub` whose spine
  references one chapter many times pegged a CPU core and ballooned output; the
  spine length is capped, chapters are memoized, and total output is bounded.
- **`extract` counts a self-closing non-void element as flat, not nested.** An
  off-by-one read the `>` byte instead of the `/`, so `<div/>`×N tripped the
  nesting cap on a valid flat document.
- **`graph` (scoped backlinks / orphans / neighborhood) keeps edges on a
  non-UTF-8 file.** `forwardlinks`/`orphans` used an intolerant read and dropped
  every `[[…]]` edge on a stray byte (disagreeing with the lossy unscoped
  scanner); they now read lossily like the rest of the toolkit.
- **`search` time-window filter tolerates a trailing-whitespace frontmatter
  fence.** A `--- ` fence made the all-content path yield no timestamps and
  silently drop a valid, indexed file; fence matching now `trim_end()`s like the
  canonical parser (the strict `store` reader is aligned too).
- **`assets status` / `scan` saturate a poisoned manifest byte total** instead of
  aborting (debug) or wrapping (release) on an absurd `bytes` value.
- **`validate` (default scope) reports a stale `index.md` entry consistently.** A
  catalog entry pointing at a deleted file surfaced as `WIKI_LINK_BROKEN`
  ("create the target" — steering an agent to recreate deleted data) in the loop
  default while `--all` correctly reported `INDEX_STALE_ENTRY`; the loop no longer
  body-link-checks the derived catalog (index integrity is the `--all` sweep's
  job).
- **`rename` fails fast on a non-creatable destination.** A destination whose
  parent component is an existing file made `create_dir_all` fail AFTER the
  linker rewrites; the destination parent is now created up-front, so a failed
  rename leaves zero authored mutations.

#### Changed

- **`validate --all` follows symlinks like the loop default.** The sweep walked
  with no `follow_links`, so it SKIPPED a symlinked-in content file the loop
  scope checks — the authoritative superset reported fewer issues than the loop.
  Both sweep walkers now follow symlinks.
- **`validate` recognizes the `## Folders` DB.md section** (shipped in 0.4.1 in
  the parser + index, but flagged `DB_MD_UNKNOWN_SECTION` with a "delete this
  heading" remedy). `SPEC.md` now documents `## Folders` and the built-in
  date-shard defaults (the source types plus `expense`/`invoice`/`meeting`/
  `order`/`ticket`/`transaction`).

## [0.4.1] — 2026-06-28

### Toolkit

#### Changed

- **Root and layer `index.md` rollups no longer invent a per-folder
  description.** Each rollup entry previously appended the *newest member
  file's* `summary` truncated to 80 chars — a mid-word cut (`… renewal in
  fli`) that read as one member masquerading as the whole folder, and that
  churned the catalog on every write. Rollups now show
  `- [[<folder>/index|Name]] (N)` (counts only). Folder display names also
  tidy separators (`hubspot-exports` → `Hubspot exports`). Removes the dead
  `truncate` helper.

#### Added

- **Optional `DB.md ## Folders` section** — agent-authored per-folder
  display and description, surfaced in the root/layer rollups
  (`- [[…|HubSpot exports]] (N) — deal + pipeline exports`); absent ⇒ counts
  only. The tool only *surfaces* curator-authored text; it never composes a
  folder description from the folder's contents.

#### Fixed

- **`extract` no longer panics or fabricates a date on a crafted `.xlsx`.** An
  out-of-range Excel date serial (e.g. a hostile `1e308` cell) overflowed
  calamine's date math — a panic in debug, a bogus far-past date in release.
  Such serials now keep their raw value, upholding "never panics on untrusted
  input, never hallucinated text."
- **`extract` bounds HTML block-nesting depth.** A tiny but deeply-nested HTML
  file made html2text's layout run for minutes (O(depth²)); the adapter now
  refuses pathological nesting, matching the bounds the docx/epub/spreadsheet
  adapters already enforce. EPUB chapters are covered too.
- **`validate` rejects a non-scalar `meta-type`.** A `meta-type:` whose value is
  a YAML list or mapping slipped past the closed-enum check and was silently
  treated as the default `fact`; it now reports `FM_BAD_META_TYPE`.
- **`assets status` honors store containment.** It resolved manifest paths
  without the guard `scan`/`verify` use, so a poisoned/hand-edited manifest could
  report an out-of-store file as `present` (and disagree with `verify`). Status
  now resolves through `ensure_path_within_store` like the other asset ops.
- **Asset paths fold a leading `./` to one canonical record.** `./sources/x.pdf`
  and `sources/x.pdf` are the same file; they no longer split into duplicate
  manifest records (doubled byte counts, false "untracked"). Traversal/absolute
  paths are still hard-rejected.
- **`log since` / `log tail` no longer risk dropping entries from a malformed
  archive name.** A hand-created `log/2026-00.md` / `2026-13.md` (out-of-range
  month) could be pruned by the newest-first early-break; such names are now
  scanned, not parsed as a real month.
- **`rename`'s "N files rewritten" count excludes the derived `index.md`
  catalog** (it was inflating the count with a regenerated artifact).
- **Removed the stale `wiki` layer from agent-facing surfaces** — eight `--help`
  strings, the `graph --in` error, and the CLI README still advertised `wiki`,
  removed in v0.4.0, so an agent trusting them constructed a hard-rejected
  `--in wiki`.
- **SPEC fixes:** the worked index example used `scope: folder` (not in the
  normative `root|layer|type-folder` enum) → `type-folder`; documented the
  shipped top-level `dbmd query`.

#### Security

- **`rename`, `link`, and `fm set` / `fm init` now enforce store containment**
  like `write` does. A destination or target reached through an in-store symlink
  pointing outside the store passed the lexical-only gate and could move or
  rewrite a file **outside the store root** (plus catalog a stale entry pointing
  at it). They now resolve the path through `ensure_path_within_store` and refuse
  with `PATH_OUTSIDE_STORE`.
- **Bumped `pdf-extract` 0.10 → 0.12 (pulls `lopdf` ≥ 0.42) to clear
  [RUSTSEC-2026-0187](https://rustsec.org/advisories/RUSTSEC-2026-0187)** — an
  unbounded-recursion stack overflow in `lopdf`'s PDF parser that a ~21 KB
  deeply-nested PDF could trigger to abort the process (a `SIGABRT` that
  `catch_unwind` cannot contain). `dbmd extract` on an untrusted PDF was exposed;
  the patched parser bounds nesting depth. No API or extraction-output change.

## [0.4.0] — 2026-06-23

### Format — v0.3 (breaking: three layers collapse to two, plus a `meta-type` field)

The store model drops from three layers to two. The `wiki/` layer is **removed**;
its synthesis content becomes ordinary `records/` files carrying a new frontmatter
field, `meta-type`. Breaking is acceptable here because the only migrator is an
agent and db.md is pre-adoption — the blast radius is small, and the migration is
agent-driven (there is no `dbmd migrate` command; an agent moves the files and
rewrites links per the new contract).

#### Changed

- **Two layers, not three.** `sources/` (evidence) + `records/` (everything the
  agent authors). The hard boundary that earns its place — evidence vs.
  agent-authored — stays as the only folder-level layer split; the soft
  records-vs-wiki split dissolves into a field.
- **`meta-type` carries what `wiki/` used to.** A new closed-enum record field:
  `fact | operational | conclusion`. Absent ⇒ `fact` (additive: old records read
  as facts, and `--where meta-type=fact` matches an un-annotated record).
  `conclusion` is the old wiki synthesis. Folders stay organized by `type` (the
  open noun); `meta-type` is a cross-cutting queryable field, not a folder. Query
  the synthesis layer with `dbmd query --in records --where meta-type=conclusion`
  (replacing the old `--in wiki`). New validation code `FM_BAD_META_TYPE` rejects
  a value outside the closed set; sources carry no `meta-type`.
- **Sources gain a testimonial kind.** Alongside documentary sources (emails,
  PDFs, imports) there is a new `note` source type with a `told_by` field, for
  "a person told the agent X." This closes the gap where chat-asserted facts had
  no captured evidence. `note` date-shards like other source streams
  (`sources/notes/<YYYY>/<MM>/`).
- **Source-first provenance is a discipline, not an enforced invariant.** Every
  record *should* trace to a source (documentary or testimonial), reconstructed
  on demand by the agent rather than materialized as mandatory per-record links.
  Ephemeral testimony is the one thing captured at write time (you cannot
  reconstruct an unsaved conversation) — as a `note`, coupled to the create/edit
  it drives. The toolkit ships no source-less-record check, so this is a curator
  contract the agent keeps, not a mechanical guarantee.
- **Single-writer moves onto `meta-type: conclusion` records.** The single-voice
  synthesis contract was a path-checkable layer boundary (the `wiki/` layer); it
  is now a per-file, frontmatter-conditional, prose-only rule inside the
  many-writer `records/` layer. This is a deliberate enforceability downgrade,
  mitigated by db.md's single-agent-per-store assumption. `dbmd` does not
  path-guard it; the agent honors it.
- **Migration shape.** Move `wiki/<topic>/*` → `records/<realtype>/*` with
  `meta-type: conclusion` and a real `type` (e.g. `concept`, `profile`,
  `playbook`, `theme`, `synthesis`, `account`) in place of the retired
  `wiki-page` type; update each `DB.md` (agent instructions, frozen-page paths,
  schemas) to drop `wiki/` and reference `records/` + `meta-type`. An agent does
  this; there is no migration command.

Toolkit impact: this is a breaking 0.x change; the crate is bumped to **0.4.0**
for this release (from 0.3.10), and the cli's `dbmd-core` cross-dependency is
raised to match. The format header moves v0.2 → v0.3.

## [0.3.10] - 2026-06-17

### Added

- **Asset layer** (`dbmd assets`) for raw binary evidence too heavy for Git
  (PDFs, recordings, large exports). A content file declares a binary via an
  `asset:` / `assets:` frontmatter key; the store-root `assets.jsonl` manifest
  records each asset's store-relative path, SHA-256, size, media type, the
  declaring wrapper(s), and whether it is required. The manifest is a pure,
  byte-for-byte-rebuildable projection (the analog of `index.jsonl`): it carries
  no provider URI and no local-presence flag, so it stays portable and
  provider-agnostic; moving the bytes is out of scope for db.md by design.
  New commands: `dbmd assets scan` (hash declared files, write the manifest),
  `verify` (the byte-completeness gate; `--quick` = presence+size, default =
  full re-hash), `status` (present/missing report), and `paths` (the VCS-neutral
  path list for an ignore mechanism). Five additive validation codes
  (`ASSET_MANIFEST_MALFORMED`, `ASSET_UNDECLARED`, `ASSET_WRAPPER_BROKEN`,
  `ASSET_MANIFEST_ORPHAN`, `ASSET_PATH_IS_CONTENT`), checked in the `--all`
  sweep — text-only, so a byteless fresh clone still passes `validate` (byte
  presence is `dbmd assets verify`, never `validate`). `SPEC.md` gains a
  `## Assets` section. Asset paths are validated store-relative (no `..`, no
  absolute) wherever the manifest is read.

### Changed

- Migration guidance now reconciles the surrounding system, not just the data.
  The quick-start prompt (README, llms.txt) tells the agent that moving an
  existing knowledge base into the store is only half the job: it must also find
  whatever already connects to that knowledge base (skills, commands, scripts)
  and update them to point at the new store, so nothing keeps reading the old
  location. It also preserves each source's provenance, verifies nothing was
  lost, and leaves no long-lived migration-map artifacts behind (git is the
  audit and rollback trail).

## [0.3.9] — 2026-06-14

A launch-readiness correctness and security release. An adversarial code
review of the toolkit surfaced reproduced defects across the codebase; this
release fixes all of them, plus the issues a follow-up adversarial review of
the fixes themselves found. The format is unchanged (still v0.2), and the
public `dbmd-core` API stays backward-compatible (only additive helpers).

### Fixed

**Silent data loss (critical):**

- `dbmd write` to a type-folder `index.md` / `index.jsonl` no longer lets the
  write-through catalog destroy the just-written record; reserved catalog
  filenames are refused at the write surface, at any folder depth.
- `dbmd index rebuild` no longer deletes user content files named `index.md`
  inside date shards, and an abort on one malformed file no longer destroys
  existing catalogs or leaves the store in a permanently unfixable validation
  state.
- A `dbmd log` note whose line looks like an entry header
  (`## [<date>] <kind> | <obj>`) can no longer fabricate phantom entries or
  corrupt the append-only log on rotation — fixed both at the write path
  (escaping) and in the reverse reader (a block-boundary header-scan bug).

**Store→host security boundary:**

- `dbmd search --type` / `--where` no longer reads files outside the store via
  a crafted `index.jsonl` sidecar path (path traversal / exfiltration).
- `dbmd write` and `dbmd extract --out` no longer write outside the store
  through a symlinked directory anywhere in the path (parent included).
- Graph traversal and `dbmd stats` no longer dereference `..` wiki-link
  targets outside the store root.

**Frontmatter integrity:**

- Sequence/mapping values on universal keys (`summary`, `status`, `type`,
  `tags`) and non-string YAML keys now round-trip verbatim on rewrite instead
  of being silently deleted or rewritten to debug form; nested plain string
  lists are no longer fabricated into wiki-links; a fence written with a
  trailing space is read consistently across every surface.

**Validation correctness:**

- Null / non-scalar `created`, `updated`, and schema `required` values are now
  caught instead of bypassing checks; unreadable (non-UTF-8) files are
  reported rather than silently passing; wiki-links inside fenced code are no
  longer flagged; `validate --all` no longer skips in-layer `log/` folders;
  links to existing non-`.md` source files are no longer false-flagged as
  broken; `Shape::Url` / `Shape::Email` edge cases corrected.

**Graph / index / extraction / query / render:**

- Link-edge detection now agrees across graph, `stats`, `rename`, and
  `validate` on fenced code, letter case, and surrounding whitespace, so
  `rename` no longer rewrites fenced documentation examples or misses real
  links, and backlinks no longer over- or under-report.
- Index rollup `(N)` counts stay consistent between full rebuild and
  write-through; multi-line summaries no longer corrupt `index.md`.
- docx / EPUB / spreadsheet extraction preserves XML entities, percent-encoded
  hrefs, spreadsheet dates, and literal bracketed / `#` text.
- `query --type`, `tree --type`, default-summary heading detection, and
  `sections` / `outline` source-relative line numbers corrected.
- `updated` is now auto-maintained on `fm set`, `link`, and `rename`.

**Filesystem / packaging:**

- `write_atomic` preserves destination file permissions instead of resetting
  them; `install.sh` upgrades atomically (no truncated binary on a
  cross-filesystem move) and honors `DBMD_BASE_URL`; a flaky macOS
  test-harness mtime guard was removed; the runtime `log.md.lock` advisory
  lock is git-ignored.

## [0.3.8] — 2026-06-13

A docs-in-binary release. No code behavior changes.

### Changed

- Setup guidance now makes a store **version-controlled by default**. The
  README quick-start prompt, `llms.txt`, and `dbmd spec` (embedded SPEC §
  "Creating a store") tell an agent to create the store inside the current
  repo when its data lives there, otherwise `git init` the store or use a
  synced folder — never to drop it at a bare, unversioned global path, and
  never to move repo-owned data out without the operator's say-so. A
  machine-global `~/db` is positioned as a symlink to the real, versioned
  store, not as the store's bare home. This is a design default, not a
  preference; the operator opts out explicitly for a throwaway store. The
  format is unchanged (still v0.2).

## [0.3.7] — 2026-06-11

### Changed

- `dbmd write` now uses a reusable `dbmd-core` create-new durable writer
  (`write_atomic_new`) instead of a CLI-level empty sentinel. Existing
  `PATH_COLLISION` behavior is unchanged, but the collision guarantee now lives
  beside the core atomic write primitive and no placeholder file is created.

## [0.3.6] — 2026-06-10

A docs-in-binary release. No code behavior changes.

### Changed

- `dbmd spec` prints the current contract: the embedded SPEC picks up the
  stack-collapse thesis, the agent-operated files-as-database framing, the
  sharded write surface, and the claims audit that landed since 0.3.5. The
  same tightening runs across the README, llms.txt, and the crate summaries
  shipped to crates.io.
- `dbmd --help` and the installer header carry the current tagline, "the open
  standard for databases in plain files", matching the README, SPEC, llms.txt,
  and crate descriptions.
- README: the quick start is now a prompt you hand to an agent, and a new
  "Safe to paste" section documents the verifiable install chain (SHA256SUMS
  verification, CI-from-tag builds, signed build-provenance attestations,
  Trusted Publishing). llms.txt carries the same audit story.

## [0.3.5] — 2026-06-09

A correctness-and-robustness release. An adversarial multi-agent review of the
toolkit surfaced 27 confirmed defects (7 high, 11 medium, 9 low); all are fixed
here, each guarded by a regression test that reconstructs the trigger and fails
against the prior code. No behavior changes outside the named bugs.

### Fixed

- **`dbmd format` silently deleted universal frontmatter fields.** A
  `type`/`id`/`summary`/`status` written as a bare YAML scalar that parses as a
  number, bool, or null (e.g. `id: 100`, `summary: 2026`, `status: 0`) was
  dropped on parse and erased on the next `format`, while `validate` reported
  the file clean. The parser now coerces these to their string form the way
  `validate`/`store`/`index` already do, so all surfaces agree and the field
  round-trips.
- **`validate --all` false-failed on a clean store.** Any `summary` containing
  ` · ` (a middle dot) tripped a spurious `INDEX_SUMMARY_MISMATCH` (exit 6); the
  index-entry summary is now matched against the renderer's real, double-spaced
  `  ·  #tag` suffix instead of the first ` · `.
- **Log month-rotation could duplicate entries.** A crash or I/O error between
  the archive write and the active-file trim, followed by the natural retry,
  permanently duplicated the prior month's entries. Rotation is now
  crash/retry-idempotent.
- **`dbmd extract` could be OOM-killed by a crafted spreadsheet.** A small
  `.xlsx`/`.ods`/`.xlsb` declaring a huge sheet range forced an unbounded dense
  allocation. The spreadsheet adapter now bounds it, returning a typed error on
  untrusted `sources/` input.
- **`--type` queries dropped records in non-canonical type-folders;** **`graph
  backlinks --type` under-reported;** **structured `search` aborted on a single
  stale sidecar entry;** **`rename` left a half-applied state on partial
  failure** (now rolls back). Plus medium/low fixes across working-set and
  post-rotation validation, the `graph neighborhood` traversal bound, write
  TOCTOU, `write_atomic` temp-file cleanup, summary truncation on UTF-8
  boundaries, and several CLI flag-placement / exit-code / help-text mismatches.

### Changed

- **Cross-module consistency.** A leading UTF-8 BOM is now tolerated uniformly
  by the parser, validator, and graph frontmatter readers (previously only some
  accepted it); `dbmd search` decodes matched lines lossily so a single invalid
  UTF-8 byte can no longer abort a scan; and `dbmd graph neighborhood` routes
  `--limit` (default 200) into the bounded traversal rather than only truncating
  the printed result.

## [0.3.4] — 2026-06-09

### Fixed

- **Broken-pipe panic when a reader closed `dbmd`'s output early.** Piping a
  command into a consumer that exits first (`dbmd spec | head`, `dbmd search …
  | grep -q`) made `print!`/`println!` panic on the closed pipe and exit `101`
  with a Rust backtrace. `dbmd` now stops cleanly with exit `0` when the reader
  on its stdout has gone away, the standard Unix behavior for a producer whose
  consumer has left. The v0.3.3 release smoke test caught it (`dbmd spec | head
  -20` under `set -o pipefail`); a regression test (`tests/broken_pipe.rs`) now
  locks the behavior in.

## [0.3.3] — 2026-06-04

### Docs

- **Explicit agent flow in every surface.** The skill, the SPEC
  (§ "How an agent uses db.md"), and `llms.txt` now open with the four-move
  path — discover → `dbmd spec` → `DB.md` → operate — so the path is
  unmistakable the moment an agent reads any of them.
- **Corrected store creation: the agent writes `DB.md`; there is no `dbmd init`.**
  The SPEC, README, and CLI README documented `dbmd fm init DB.md` as the way to
  initialize a store, but that command refuses on a directory with no `DB.md`
  (chicken-and-egg), and store creation is agent/operator-authored **by design**,
  not a tool command. Replaced the bogus one-liner with the real method (write a
  `DB.md` with `type: db-md` + `scope` + `owner`) and stated the thin-tool
  principle explicitly: `dbmd` plumbs (validate / index / query / link) and never
  scaffolds what a capable agent authors. Also documented that `scope`/`owner`
  are required (enforced by `dbmd validate --all`), which the SPEC understated.

## [0.3.2] — 2026-06-04

### Removed

- **`dbmd install-skill` / `dbmd uninstall-skill`.** The installer is text:
  `dbmd spec` + the repo-root `llms.txt` are the contract, and the open-format
  skill ships in the repo at `skills/db-md/SKILL.md`. Placing that skill is
  generic file work — copy it, use your harness's own skill installer (Codex's
  `skill-installer`, a Claude Code plugin), or tell your agent to. db.md no
  longer ships per-harness install code: it was the one thing coupled to harness
  internals (and the thing that broke when Codex moved its skills directory). The
  mechanism is generic text plus a capable model — nothing to maintain or drift.

### Docs

- **Repo-root `llms.txt`** — an agent-readable entry point at the top of the
  repo, in the [llms.txt](https://llmstxt.org) spirit: the installer is text. An
  agent (or a human) reads one plain file to learn what db.md is and how to
  install, integrate, and operate a store.
- **Docs reframed around the text path.** README, TOOLS.md, SPEC.md, and
  `llms.txt` present one model: `dbmd spec` is the single source of truth;
  persistence is a skill *file* you place (or your agent / harness installer
  places), not a `dbmd` command. Nothing inlines the SPEC, so nothing drifts.

### Fixed

- Preserved the declared Rust 1.85 MSRV, end to end. The direct `zip` dependency
  stays on the 7.2 line (`zip` 8.x now requires Rust 1.88), and the test suite
  again compiles on 1.85 — a handful of `PathBuf == *"…"` comparisons needed
  `PartialEq<str> for PathBuf`, which std only added after 1.85, so they now use
  `Path::new(…)`. Verified with a real `cargo +1.85 build` and `test`.

## [0.3.1] — 2026-06-03

A world-class hardening pass — a deep adversarial audit of every core module
(parser, validate, store, index, log, graph, stats, summary) plus the two gaps
left open at 0.3.0. Every finding was adversarially verified and fixed with a
regression test; the toolkit stays clippy-clean (`-D warnings`) and
`unsafe`-free.

### Format (additive to v0.2)

- **`shard: by-date | flat` schema directive** — on a `### <type>` block in
  `DB.md ## Schemas`, declare whether that type's records are date-sharded on
  disk or kept flat. It overrides the built-in default, so a custom event type
  opts into sharding the generic v0.2 way, and any type can force flat.

### Toolkit

#### Fixed

- **Schema validation no longer silently accepts a non-scalar value.** A shape-
  or enum-constrained field holding a YAML list or mapping now flags
  `SCHEMA_SHAPE_MISMATCH` instead of skipping the check entirely.
- **Frontmatter parsing no longer panics** on a YAML-tagged top-level mapping
  (a `!tag` on the frontmatter) — it is handled, never aborts the process.
- **`validate` flags a type-folder `index.md` entry that is missing its summary
  text** when the file has a `summary` (`INDEX_SUMMARY_MISMATCH`), per the SPEC.
- **Content files named `log.md` / `DB.md` inside a layer are no longer dropped**
  from store walks — those names are reserved only at the store root, so a
  `records/…/log.md` is real content to `validate` / `index` / `stats`.
- **`dbmd stats` orphan count now agrees with `dbmd graph orphans`** for
  self-linking files (a self-link is not a graph edge in either surface).
- **`summary_template` interpolates `{tags}` / `{created}` / `{updated}`** — the
  typed universal fields, which previously rendered empty.
- **A bare `enum` schema modifier** (`enum, a, b`, no colon) no longer includes
  the keyword `enum` itself as an allowed value.
- **`dbmd index rebuild --folder` cascades to the layer and root rollups**
  instead of leaving stale counts a later `validate` would flag as an index
  desync — consistent with `rebuild` and the write-through path.
- **`index.jsonl` paths are written OS-independently** (forward slashes), so the
  catalog is byte-portable across platforms (a Windows-written store cloned onto
  POSIX and vice versa).

#### Internal

- The atomic, **durable** write for primary data (content records, `log.md` and
  its archives, link rewrites) is now one shared `dbmd_core::write_atomic`
  primitive (temp file + fsync + rename + parent-dir fsync) instead of four
  near-identical copies. The rebuildable `index.md` / `index.jsonl` keep their
  intentionally lighter, atomic-but-not-fsync write — a crash-lost catalog entry
  is recovered by `dbmd index rebuild`, so a per-write fsync there would be cost
  without benefit.

## [0.3.0] — 2026-06-03

### Toolkit

#### Added

- **`dbmd install-skill`** / **`dbmd uninstall-skill`** — install (or remove) the
  cross-agent [Agent Skill](https://agentskills.io) that teaches a local coding
  agent to operate a db.md store with `dbmd`. One source, every agent: the skill
  is authored once at `skills/db-md/SKILL.md`, embedded in the binary, and dropped
  into each agent's skills dir in the open `SKILL.md` format — Claude Code
  (`~/.claude/skills/db-md/SKILL.md`) and Codex (`~/.codex/skills/db-md/SKILL.md`),
  the same file, frontmatter and all. With no `--target` it points every detected
  agent in one command (`--target claude-code|codex` narrows to one). The
  persistent sibling of `dbmd spec`: where `spec` loads the contract for one
  session, `install-skill` drops a skill the agent discovers on every future
  session, and `uninstall-skill` removes exactly what it wrote (preserving any
  user-created siblings). The skill body is a thin pointer that runs `dbmd spec`,
  the single source of truth — it never inlines the SPEC, so it cannot drift.
- Validation now emits `FM_MISSING_CREATED` and `FM_MISSING_UPDATED` when a
  content file omits the universal timestamps.

#### Changed

- `dbmd validate` falls back to a per-file content sweep when the default
  working-set has no logged changed objects, avoiding vacuous clean reports on
  fresh stores or externally edited stores with no `log.md` entry.
- `dbmd index show --json` and `dbmd index rebuild --dry-run --json` now emit
  machine-parseable envelopes instead of ignoring global JSON mode.

#### Fixed

- Mutating CLI paths (`write`, `link`, `rename`, `fm`, and scoped `index`
  operations) now reject absolute/traversal paths outside the opened store.
- Core write paths use exclusive, same-directory temp files before atomic
  rename, closing predictable-temp clobber races.
- Normal CLI commands fail closed on unreadable or malformed `DB.md` instead of
  silently using a default config.
- `fm init` can initialize raw markdown files that were externally dropped into
  the store.
- Schema `default <value>` modifiers are applied by `write` and `fm init`
  without overwriting explicit fields.
- Schema-declared `link to` fields now still warn on `.md` wiki-link targets.
- `index rebuild --layer <layer>` repairs child type-folder `index.jsonl`
  artifacts before rendering the layer rollup.
- Summary templates and `index.jsonl` projection normalize unquoted wiki-link
  YAML shapes consistently.
- `graph orphans` counts only links to existing store files as graph edges.
- Skill install/uninstall refuses to overwrite or remove unmanaged agent
  instruction files.

### Format — v0.2 (breaking: the type model is now generic)

The spec no longer ships a built-in type vocabulary. `type` is a free-form
label, and schema enforcement comes solely from the store's own
`DB.md ## Schemas`. The `contact` / `expense` / … types are now illustrative
**examples**, not normative. **Migration:** a store that relied on the old
implicit schemas (e.g. `contact.company` enforced as a `records/companies/`
link, or the type-specific dedup) must declare those rules explicitly in
`## Schemas` — copy the example schema pack from SPEC § Example types.

#### Added

- **`unique:` schema directive** — declare a uniqueness constraint over one or
  more fields (`- unique: email` / `- unique: date, amount, vendor`);
  collisions warn as the new generic `DUP_UNIQUE_KEY` code. Wiki-link fields
  compare by target; list fields compare as a sorted set.
- **`summary_template:` schema directive** — a `{field}`-interpolation pattern
  for a type's default `summary` (e.g. `summary_template: {role} at {company}`),
  replacing the old built-in per-type composers.

#### Removed

- The implicit / built-in per-type schemas — no type carries an enforced schema
  unless `## Schemas` declares it.
- Seven validation codes: `LAYER_TYPE_MISMATCH` and the six type-specific
  collisions (`DUP_CONTACT_EMAIL`, `DUP_COMPANY_DOMAIN`, `DUP_EXPENSE_TUPLE`,
  `DUP_INVOICE_TUPLE`, `DUP_EMAIL_REINGEST`, `DUP_MEETING_TUPLE`) — superseded by
  the schema-driven `DUP_UNIQUE_KEY`. The live SPEC table now has 40 codes.
- The hard-coded per-type `summary` composers, and the `dbmd stats`
  recognized-vs-custom type split (every type is now the store's own).

#### Changed

- Folder placement is no longer enforced by type (`LAYER_TYPE_MISMATCH` is
  gone); the three-layer layout stays a convention.

Toolkit impact: this is a breaking 0.x change; the crate is bumped to **0.3.0**
for this release.

## [0.2.4] — 2026-06-01

- **Release process documented.** Added `RELEASING.md` (a cold-start release
  runbook) and `AGENTS.md`, and referenced the tagged `SPEC.md` v0.1 from the
  README. The `crates-io` publish environment no longer requires a manual
  approval click (solo maintainer). No functional changes to the toolkit.

## [0.2.3] — 2026-05-30

- **First release published from CI via Trusted Publishing.** Both
  crates are published by the `release.yml` GitHub Actions workflow on a
  version tag, using crates.io Trusted Publishing (OIDC, no stored API
  token), with SLSA build-provenance attestations on the release
  binaries. No functional changes to the toolkit. See
  [RELEASING.md](RELEASING.md).

## [0.2.2] — 2026-05-30

- **Crate READMEs.** `dbmd-core` and `dbmd-cli` now ship `README.md`
  files (with the `readme` field set) so their crates.io pages render
  documentation. No functional change to the toolkit.

## [0.2.1] — 2026-05-30

- **Self-contained standard.** db.md stands alone with no external
  project dependency: the spec, the `dbmd` toolkit, and the docs make
  no reference to any other standard or platform.
- **Vendor-neutral distribution.** Install via `cargo install dbmd-cli`,
  the Homebrew tap (`brew install carloslfu/tap/dbmd`), or the prebuilt,
  checksummed, provenance-attested tarballs on the GitHub releases page.
- **Security reporting** via GitHub private vulnerability reporting.

## [0.2.0] — 2026-05-29

The all-Rust rewrite. db.md becomes a single deterministic binary with
zero AI dependencies, and the store model settles into three layers
plus one config file.

### Added

- **One Rust binary, `dbmd`** (git / cargo / kubectl shape) doing every
  db.md-specific file/data operation: read, write, search, validate,
  extract, graph, index, log.
- **Embedded ripgrep** via the `grep` + `ignore` crates — fast search
  with no separate `rg` to install and no shelling out.
- **Built-in document extraction** (`dbmd extract`) for PDF, docx,
  xlsx, epub, and html via permissively-licensed Rust crates — no GPL
  `pdfgrep`, no AGPL `rga`.
- **`dbmd-core` library crate.** All logic lives in the library; the
  binary is thin arg-parse/format wrappers. `cargo add dbmd-core` to
  build db.md-aware Rust tools.
- **`records/` layer.** The store is now three layers — `sources/`
  (raw evidence), `records/` (atomic typed data), `wiki/`
  (curator-synthesized narrative).
- **Single `DB.md` config file** with parseable, validated sections:
  `## Agent instructions`, `## Policies` (`### Frozen pages`,
  `### Ignored types`), and `## Schemas` (`### <type>` field
  definitions with `required` / shape / `link to` / `default` / `enum`
  modifiers). Frozen-page writes are refused by `dbmd validate`.
- **Hierarchical `index.md` catalog**, maintained write-through by the
  write commands, with a 500-entry cap per node and a `## More`
  overflow footer.
- **Append-only `log.md`** with monthly rotation into
  `log/<YYYY-MM>.md`.
- **Required `summary` frontmatter field** on every content file — the
  single source of truth each `index.md` reads to build its catalog.
- **Six-step agent session lifecycle** and the full curator contract,
  documented in `SPEC.md` (§ The agent session, § The curator
  contract).
- **O(changed) vs. O(store) discipline.** Loop ops (search, fm,
  backlinks, write, log tail, working-set `validate`) stay flat as the
  store grows; sweep ops (`validate --all`, `index rebuild`, `stats`,
  whole-graph queries) run off the interactive loop. Performance
  budgets are baked into the toolkit contract.
- **Distribution**: a crates.io crate (`cargo install dbmd-cli`), a
  Homebrew tap (`brew install carloslfu/tap/dbmd`), and prebuilt,
  checksummed, provenance-attested tarballs on the GitHub releases page.
- **`dbmd spec`** prints the bundled canonical spec — install the
  binary, run `dbmd spec` to read the standard and load it into an
  agent harness's system prompt.
- **Mechanical license + zero-AI enforcement.** `cargo deny` over the
  whole resolved tree plus a `license_policy` test over the shipped
  closure: MIT / Apache-2.0 / BSD / Unlicense / MPL / Zlib /
  Unicode-3.0 only, and a banned-crate list covering provider SDKs and
  every embeddings / vector / ANN crate.

### Changed

- **The agent harness is bring-your-own.** "Curator" is a role any
  agent (Claude Code, Codex, a custom loop) plays by reading the spec
  and driving `dbmd` subcommands. db.md ships no LLM runtime and no API
  keys.
- **Wiki-links require the full store-relative path**
  (`[[records/contacts/sarah-chen]]`). Short-form links are now a
  validation error.
- **Atomic typed data moved from `wiki/<plural>/` to
  `records/<plural>/`.** The `wiki/` layer is now narrative synthesis
  only; the typed rows live in `records/`.

### Removed

- **The Go toolchain.** The five Go binaries (`dbmd`, `dbmd-curator`,
  `dbmd-file-watcher`, `dbmd-email-imap`, `dbmd-mcp-fetcher`), the Go
  `parser` package, `go.mod` / `go.sum`, and the v0.1 reference
  ingesters are gone.
- **The `dbmd-curator` binary and any LLM backend.** Curation is the
  agent's job using `dbmd` primitives — no curator binary, no
  `dbmd curate` subcommand, no `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`
  handling anywhere in the toolkit.
- **The reference ingesters.** Getting data in is "land a file under
  `sources/`, then `dbmd fm init`" composed with the tools you already
  have (`mbsync`, `rsync`, `curl`, cron), plus `dbmd write` for
  tool-produced text.
- **The `rules/` folder.** Its `curator.md`, `policies.md`, and
  `schemas/` content folds into the single `DB.md` config file.
- **The curator + ingester `docker-compose.yml`** (it ran the dropped
  binaries with provider API keys).

## [0.1.0]

The original Go reference implementation: the `dbmd` CLI plus the
`dbmd-curator` / `dbmd-file-watcher` / `dbmd-email-imap` /
`dbmd-mcp-fetcher` binaries, a `sources/ wiki/ rules/` store model, and
a Go `parser` package. Superseded by 0.2.0.

[0.2.0]: https://github.com/carloslfu/db.md/releases/tag/v0.2.0
[0.1.0]: https://github.com/carloslfu/db.md/releases/tag/v0.1.0
