//! Integration tests that bind `dbmd-core`'s store + graph + DB.md-schema
//! behaviour to the **committed corpora on disk** (`tests/corpora/`), not to
//! synthetic temp dirs.
//!
//! The unit tests inside `store.rs` / `graph.rs` / `parser.rs` exercise these
//! same primitives against `tempfile` fixtures the test builds byte-by-byte.
//! That proves the logic in isolation but leaves the checked-in corpora —
//! `corpus-a-canonical` (happy path) and `corpus-b-edges` (designed-to-fail) —
//! unexercised by the parser/store/graph layer (plan
//! `plans/db-md-rust-toolkit.md` lines 237 / 252 / 259). These tests close that
//! gap: they `Store::open` the real corpora and assert `Store::walk*` /
//! `Store::find_links_to` / `graph::backlinks` / `graph::forwardlinks` /
//! `parse_db_md` against the **intent** of each fixture (what the SPEC and the
//! corpus's own `DB.md` / `EXPECTED/README.md` say MUST be true), never against
//! whatever the tool happens to emit.
//!
//! The corpora are read-only here — every assertion is a pure read; nothing is
//! written, so the committed fixtures are never mutated.
//!
//! ## How the expected values are derived (and why each test would catch a bug)
//!
//! Every golden below is hand-derived from the corpus files + the SPEC, then
//! chosen so a *plausible* regression flips it:
//! - **walk**: the exact `sources/` (6) content set is listed in full, the four
//!   `meta-type: conclusion` records (former wiki pages) are confirmed in the
//!   `records/` layer, and the whole-store content count is pinned (515). A walk
//!   that leaked an `index.md` / `index.jsonl` catalog, dropped a date-shard, or
//!   descended a hidden dir would change the set or the count.
//! - **find_links_to / backlinks**: the incoming-edge set for the central
//!   `records/contacts/sarah-chen` node is enumerated by hand from the five
//!   content files (plus the `index.md` catalog, which `find_links_to` includes
//!   and `backlinks` excludes). Missing the frontmatter `company: [[…]]` edge,
//!   the `attendees:` block-list edges, or the `|display` form would shrink the
//!   set; conflating the two catalog policies would cross them.
//! - **schema parse**: the `meeting.attendees` field is `link to
//!   records/contacts/` in `corpus-b` but a bare `required` in `corpus-a` — a
//!   parser that ignored `link to` (or read the wrong store's DB.md) would make
//!   the two stores look identical, which these tests forbid.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use dbmd_core::graph::{backlinks, backlinks_filtered, forwardlinks};
use dbmd_core::parser::{parse_db_md, Shape};
use dbmd_core::store::{Layer, Store};

// ── Corpus location (read-only) ─────────────────────────────────────────────

/// The repo-root `tests/corpora` directory, resolved from this crate's manifest
/// (`crates/dbmd-core` → `../../tests/corpora`). Committed, read-only fixtures.
fn corpora_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("corpora")
}

/// Open the canonical happy-path store (`corpus-a-canonical`) from disk.
fn open_corpus_a() -> Store {
    let path = corpora_dir().join("corpus-a-canonical");
    Store::open(&path).expect("corpus-a-canonical is a db.md store (has DB.md)")
}

/// Open the designed-to-fail store (`corpus-b-edges`) from disk.
fn open_corpus_b() -> Store {
    let path = corpora_dir().join("corpus-b-edges");
    Store::open(&path).expect("corpus-b-edges is a db.md store (has DB.md)")
}

/// Normalize a list of store-relative paths to sorted `/`-joined strings, so an
/// assertion reads cleanly and is OS-separator-independent.
fn as_sorted_strings(paths: &[PathBuf]) -> Vec<String> {
    let mut v: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    v.sort();
    v
}

/// Same as [`as_sorted_strings`] but for the literal `&str` goldens, so the two
/// sides of an `assert_eq!` are the same owned type.
fn owned(strs: &[&str]) -> Vec<String> {
    strs.iter().map(|s| s.to_string()).collect()
}

// ── store: walk / walk_layer (SWEEP) on the real corpus ─────────────────────

/// `Store::walk_layer(Sources)` over `corpus-a` returns exactly the six source
/// content files, recursing the `<YYYY>/<MM>` email shards and skipping every
/// `index.md` / `index.jsonl` catalog.
///
/// Intent: the canonical store's `sources/` holds two flat `docs/` PDFs and four
/// date-sharded `emails/`. A walk that failed to recurse the shards would miss
/// the four emails; one that leaked catalogs would gain `index.md` entries.
#[test]
fn corpus_a_walk_sources_layer_is_the_six_known_files() {
    let store = open_corpus_a();
    let got = as_sorted_strings(&store.walk_layer(Layer::Sources).expect("walk sources"));
    assert_eq!(
        got,
        owned(&[
            "sources/docs/2026-03-15-northstar-msa.md",
            "sources/docs/2026-04-30-aws-invoice.md",
            "sources/emails/2026/04/2026-04-03-figma-renewal-notice.md",
            "sources/emails/2026/04/2026-04-28-aws-invoice-available.md",
            "sources/emails/2026/05/2026-05-12-marcus-intro.md",
            "sources/emails/2026/05/2026-05-22-elena-renewal.md",
        ]),
        "sources walk must recurse date-shards and exclude index.md/index.jsonl"
    );
}

/// The four curator-authored synthesis pages are `meta-type: conclusion`
/// records under `records/` (the `wiki/` layer was removed). They live across
/// three conclusion topic-folders (`profiles/`, `projects/`, `synthesis/`) and
/// are surfaced by the Records-layer walk alongside the atomic records.
#[test]
fn corpus_a_conclusion_records_are_in_the_records_layer() {
    let store = open_corpus_a();
    let records = as_sorted_strings(&store.walk_layer(Layer::Records).expect("walk records"));
    for conclusion in [
        "records/profiles/elena-rodriguez.md",
        "records/profiles/sarah-chen.md",
        "records/projects/northstar-renewal.md",
        "records/synthesis/2026-renewal-plan.md",
    ] {
        assert!(
            records.contains(&conclusion.to_string()),
            "the records-layer walk must surface the conclusion record {conclusion}"
        );
    }
    // There is no longer a `wiki/` directory at all — the walk never yields one.
    assert!(
        !records.iter().any(|p| p.starts_with("wiki/")),
        "no path under a `wiki/` layer may appear; the layer was removed"
    );
}

/// `Store::walk` over `corpus-a` returns the full content set across both
/// layers and **never** a meta file (`DB.md` / `index.md` / `log.md`), a
/// sidecar (`index.jsonl`), or anything under `log/`.
///
/// The count (515 = 6 sources + 509 records) is pinned because it is the single
/// number a broad class of walk bugs would move: a leaked catalog (+N), a
/// dropped shard subtree (−N), or a descended hidden dir (+N). The 509 records
/// include the four `meta-type: conclusion` synthesis records (the former wiki
/// pages, now under `records/`). The explicit exclusion checks name the specific
/// files a correct walk must omit.
#[test]
fn corpus_a_walk_is_content_only_no_meta_no_sidecar_no_log() {
    let store = open_corpus_a();
    let all = store.walk().expect("walk corpus-a");
    let set: BTreeSet<String> = as_sorted_strings(&all).into_iter().collect();

    // Cardinality: every content .md across both layers, nothing else.
    assert_eq!(
        all.len(),
        515,
        "expected 6 sources + 509 records content files"
    );

    // Per-layer split matches the layer walks (so a leak/drop in one layer is
    // localized, not just absorbed into the total).
    assert_eq!(store.walk_layer(Layer::Sources).unwrap().len(), 6);
    assert_eq!(store.walk_layer(Layer::Records).unwrap().len(), 509);

    // Meta files at root and per-folder catalogs must be absent.
    for excluded in [
        "DB.md",
        "index.md",
        "log.md",
        "records/contacts/index.md",
        "records/contacts/index.jsonl",
        "sources/emails/index.md",
        "sources/emails/index.jsonl",
        "records/profiles/index.md",
    ] {
        assert!(
            !set.contains(excluded),
            "Store::walk must not yield the meta/sidecar file {excluded}"
        );
    }

    // A representative content file from each layer (incl. a sharded one and a
    // conclusion record) must be present — proves the walk reached into the
    // shards, not just the roots.
    for included in [
        "sources/emails/2026/05/2026-05-22-elena-renewal.md",
        "records/contacts/sarah-chen.md",
        "records/projects/northstar-renewal.md",
    ] {
        assert!(
            set.contains(included),
            "Store::walk must yield the content file {included}"
        );
    }
}

// ── store: find_links_to (embedded ripgrep, all .md incl. catalogs) ─────────

/// `Store::find_links_to(records/contacts/sarah-chen)` over `corpus-a` returns
/// every `.md` file in the store tree carrying an incoming
/// `[[records/contacts/sarah-chen]]` link — **including** the
/// `records/contacts/index.md` catalog, per the method's contract ("an incoming
/// reference is wherever the literal link text lives").
///
/// The six-file set is enumerated by hand from the corpus:
/// - `records/companies/northstar.md` — body mention,
/// - `records/contacts/index.md` — the type-folder catalog,
/// - `records/meetings/2026/04/…quarterly-review.md` — `attendees:` + body,
/// - `records/meetings/2026/05/…renewal-call.md` — `attendees:` + body,
/// - `records/profiles/sarah-chen.md` — the profile conclusion record,
/// - `records/projects/northstar-renewal.md` — the project conclusion record.
///
/// `find_links_to`'s scan is "every `.md`, skip hidden + `log/`", so it also
/// reaches `corpus-a`'s `EXPECTED/` golden-mirror tree (a fixture-metadata
/// folder that happens to carry a *copy* of the contacts catalog, hence a copy
/// of the link). That is the method behaving correctly — `EXPECTED/` is neither
/// hidden nor `log/` — but it is fixture metadata, not store content, so we make
/// the exact-equality assertion over the semantically-meaningful region: the
/// two content layers. The two separate checks below pin both facts: (1) the
/// content-layer hits are *exactly* the six, and (2) the catalog `index.md` is
/// included while the `index.jsonl` sidecar (not a `.md`) never is.
///
/// A bug that skipped catalogs, missed the `attendees:` block-list links, or
/// failed to recurse the meeting shards would drop entries from this set; one
/// that started matching sidecars or short-form links would add to it.
#[test]
fn corpus_a_find_links_to_sarah_chen_includes_catalog() {
    let store = open_corpus_a();
    let all = store
        .find_links_to(Path::new("records/contacts/sarah-chen"))
        .expect("find_links_to sarah-chen");
    let all_set: BTreeSet<String> = as_sorted_strings(&all).into_iter().collect();

    // (1) Restricted to the store content layers, the hit set is exactly the six
    // known referrers (the body/frontmatter records + the type-folder catalog).
    // Filtering out `EXPECTED/` keeps this assertion about db.md semantics, not
    // about the fixture's golden-mirror layout.
    let in_layers: Vec<String> = all_set
        .iter()
        .filter(|p| p.starts_with("sources/") || p.starts_with("records/"))
        .cloned()
        .collect();
    assert_eq!(
        in_layers,
        owned(&[
            "records/companies/northstar.md",
            "records/contacts/index.md",
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review.md",
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call.md",
            "records/profiles/sarah-chen.md",
            "records/projects/northstar-renewal.md",
        ]),
        "find_links_to scans every .md (catalogs included) for the literal link"
    );

    // (2) The type-folder catalog is included (it carries the literal link), but
    // the `.jsonl` sidecar twin is never returned — the scan is `.md`-only.
    assert!(
        all_set.contains("records/contacts/index.md"),
        "the index.md catalog carries the link and must be returned"
    );
    assert!(
        !all_set.iter().any(|p| p.ends_with(".jsonl")),
        "find_links_to is a .md scan; an index.jsonl is never a hit"
    );
}

/// `find_links_to` matches the **literal link text**, independent of whether the
/// target file exists. In `corpus-b` the only reference to
/// `records/contacts/ghost` is the deliberately-broken body link inside
/// `records/misc/broken-link.md`, and `records/contacts/ghost.md` does not
/// exist anywhere in the store.
///
/// This pins the separation between *link presence* (find_links_to's job) and
/// *link resolution* (validate's `WIKI_LINK_BROKEN`): the dangling target still
/// has exactly one incoming reference.
#[test]
fn corpus_b_find_links_to_dangling_target_finds_the_one_referrer() {
    let store = open_corpus_b();
    // Guard the fixture's premise: the target file really is absent.
    assert!(
        !store.root.join("records/contacts/ghost.md").exists(),
        "fixture invariant: records/contacts/ghost.md must not exist"
    );
    let got = as_sorted_strings(
        &store
            .find_links_to(Path::new("records/contacts/ghost"))
            .expect("find_links_to ghost"),
    );
    assert_eq!(
        got,
        owned(&["records/misc/broken-link.md"]),
        "a broken link's target still has exactly its one referrer"
    );
}

// ── graph: backlinks / forwardlinks on the real corpus ──────────────────────

/// `graph::backlinks(records/contacts/sarah-chen)` over `corpus-a` returns the
/// five **content** files that link to it, in canonical bare form (no `.md`),
/// with the `records/contacts/index.md` catalog **excluded** (backlinks reads
/// its candidate set from the type-folder `index.jsonl` sidecars, which list
/// content files only — never the catalogs).
///
/// This is the same incoming-edge set as `find_links_to` *minus* the catalog,
/// and re-asserting it here pins the two-policy distinction (graph = content
/// edges; store.find_links_to = every literal reference). It also confirms each
/// edge is reconfirmed by re-parsing the candidate: the frontmatter
/// `company: [[…]]` edge from `northstar.md` and the `attendees:` block-list
/// edges from both meetings are caught, not just body links.
#[test]
fn corpus_a_backlinks_sarah_chen_is_content_only_bare_form() {
    let store = open_corpus_a();
    let got = as_sorted_strings(
        &backlinks(&store, Path::new("records/contacts/sarah-chen")).expect("backlinks sarah-chen"),
    );
    assert_eq!(
        got,
        owned(&[
            "records/companies/northstar",
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review",
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call",
            "records/profiles/sarah-chen",
            "records/projects/northstar-renewal",
        ]),
        "backlinks: content files only (no index.md catalog), bare no-.md paths"
    );
}

/// `graph::forwardlinks(records/contacts/sarah-chen.md)` over `corpus-a` returns
/// the three distinct outgoing targets the file links to — across both
/// frontmatter (`company: [[records/companies/northstar]]`) and body
/// (`[[records/projects/northstar-renewal]]`, the renewal-call meeting). The
/// duplicate `northstar` reference (once in frontmatter, once in the body) is
/// deduped to a single edge.
///
/// A forwardlinks that read only the body (missing the frontmatter `company`
/// link) would drop `records/companies/northstar`; one that didn't dedup would
/// list it twice.
#[test]
fn corpus_a_forwardlinks_sarah_chen_spans_frontmatter_and_body_deduped() {
    let store = open_corpus_a();
    let got = as_sorted_strings(
        &forwardlinks(&store, Path::new("records/contacts/sarah-chen.md"))
            .expect("forwardlinks sarah-chen"),
    );
    assert_eq!(
        got,
        owned(&[
            "records/companies/northstar",
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call",
            "records/projects/northstar-renewal",
        ]),
        "forwardlinks must include the frontmatter `company` link, deduped"
    );
}

/// `forwardlinks` accepts the bare (no-`.md`) seed spelling and resolves it to
/// the on-disk file, returning the same edge set as the `.md` spelling. A
/// just-ingested contact (`marcus-okafor`) links only to its company, twice in
/// the file (frontmatter + body) — proving both the bare-seed resolution and
/// the dedup on a minimal node.
#[test]
fn corpus_a_forwardlinks_accepts_bare_seed_and_dedups_single_edge() {
    let store = open_corpus_a();
    let bare = forwardlinks(&store, Path::new("records/contacts/marcus-okafor"))
        .expect("forwardlinks bare seed");
    let dotted = forwardlinks(&store, Path::new("records/contacts/marcus-okafor.md"))
        .expect("forwardlinks dotted seed");
    assert_eq!(
        as_sorted_strings(&bare),
        owned(&["records/companies/northstar"]),
        "marcus-okafor links only to its company (deduped from two mentions)"
    );
    assert_eq!(
        as_sorted_strings(&bare),
        as_sorted_strings(&dotted),
        "bare and .md seed spellings resolve to the same file and edge set"
    );
}

/// `forwardlinks` must extract a wiki-link that lives **only** in frontmatter.
/// The `internal-renewal-sync` meeting's single attendee
/// (`[[records/contacts/david-kim]]` in the `attendees:` block-list) is its only
/// wiki-link anywhere — the body carries none. So the file's outgoing edge set
/// is exactly `[records/contacts/david-kim]`.
///
/// This is the test that bites the frontmatter-skip bug class: a `forwardlinks`
/// that scanned only the body (a plausible "parse the markdown body" regression)
/// would return an EMPTY set here, since this file has zero body links. The
/// sarah-chen / marcus-okafor forwardlinks above duplicate their key edges in the
/// body, so only this single-attendee meeting isolates the frontmatter path.
#[test]
fn corpus_a_forwardlinks_extracts_a_frontmatter_only_edge() {
    let store = open_corpus_a();
    let got = forwardlinks(
        &store,
        Path::new("records/meetings/2026/05/2026-05-12-internal-renewal-sync.md"),
    )
    .expect("forwardlinks internal-renewal-sync");
    assert_eq!(
        as_sorted_strings(&got),
        owned(&["records/contacts/david-kim"]),
        "the only edge (a frontmatter `attendees:` link) must be extracted"
    );
}

/// David Kim's incoming edges over `corpus-a`, asserted on **both** of the two
/// distinct `backlinks_filtered` paths — because they are different engines and
/// only one of them parses, so they need separate goldens to be genuinely
/// guarded (see `graph::backlinks_filtered` doc, "Scale (the loop contract)").
///
/// The four content files that link to David Kim are enumerated by hand from the
/// corpus, with how each edge is written called out — this is the load-bearing
/// detail, since two edges live **only** in `attendees:` frontmatter:
/// - `records/companies/acme.md` — body mention (a `company`),
/// - `records/meetings/2026/04/…quarterly-review.md` — `attendees:` only,
/// - `records/meetings/2026/05/…internal-renewal-sync.md` — `attendees:` only,
/// - `records/meetings/2026/05/…renewal-call.md` — `attendees:` + body.
///
/// **Assertion 1 — unscoped path (embedded-ripgrep scan, no parse).** Plain
/// `backlinks` routes through `backlinks_filtered(.., &[], None)`, whose unscoped
/// branch is `Store::find_links_to`: a raw-bytes ripgrep pass that matches the
/// literal `[[…david-kim]]` text wherever it sits — frontmatter or body — and
/// never parses a file. So all four referrers come back here regardless of how
/// the edge is written. This guards the **scan** engine: skipping `index.md`
/// catalogs (none link Kim, so no change), failing to recurse the meeting
/// `<YYYY>/<MM>` shards (would drop the two date-sharded meetings), or matching
/// short-form / sidecar twins would move the set. It does **not**, however,
/// exercise the confirm-by-parse path — a frontmatter-blind parser leaves this
/// assertion green (the link bytes are still on disk for ripgrep to find). That
/// regression is caught by assertion 2 and by
/// `corpus_a_forwardlinks_extracts_a_frontmatter_only_edge`.
///
/// **Assertion 2 — scoped path (`--type meeting`, confirm-by-parse).** Adding a
/// `--type` (here `["meeting"]`) flips `backlinks_filtered` to its scoped branch:
/// the candidate set is read from the `meeting` type-folder sidecar and each
/// candidate is then **confirmed by re-parsing it via `forwardlinks`** (body +
/// every frontmatter field), not by trusting the sidecar's `links:` projection
/// (which omits `attendees:`). Scoped to `meeting`, the expected set is the three
/// meetings — and because two of them reference Kim *only* through `attendees:`
/// frontmatter, a `forwardlinks` that read only the body, or a confirm-read that
/// trusted the frontmatter `links:` projection, would drop those two and shrink
/// this set. The synthetic `graph.rs` `backlinks_filtered_*` unit tests cover the
/// scoped branch but write their candidate edges in the *body*; this is the
/// assertion that pins the scoped confirm-read against **frontmatter-only**
/// `attendees:` edges on real corpus data.
#[test]
fn corpus_a_backlinks_david_kim_includes_frontmatter_only_edges() {
    let store = open_corpus_a();

    // Assertion 1 — unscoped: the raw ripgrep scan finds the literal link text
    // wherever it lives, so all four referrers (incl. the two frontmatter-only
    // meetings) come back. This pins the scan engine; it does NOT prove the
    // parse path (the bytes are on disk regardless of parser correctness).
    let unscoped = as_sorted_strings(
        &backlinks(&store, Path::new("records/contacts/david-kim")).expect("backlinks david-kim"),
    );
    assert_eq!(
        unscoped,
        owned(&[
            "records/companies/acme",
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review",
            "records/meetings/2026/05/2026-05-12-internal-renewal-sync",
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call",
        ]),
        "unscoped backlinks (ripgrep scan) must find every literal `[[…david-kim]]`, \
         frontmatter or body"
    );

    // Assertion 2 — scoped `--type meeting`: this branch confirms each candidate
    // by re-parsing it through forwardlinks (body + ALL frontmatter). Two of the
    // three meetings reference Kim ONLY via `attendees:` frontmatter, so a
    // frontmatter-blind confirm-read (body-only parse, or trusting the sidecar
    // `links:` projection that omits `attendees:`) would drop them and shrink
    // this set. The `acme` company is excluded by the `--type meeting` filter.
    let scoped = as_sorted_strings(
        &backlinks_filtered(
            &store,
            Path::new("records/contacts/david-kim"),
            &["meeting".to_string()],
            None,
        )
        .expect("scoped backlinks david-kim --type meeting"),
    );
    assert_eq!(
        scoped,
        owned(&[
            "records/meetings/2026/04/2026-04-15-northstar-quarterly-review",
            "records/meetings/2026/05/2026-05-12-internal-renewal-sync",
            "records/meetings/2026/05/2026-05-22-northstar-renewal-call",
        ]),
        "scoped backlinks confirms candidates by parse; the two attendees-only \
         meetings must survive a frontmatter-aware confirm-read"
    );
}

/// The two directions agree on one edge set: for the central renewal-call
/// meeting, every file `backlinks` reports as linking *to* it must, when read,
/// actually link to it (`forwardlinks` of that file contains the meeting).
///
/// This is the load-bearing graph invariant ("an incoming edge to X is exactly:
/// some file whose forwardlinks contains X"), asserted against real corpus data
/// rather than a synthetic pair. A backlinks implementation that trusted the
/// sidecar's frontmatter `links` projection instead of re-parsing would admit a
/// candidate whose actual link lives only in a typed field or body, breaking
/// this round-trip.
#[test]
fn corpus_a_backlinks_and_forwardlinks_agree_on_one_edge_set() {
    let store = open_corpus_a();
    let meeting = "records/meetings/2026/05/2026-05-22-northstar-renewal-call";
    let incoming = backlinks(&store, Path::new(meeting)).expect("backlinks meeting");
    assert!(
        !incoming.is_empty(),
        "the renewal-call meeting is referenced by several records — \
         a non-empty backlink set is part of the fixture's intent"
    );
    for linker in &incoming {
        let out = forwardlinks(&store, linker).expect("forwardlinks of a backlinker");
        let out_set: BTreeSet<String> = as_sorted_strings(&out).into_iter().collect();
        assert!(
            out_set.contains(meeting),
            "{} is reported as a backlink of {meeting} but its forwardlinks \
             do not contain it — the two directions disagree",
            linker.display()
        );
    }
}

// ── parser: DB.md ## Schemas / ## Policies parse on the real corpora ────────

/// Parsing `corpus-a`'s `DB.md` yields the schemas, policies, and agent
/// instructions the file declares, with every `### <type>` field's modifiers
/// decoded per the SPEC (`required`, shape, `link to <prefix>/` with the
/// trailing slash stripped, `enum:`, `default`).
///
/// Each assertion is hand-derived from `corpus-a-canonical/DB.md` + SPEC §
/// "## Schemas". The checks are chosen to fail under a modifier-parsing bug:
/// dropping `link to` would null `company.link_prefix`; mis-splitting `enum:`
/// (which carries its own commas) would truncate `company.relationship`;
/// ignoring `default` would null `expense.currency`.
#[test]
fn corpus_a_db_md_schemas_and_policies_parse_per_spec() {
    let store = open_corpus_a();
    let cfg = &store.config;

    // Re-parse the raw file directly too, so the test pins `parse_db_md` itself
    // (not just whatever `Store::open` cached) and would catch `open` silently
    // substituting a default config.
    let raw = std::fs::read_to_string(store.root.join("DB.md")).expect("read corpus-a DB.md");
    let parsed = parse_db_md(&raw, &store.root.join("DB.md")).expect("parse corpus-a DB.md");
    assert_eq!(
        &parsed, cfg,
        "Store::open must surface exactly what parse_db_md produces"
    );

    // ## Agent instructions — present and carrying the file's prose.
    let instructions = cfg
        .agent_instructions
        .as_deref()
        .expect("corpus-a DB.md declares ## Agent instructions");
    assert!(
        instructions.contains("British English"),
        "agent instructions prose must be captured verbatim, got: {instructions:?}"
    );

    // ## Policies → ### Frozen pages / ### Ignored types.
    assert_eq!(
        cfg.frozen_pages,
        vec![PathBuf::from("records/synthesis/2026-renewal-plan.md")],
        "the one frozen page must parse from its bullet"
    );
    assert_eq!(
        cfg.ignored_types,
        vec!["test".to_string()],
        "the one ignored type must parse from its bullet"
    );

    // ## Schemas — the five declared types, by H3 heading.
    let schema_types: BTreeSet<&str> = cfg.schemas.keys().map(|s| s.as_str()).collect();
    assert_eq!(
        schema_types,
        BTreeSet::from(["contact", "company", "expense", "meeting", "invoice"]),
        "every ### <type> sub-section must become a schema"
    );

    // contact: required + shape + link-to-prefix (trailing slash stripped).
    let contact = &cfg.schemas["contact"];
    let name = contact
        .fields
        .iter()
        .find(|f| f.name == "name")
        .expect("contact.name");
    assert!(name.required && name.shape == Some(Shape::String));

    let email = contact
        .fields
        .iter()
        .find(|f| f.name == "email")
        .expect("contact.email");
    assert!(email.required && email.shape == Some(Shape::Email));

    let company = contact
        .fields
        .iter()
        .find(|f| f.name == "company")
        .expect("contact.company");
    assert!(company.required, "contact.company is required");
    assert_eq!(
        company.link_prefix.as_deref(),
        Some(Path::new("records/companies")),
        "`link to records/companies/` must parse with the trailing slash dropped"
    );
    assert!(
        company.shape.is_none(),
        "a link field carries no scalar shape"
    );

    let role = contact
        .fields
        .iter()
        .find(|f| f.name == "role")
        .expect("contact.role");
    assert!(
        !role.required && role.shape == Some(Shape::String),
        "role is an optional string"
    );

    // company.relationship: enum with four options (enum carries its own commas
    // and must swallow the whole tail of the line).
    let company_schema = &cfg.schemas["company"];
    let relationship = company_schema
        .fields
        .iter()
        .find(|f| f.name == "relationship")
        .expect("company.relationship");
    assert_eq!(
        relationship.enum_values.as_deref(),
        Some(
            &[
                "customer".to_string(),
                "vendor".to_string(),
                "partner".to_string(),
                "prospect".to_string(),
            ][..]
        ),
        "enum: must parse the full comma-separated option list"
    );
    assert!(
        !relationship.required && relationship.shape.is_none(),
        "an enum-only field is optional and has no scalar shape"
    );

    // expense.currency: a `default <value>` field.
    let expense = &cfg.schemas["expense"];
    let currency = expense
        .fields
        .iter()
        .find(|f| f.name == "currency")
        .expect("expense.currency");
    assert_eq!(
        currency.default,
        Some(serde_norway::Value::String("USD".to_string())),
        "`default USD` must parse into the field's default value"
    );

    // invoice.amount carries the currency shape.
    let invoice = &cfg.schemas["invoice"];
    let amount = invoice
        .fields
        .iter()
        .find(|f| f.name == "amount")
        .expect("invoice.amount");
    assert!(
        amount.required && amount.shape == Some(Shape::Currency),
        "invoice.amount is a required currency"
    );
}

/// `corpus-a` and `corpus-b` declare the `meeting` schema differently, and the
/// parser must reflect the difference: in `corpus-b`, `meeting.attendees` is
/// `link to records/contacts/`, whereas in `corpus-a` it is a bare `required`
/// field with no link prefix.
///
/// This is the discriminator test — a parser that ignored `link to`, or that
/// (e.g.) read the wrong store's `DB.md`, would make the two stores' `meeting`
/// schemas indistinguishable, which this asserts is wrong.
#[test]
fn corpus_a_and_b_meeting_schemas_differ_on_attendees_link() {
    let a = open_corpus_a();
    let b = open_corpus_b();

    let a_attendees = a.config.schemas["meeting"]
        .fields
        .iter()
        .find(|f| f.name == "attendees")
        .expect("corpus-a meeting.attendees");
    assert!(
        a_attendees.required && a_attendees.link_prefix.is_none(),
        "corpus-a meeting.attendees is a bare required field (no link prefix)"
    );

    let b_attendees = b.config.schemas["meeting"]
        .fields
        .iter()
        .find(|f| f.name == "attendees")
        .expect("corpus-b meeting.attendees");
    assert!(
        b_attendees.required,
        "corpus-b meeting.attendees is required"
    );
    assert_eq!(
        b_attendees.link_prefix.as_deref(),
        Some(Path::new("records/contacts")),
        "corpus-b declares `attendees (required, link to records/contacts/)`"
    );

    assert_ne!(
        a_attendees.link_prefix, b_attendees.link_prefix,
        "the two corpora's meeting.attendees must parse differently"
    );
}

/// `corpus-b`'s `bad-db-md/` sub-store has a `DB.md` whose body carries an
/// unrecognized `## Glossary` H2. `parse_db_md` must **ignore** the unknown
/// section (so it never corrupts config) while still capturing the recognized
/// `## Agent instructions` — the parser is lenient, and flagging the unknown
/// section is `validate`'s job (`DB_MD_UNKNOWN_SECTION`), not the parser's.
///
/// Asserting against the real `bad-db-md/DB.md` file binds the
/// "unknown-section-is-not-a-parse-error" contract to the committed fixture.
#[test]
fn corpus_b_bad_db_md_parses_known_sections_and_ignores_unknown() {
    let bad = corpora_dir().join("corpus-b-edges").join("bad-db-md");
    let db_md = bad.join("DB.md");
    let raw = std::fs::read_to_string(&db_md).expect("read bad-db-md DB.md");
    let cfg = parse_db_md(&raw, &db_md).expect("parse_db_md is lenient on unknown sections");

    assert!(
        cfg.agent_instructions
            .as_deref()
            .map(|s| s.contains("Recognized section"))
            .unwrap_or(false),
        "the recognized ## Agent instructions must still be captured"
    );
    // The unknown ## Glossary section contributes nothing to the parsed config.
    assert!(
        cfg.schemas.is_empty() && cfg.frozen_pages.is_empty() && cfg.ignored_types.is_empty(),
        "an unknown H2 must not leak into schemas/policies"
    );
}
