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

/// A fresh tempdir store with a valid `DB.md` identity file and the three layer
/// dirs — the same "truly clean" baseline the in-crate `Fixture::new` builds.
fn fresh_store(dir: &Path) {
    std::fs::write(
        dir.join("DB.md"),
        "---\ntype: db-md\nscope: company\nowner: Test\n---\n",
    )
    .unwrap();
    for layer in ["sources", "records", "wiki"] {
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
    write(root, "records/companies/acme.md", &contact("Acme · Q2 renewal"));

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
        issues
            .iter()
            .any(|i| i.code == codes::WIKI_LINK_SHORT_FORM
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
