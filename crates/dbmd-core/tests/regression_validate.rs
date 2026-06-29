//! Regression tests for confirmed launch-blocking `validate` bugs.
//!
//! Each test reconstructs the exact trigger from the adversarial review and
//! asserts the *corrected* behaviour through the crate's public surface
//! (`Store` + `validate::validate_all` / `validate::validate_working_set`),
//! never a private internal. Every test is written so it FAILS against the
//! pre-fix code, encoding the bug so it can never silently return.
//!
//! Findings covered:
//! - **#2** `INDEX_SUMMARY_MISMATCH` false-positives on any `summary` containing
//!   `" · "` (middle dot) — `validate --all` exits non-zero on a clean store.
//! - **#8 / #15** Working-set `validate` reads only the active `log.md`, so a
//!   changed-but-unvalidated file stranded in a `log/<YYYY-MM>.md` archive after
//!   a month rollover is silently skipped (and the `validate` cutoff resets).

use std::path::Path;

use dbmd_core::index::Index;
use dbmd_core::store::Store;
use dbmd_core::validate::{codes, validate_all, validate_working_set, Issue};

// ── fixture helpers (the inline `mod tests` `Fixture` is private to the crate,
//    so an integration test rebuilds the minimal surface it needs over a real
//    tempdir, opening the store through the public `Store::open_strict`) ───────

/// Write a store-relative file, creating parent dirs.
fn write(root: &Path, rel: &str, contents: &str) {
    let abs = root.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, contents).unwrap();
}

/// A fresh tempdir store with a valid `DB.md` identity file and the two layer
/// dirs — the same "truly clean" baseline the in-crate `Fixture::new` builds.
fn fresh_store(dir: &Path) {
    std::fs::write(
        dir.join("DB.md"),
        "---\ntype: db-md\nscope: company\nowner: Test\n---\n",
    )
    .unwrap();
    for layer in ["sources", "records"] {
        std::fs::create_dir_all(dir.join(layer)).unwrap();
    }
}

/// Open the tempdir as a db.md store via the public, config-parsing path.
fn open(dir: &Path) -> Store {
    Store::open_strict(dir).expect("tempdir has a valid DB.md")
}

/// A minimal valid `contact` body with the given one-line summary.
fn contact(summary: &str) -> String {
    format!(
        "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: \"{summary}\"\nname: A\n---\n\n# A\n"
    )
}

fn has(issues: &[Issue], code: &str) -> bool {
    issues.iter().any(|i| i.code == code)
}

// ── Finding #2: middle-dot summary must not trip INDEX_SUMMARY_MISMATCH ───────

/// A clean, perfectly-synced store whose `summary` contains the typographic
/// middle-dot separator `" · "` must NOT report `INDEX_SUMMARY_MISMATCH`.
///
/// The index renderer emits the tag suffix as `  ·  #tag` (double-spaced, and
/// only when tags exist); a `·` inside the summary text itself is part of the
/// summary, not a tag separator. Pre-fix, `extract_index_entry_summary` split on
/// single-spaced `" · "`, truncating the index entry text `Acme · Q2 renewal` to
/// `Acme` and mismatching the file's real summary — a spurious `Severity::Error`
/// (CLI exit 6) on a healthy store that `index rebuild` cannot clear. The store
/// is built with the canonical `Index::rebuild_all`, so the on-disk index
/// exactly quotes the file's summary; the only way to fail is the parser bug.
#[test]
fn regression_index_summary_with_middle_dot_does_not_false_positive() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    fresh_store(root);

    // No tags → the rendered index entry is `- [[...]] — Acme · Q2 renewal`
    // with the middle dot belonging to the summary, not a tag suffix.
    write(
        root,
        "records/companies/acme.md",
        &contact("Acme · Q2 renewal"),
    );

    let store = open(root);
    // Build the canonical index.md + index.jsonl the same way `dbmd index
    // rebuild` does — so the on-disk entry verbatim quotes the file's summary.
    Index::rebuild_all(&store).unwrap();

    let issues = validate_all(&store).unwrap();
    assert!(
        !has(&issues, codes::INDEX_SUMMARY_MISMATCH),
        "a middle-dot summary on a freshly-rebuilt store must not desync: {issues:#?}"
    );
    // The whole point is the sweep stays green — nothing should be an error.
    assert!(
        !issues.iter().any(Issue::is_error),
        "clean store with a middle-dot summary should have no errors: {issues:#?}"
    );
}

/// Sanity guard for the other side of finding #2's fix: a genuine trailing tag
/// suffix (`  ·  #tag`, the renderer's real double-spaced form) is still
/// stripped before the compare, so a correctly-tagged entry stays clean. This
/// locks the fix to "match the renderer's tag block" rather than "never strip".
#[test]
fn regression_index_summary_strips_real_double_spaced_tag_suffix() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    fresh_store(root);

    // A contact carrying tags → `Index::rebuild_all` emits `… — s  ·  #vip`.
    write(
        root,
        "records/contacts/a.md",
        "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: \"clean summary\"\nname: A\ntags:\n  - vip\n---\n\n# A\n",
    );

    let store = open(root);
    Index::rebuild_all(&store).unwrap();

    let issues = validate_all(&store).unwrap();
    assert!(
        !has(&issues, codes::INDEX_SUMMARY_MISMATCH),
        "the renderer's `  ·  #tag` suffix must be stripped before compare: {issues:#?}"
    );
}

// ── Findings #8 / #15: working-set validate must cross log archives ───────────

/// After a month rollover, a file changed-but-never-validated in a prior month
/// is rotated into `log/<YYYY-MM>.md`; the working-set `validate` must still
/// validate it.
///
/// Trigger: the active `log.md` holds only a current-month (June) mutation, and
/// the May archive holds the prior-month `update` to `sarah-chen.md`, which
/// carries a real defect (a short-form wiki-link). Pre-fix, `changed_objects_since`
/// read only the active `log.md`, so the archived May change never entered the
/// working set and its `WIKI_LINK_SHORT_FORM` defect went unreported — a silent
/// vacuous pass. Because a June mutation exists, the empty-set content-sweep
/// fallback does not fire, so the only way to surface the defect is to read the
/// archive.
#[test]
fn regression_working_set_validates_archived_changed_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    fresh_store(root);

    // The archived-month file carries a short-form (defective) wiki-link.
    write(
        root,
        "records/contacts/sarah-chen.md",
        "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-31T14:00:00-07:00\nsummary: \"changed in May, never validated\"\nname: Sarah\n---\n\nSee [[tom]].\n",
    );
    // A clean current-month file (its create is the only entry left in active log).
    write(root, "records/contacts/tom.md", &contact("created in June"));

    // Active log.md: only the June create survives rotation.
    write(
        root,
        "log.md",
        "---\ntype: log\n---\n\n## [2026-06-01 08:00] create | records/contacts/tom\n",
    );
    // May archive: the validate AND the unvalidated May-31 update were rotated here.
    write(
        root,
        "log/2026-05.md",
        "## [2026-05-30 09:00] validate\nPASS\n\n## [2026-05-31 14:00] update | records/contacts/sarah-chen\nedited\n",
    );

    let store = open(root);
    let issues = validate_working_set(&store, None).unwrap();
    assert!(
        issues.iter().any(|i| i.code == codes::WIKI_LINK_SHORT_FORM
            && i.file == Path::new("records/contacts/sarah-chen.md")),
        "the archived May change must be validated, surfacing its short-form link: {issues:#?}"
    );
}

/// The working-set cutoff (`last_validate_at`) must read the `validate` entry
/// from a `log/<YYYY-MM>.md` archive, not silently reset to `None`.
///
/// Trigger: the last `validate` entry and an earlier (pre-validate) `update`
/// both rotated into the May archive; a post-validate change sits in the active
/// June log. Pre-fix, `last_validate_at` read only the active log → returned
/// `None` → the cutoff vanished and a `changed_objects_since(None)` that also
/// ignored archives mis-anchored the window. With the fix, the cutoff is the
/// archived May-30 validate, so the pre-validate `before.md` change is correctly
/// EXCLUDED while the post-validate `after.md` change is INCLUDED.
#[test]
fn regression_working_set_cutoff_reads_archived_validate_entry() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    fresh_store(root);

    // `before` changed before the (archived) validate; it must be excluded.
    // It carries a defect (short-form link) that must NOT be reported.
    write(
        root,
        "records/contacts/before.md",
        "---\ntype: contact\ncreated: 2026-05-20T10:00:00-07:00\nupdated: 2026-05-20T10:00:00-07:00\nsummary: \"changed before validate\"\nname: B\n---\n\nSee [[ghost]].\n",
    );
    // `after` changed after the validate; its defect must be reported.
    write(
        root,
        "records/contacts/after.md",
        "---\ntype: contact\ncreated: 2026-06-02T10:00:00-07:00\nupdated: 2026-06-02T10:00:00-07:00\nsummary: \"changed after validate\"\nname: A\n---\n\nSee [[phantom]].\n",
    );

    // Active log: only the post-validate June change survives.
    write(
        root,
        "log.md",
        "---\ntype: log\n---\n\n## [2026-06-02 10:00] update | records/contacts/after\n",
    );
    // May archive: the pre-validate update AND the validate entry (the cutoff).
    write(
        root,
        "log/2026-05.md",
        "## [2026-05-20 10:00] update | records/contacts/before\nx\n\n## [2026-05-30 09:00] validate\nPASS\n",
    );

    let store = open(root);
    let issues = validate_working_set(&store, None).unwrap();
    assert!(
        issues
            .iter()
            .any(|i| i.file == Path::new("records/contacts/after.md")),
        "post-validate change must be in the working set: {issues:#?}"
    );
    assert!(
        !issues
            .iter()
            .any(|i| i.file == Path::new("records/contacts/before.md")),
        "pre-validate change (before the archived cutoff) must be excluded: {issues:#?}"
    );
}

// ── Adversarial review (second pass) ─────────────────────────────────────────

/// INDEX_SUMMARY_MISMATCH must not false-positive on a valid one-line summary
/// that carries INTERNAL whitespace (a double space, a tab). The index renderer
/// collapses whitespace runs when it writes the `index.md` browse line; pre-fix
/// the validator compared that collapsed text against the RAW file summary and
/// flagged a mismatch — permanently, since `index rebuild` regenerates the same
/// collapsed line (the store wedges at exit 6). The fix normalizes BOTH sides
/// with the renderer's `collapse_whitespace` before comparing.
#[test]
fn regression_index_summary_internal_whitespace_does_not_false_positive() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    fresh_store(root);

    // Internal DOUBLE SPACE — a legal one-line summary (only a newline is
    // forbidden by SUMMARY_MULTILINE).
    write(
        root,
        "records/companies/acme.md",
        &contact("Partner;  our operating co"),
    );
    // Internal TAB — same class.
    write(root, "records/companies/beta.md", &contact("Ops\tlead co"));

    let store = open(root);
    Index::rebuild_all(&store).unwrap();

    let issues = validate_all(&store).unwrap();
    assert!(
        !has(&issues, codes::INDEX_SUMMARY_MISMATCH),
        "internal-whitespace summaries on a freshly-rebuilt store must not desync: {issues:#?}"
    );
    assert!(
        !issues.iter().any(Issue::is_error),
        "a clean store with internal-whitespace summaries must have no errors: {issues:#?}"
    );
}

/// `## Folders` is a real, shipped DB.md section: `parse_db_md` reads it into
/// `Config.folders` and the index renders folder display names + descriptions
/// from it. It must NOT be flagged `DB_MD_UNKNOWN_SECTION` (whose remedy is to
/// delete the heading — which would destroy curator-authored rollup names). A
/// genuinely unknown section must still warn.
#[test]
fn regression_db_md_folders_section_is_recognized() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("DB.md"),
        "---\ntype: db-md\nscope: company\nowner: Test\n---\n\n# Store\n\n## Folders\n\n- records/contacts|Contacts — people we have met\n",
    )
    .unwrap();
    for layer in ["sources", "records"] {
        std::fs::create_dir_all(root.join(layer)).unwrap();
    }
    write(root, "records/contacts/a.md", &contact("a contact"));

    let store = open(root);
    Index::rebuild_all(&store).unwrap();
    let issues = validate_all(&store).unwrap();
    assert!(
        !has(&issues, codes::DB_MD_UNKNOWN_SECTION),
        "`## Folders` is a recognized DB.md section and must not be flagged: {issues:#?}"
    );

    // Control: a genuinely unknown section still warns (the fix added exactly
    // `folders`, it did not disable the check).
    std::fs::write(
        root.join("DB.md"),
        "---\ntype: db-md\nscope: company\nowner: Test\n---\n\n# Store\n\n## Bogus\n\n- nope\n",
    )
    .unwrap();
    let store2 = open(root);
    let issues2 = validate_all(&store2).unwrap();
    assert!(
        has(&issues2, codes::DB_MD_UNKNOWN_SECTION),
        "an unrecognized DB.md section must still warn: {issues2:#?}"
    );
}

/// Default `dbmd validate` (working set) must never read or report on a file
/// OUTSIDE the store via a `..` object in a `log.md` header. Pre-fix
/// `changed_objects_since` inserted the object verbatim, so a
/// `records/../../leaky` header made `validate_working_set` read + frontmatter-
/// report a file two dirs above the store root (a containment escape + an
/// existence oracle + frontmatter disclosure). The fix routes the object through
/// `safe_md_target_rel`, dropping any `..`/absolute/prefix path from the changed
/// set.
#[test]
fn regression_validate_working_set_does_not_escape_store_via_log_object() {
    // Store nested below a host root; the secret sits OUTSIDE the store root at
    // exactly the location `records/../../leaky.md` resolves to from the root.
    // root = host/mid/store; root.join("records/../../leaky.md") walks
    // records → .. → store → .. → mid → leaky.md, i.e. host/mid/leaky.md
    // (= root.parent()), which is outside the store root.
    let host = tempfile::TempDir::new().unwrap();
    let root = host.path().join("mid").join("store");
    std::fs::create_dir_all(&root).unwrap();
    fresh_store(&root);
    std::fs::write(
        root.parent().unwrap().join("leaky.md"),
        "---\ntype: contact\ncreated: TOP-SECRET\nsummary: secret\nname: X\n---\n\n# x\n",
    )
    .unwrap();

    // A real in-store change keeps the working set non-empty (so the empty-set
    // vacuous-fallback sweep does not mask the result), beside the escaping one.
    write(&root, "records/contacts/real.md", &contact("real one"));
    write(
        &root,
        "log.md",
        "---\ntype: log\n---\n\n## [2026-06-01 08:00] create | records/contacts/real\n## [2026-06-01 08:01] update | records/../../leaky\n",
    );

    let store = open(&root);
    let issues = validate_working_set(&store, None).unwrap();
    assert!(
        !issues
            .iter()
            .any(|i| i.file.to_string_lossy().contains("leaky")),
        "validate must not read/report a file outside the store via a `..` log object: {issues:#?}"
    );
}

/// `validate --all` must follow symlinks like the loop default (`md_walker`
/// `follow_links(true)`). A content file symlinked into a type-folder is checked
/// by `dbmd validate` (the loop default), but pre-fix `walk_content_files` used a
/// no-follow `WalkDir`, so `--all` silently SKIPPED it — the authoritative
/// superset reporting FEWER issues than the loop scope on the same store.
#[cfg(unix)]
#[test]
fn regression_validate_all_follows_symlinked_content_file() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    fresh_store(root);

    // A real content file OUTSIDE the layers, carrying a broken full-path
    // wiki-link, symlinked INTO records/profiles/.
    let real = root.join("external/bio.md");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(
        &real,
        "---\ntype: profile\nmeta-type: conclusion\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: \"bio\"\n---\n\nSee [[records/contacts/does-not-exist]].\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("records/profiles")).unwrap();
    symlink(&real, root.join("records/profiles/bio.md")).unwrap();

    let store = open(root);
    let ws = validate_working_set(&store, None).unwrap();
    let all = validate_all(&store).unwrap();
    assert!(
        ws.iter().any(|i| i.code == codes::WIKI_LINK_BROKEN),
        "the loop default must flag the symlinked-in file's broken link: {ws:#?}"
    );
    assert!(
        all.iter().any(|i| i.code == codes::WIKI_LINK_BROKEN),
        "`validate --all` must also follow the symlink and flag it (superset contract): {all:#?}"
    );
}

/// A stale `index.md` entry must be reported with the SAME code in both scopes.
/// Pre-fix the working-set path body-link-checked `index.md` (pulled in as an
/// incoming linker) and reported a dangling entry as `WIKI_LINK_BROKEN` with the
/// remedy "create the target" — the OPPOSITE of `--all`'s `INDEX_STALE_ENTRY`
/// ("run `dbmd index rebuild`"). The fix excludes the derived catalog from
/// working-set body-link checks, deferring index integrity to `check_indexes`.
#[test]
fn regression_working_set_stale_index_entry_is_not_wiki_link_broken() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    fresh_store(root);

    write(root, "records/contacts/a.md", &contact("a"));
    write(root, "records/contacts/c.md", &contact("c"));
    let store = open(root);
    Index::rebuild_all(&store).unwrap();

    // Delete c.md out of band (index.md keeps its stale `[[…/c]]` entry) and log
    // the delete so c.md — and its index.md linker — enter the working set.
    std::fs::remove_file(root.join("records/contacts/c.md")).unwrap();
    write(
        root,
        "log.md",
        "---\ntype: log\n---\n\n## [2026-06-01 08:00] delete | records/contacts/c\n",
    );

    let store = open(root);
    let ws = validate_working_set(&store, None).unwrap();
    assert!(
        !ws.iter().any(|i| i.code == codes::WIKI_LINK_BROKEN
            && i.file.file_name().and_then(|n| n.to_str()) == Some("index.md")),
        "a stale index.md entry must NOT be WIKI_LINK_BROKEN in the working set: {ws:#?}"
    );
    let all = validate_all(&store).unwrap();
    assert!(
        has(&all, codes::INDEX_STALE_ENTRY),
        "`validate --all` must report the stale index entry as INDEX_STALE_ENTRY: {all:#?}"
    );
}

// ── Loose-only layer: `index rebuild` output must validate clean ──────────────
//
// Findings #0/#1/#3/#4/#8/#12/#14 (one root cause, hit by seven finders): a layer
// holding ONLY loose content (files directly at the layer root, no type-folder)
// is catalogued by the WRITER in the layer's own `index.jsonl` and deliberately
// gets NO root or layer `index.md` (those roll up type-folders only). But
// `validate --all` demanded both `index.md` artifacts whenever any content file
// existed, so a freshly `index rebuild`-ed loose-only store FAILED its own
// validator with two unfixable `INDEX_MISSING` errors (the suggested
// `dbmd index rebuild` never converges) — a write-through/rebuild parity break.

/// A store whose only content is a single loose record must, after the canonical
/// `Index::rebuild_all`, validate clean: no `INDEX_MISSING` for the root or layer
/// `index.md` the rebuild intentionally does not create.
#[test]
fn loose_only_layer_validates_clean_after_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fresh_store(root);
    // One loose record directly under the layer root — no type-folder.
    write(
        root,
        "records/loose-note.md",
        "---\ntype: note\ncreated: 2026-06-01T08:00:00-07:00\nupdated: 2026-06-01T08:00:00-07:00\nsummary: \"A loose note\"\n---\n\n# A\n",
    );

    let store = open(root);
    Index::rebuild_all(&store).unwrap();

    // The canonical rebuild creates the layer `index.jsonl` (loose catalog) and
    // NO `index.md` anywhere — and `validate --all` must accept exactly that.
    let all = validate_all(&store).unwrap();
    assert!(
        !has(&all, codes::INDEX_MISSING),
        "a freshly-rebuilt loose-only store must not report INDEX_MISSING: {all:#?}"
    );
    assert!(
        !all.iter()
            .any(|i| i.severity == dbmd_core::validate::Severity::Error),
        "a freshly-rebuilt loose-only store must validate clean (no errors): {all:#?}"
    );
}

/// Removing the layer `index.jsonl` from a loose-only store MUST still be caught
/// — the loose catalog requirement moved to `INDEX_JSONL_MISSING`, it was not
/// dropped along with the spurious `index.md` requirement.
#[test]
fn loose_only_layer_missing_jsonl_is_still_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fresh_store(root);
    write(
        root,
        "records/loose-note.md",
        "---\ntype: note\ncreated: 2026-06-01T08:00:00-07:00\nupdated: 2026-06-01T08:00:00-07:00\nsummary: \"A loose note\"\n---\n\n# A\n",
    );
    let store = open(root);
    Index::rebuild_all(&store).unwrap();
    // Delete the loose catalog the rebuild created.
    std::fs::remove_file(root.join("records/index.jsonl")).unwrap();

    let all = validate_all(&store).unwrap();
    assert!(
        has(&all, codes::INDEX_JSONL_MISSING),
        "a loose file with no layer index.jsonl must report INDEX_JSONL_MISSING: {all:#?}"
    );
}
