//! Regression tests for confirmed launch-blocking bugs in `dbmd rename`
//! (`crates/dbmd-cli/src/cmd/rename.rs`).
//!
//! Finding #6 — *rename has no rollback: an error mid-rewrite leaves the file
//! moved, links half-rewritten, and indexes stale.* The pre-fix handler moved
//! the file FIRST (`std::fs::rename`) and rewrote linkers SECOND, propagating
//! the first per-linker error via `?`. A single non-UTF8 linker (which
//! `find_links_to`'s lossy ripgrep matcher reports as a hit but
//! `read_to_string` rejects with `InvalidData`) aborted the loop *after* the
//! move, leaving the store half-renamed: file gone from `<old>`, some linkers
//! dangling at `[[old]]`, both folder indexes stale.
//!
//! The fix reorders the operation — every linker is rewritten while the file
//! still sits at `<old>`, and the move happens LAST, only once every rewrite
//! committed — and skips a non-UTF8 linker (with a warning) instead of hard
//! aborting. These tests reconstruct the exact triggers and assert the corrected
//! behavior; each WOULD FAIL against the pre-fix code.
//!
//! Driven end-to-end through the compiled `dbmd` binary against throwaway temp
//! stores (the same shape as `tests/writers.rs`), never touching the committed
//! corpora.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Absolute path to the `dbmd` binary Cargo built for this integration-test
/// target (`CARGO_BIN_EXE_<name>` is set for the crate's `[[bin]]`).
const DBMD: &str = env!("CARGO_BIN_EXE_dbmd");

/// A throwaway store: a `TempDir` with a `DB.md` marker.
struct Store {
    dir: TempDir,
}

impl Store {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("DB.md"),
            "---\ntype: db-md\nscope: company\nowner: T\n---\n\n# Store\n",
        )
        .expect("write DB.md");
        Store { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn abs(&self, rel: &str) -> std::path::PathBuf {
        self.dir.path().join(rel)
    }

    /// Write a content file verbatim, creating parents.
    fn seed(&self, rel: &str, contents: &str) {
        let abs = self.abs(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, contents).unwrap();
    }

    /// Write raw bytes verbatim (for a non-UTF8 linker), creating parents.
    fn seed_bytes(&self, rel: &str, bytes: &[u8]) {
        let abs = self.abs(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, bytes).unwrap();
    }

    /// Run `dbmd <args> --dir <store>` and capture the outcome.
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(DBMD);
        cmd.args(args).arg("--dir").arg(self.root());
        let out = cmd.output().expect("spawn dbmd");
        Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// `run` with `DBMD_NOW` pinned so any auto-maintained timestamp the command
    /// stamps is byte-for-byte deterministic (the reproducibility hook in
    /// `dbmd_core::time`). Used by the re-stamp regression below.
    fn run_now(&self, now: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new(DBMD);
        cmd.args(args)
            .arg("--dir")
            .arg(self.root())
            .env("DBMD_NOW", now);
        let out = cmd.output().expect("spawn dbmd");
        Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

/// The captured result of one `dbmd` invocation.
struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Output {
    fn stdout_json(&self) -> serde_json::Value {
        serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {:?}", self.stdout))
    }
}

/// Finding #6 — the exact trigger. `records/contacts/sarah.md` is linked from a
/// clean linker AND from a linker that carries a `[[records/contacts/sarah]]`
/// line beside a stray non-UTF8 byte (a realistic externally-dropped Latin-1
/// source). `find_links_to`'s lossy ripgrep matches the non-UTF8 file, but the
/// pre-fix `rewrite_links_in_file` hard-failed on `read_to_string` *after* the
/// move — stranding a half-renamed store.
///
/// Corrected behavior asserted here:
///   - the rename SUCCEEDS (exit 0) instead of aborting,
///   - the file actually moved to `<new>` and is gone from `<old>`,
///   - the clean linker WAS rewritten to `[[new]]`,
///   - the non-UTF8 linker is skipped (its bytes survive untouched) and its
///     skip is surfaced as a non-fatal warning.
///
/// Pre-fix this test fails two ways at once: the command exits non-zero (the
/// `?` propagates `InvalidData`), and even the move-then-abort path leaves the
/// store inconsistent — neither of which can happen now.
#[test]
fn regression_rename_skips_non_utf8_linker_and_completes_consistently() {
    let store = Store::new();
    store.seed(
        "records/contacts/sarah.md",
        "---\ntype: contact\nsummary: x\n---\n# Sarah\n",
    );
    // A clean linker that MUST be rewritten.
    store.seed(
        "records/concepts/clean.md",
        "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\nSee [[records/contacts/sarah]].\n",
    );
    // A non-UTF8 linker: a valid ASCII link line PLUS a stray Latin-1 byte
    // (0xE9 = 'é' in Latin-1, invalid as a standalone UTF-8 byte). ripgrep's
    // lossy matcher reports this file as an incoming linker; `read_to_string`
    // rejects it with InvalidData.
    let mut bad: Vec<u8> = Vec::new();
    bad.extend_from_slice(b"---\ntype: source\nsummary: s\n---\n");
    bad.extend_from_slice(b"Ref [[records/contacts/sarah]] here. caf");
    bad.push(0xE9); // lone Latin-1 byte: not valid UTF-8
    bad.extend_from_slice(b"\n");
    store.seed_bytes("sources/import/dropped.md", &bad);

    let out = store.run(&[
        "--json",
        "rename",
        "records/contacts/sarah.md",
        "records/contacts/sarah-chen.md",
    ]);

    // The rename completes rather than aborting on the non-UTF8 linker.
    assert_eq!(
        out.code,
        Some(0),
        "rename must complete despite a non-UTF8 linker; stderr: {}",
        out.stderr
    );

    // The file actually moved — no half-state where it is gone but linkers
    // dangle. (Pre-fix the move happened too, but the loop then aborted; here
    // the move is the LAST step and only runs because every rewrite committed.)
    assert!(
        !store.abs("records/contacts/sarah.md").exists(),
        "source must be moved away from <old>"
    );
    assert!(
        store.abs("records/contacts/sarah-chen.md").exists(),
        "destination must exist at <new>"
    );

    // The clean linker WAS rewritten to the new target.
    let clean = std::fs::read_to_string(store.abs("records/concepts/clean.md")).unwrap();
    assert!(
        clean.contains("[[records/contacts/sarah-chen]]"),
        "clean linker must be retargeted; got: {clean}"
    );
    assert!(
        !clean.contains("[[records/contacts/sarah]]"),
        "clean linker must no longer reference the old path; got: {clean}"
    );

    // The non-UTF8 linker is skipped: its bytes are untouched (still the old
    // link + the stray byte) and the skip is reported as a non-fatal warning.
    let bad_after = std::fs::read(store.abs("sources/import/dropped.md")).unwrap();
    assert_eq!(
        bad_after, bad,
        "the skipped non-UTF8 linker must be left byte-for-byte unchanged"
    );
    assert!(
        out.stderr.contains("non-UTF8") && out.stderr.contains("sources/import/dropped.md"),
        "a skipped non-UTF8 linker must surface a warning naming it; stderr: {}",
        out.stderr
    );

    // The reported rewrite count covers ONLY the linker that actually changed
    // (the clean one), not the skipped non-UTF8 file.
    let v = out.stdout_json();
    assert_eq!(
        v["links_rewritten"], 1,
        "only the clean linker counts as rewritten"
    );
}

/// Finding #6 — the ordering invariant in isolation: the file move is the LAST
/// mutation, so when a linker rewrite would otherwise be a problem the source
/// file is never stranded. This test pins the *positive* guarantee that the
/// fix's reordering provides: with a non-UTF8 linker present, the OTHER linkers
/// are still correctly rewritten AND the move still happens — i.e. one bad
/// externally-dropped source cannot corrupt a rename of an unrelated record.
///
/// Pre-fix, the very first non-UTF8 linker encountered in BTreeSet order
/// (`sources/a-import.md` sorts before `sources/z-late.md`) would abort the
/// loop, so the `sources/z-late.md` linker that sorts AFTER it would be left
/// dangling at `[[old]]` while the file had already moved. Post-fix every clean
/// linker is rewritten regardless of where the bad one falls in iteration order.
#[test]
fn regression_rename_non_utf8_linker_does_not_strand_later_linkers() {
    let store = Store::new();
    store.seed(
        "records/contacts/sarah.md",
        "---\ntype: contact\nsummary: x\n---\n# Sarah\n",
    );
    // A non-UTF8 linker that sorts EARLY — sorts BEFORE the clean linker below in
    // the BTreeSet order `find_links_to` returns, so pre-fix it aborts the loop
    // before the later linker is ever reached.
    let mut bad: Vec<u8> = Vec::new();
    bad.extend_from_slice(b"---\ntype: source\nsummary: s\n---\n");
    bad.extend_from_slice(b"[[records/contacts/sarah]] ");
    bad.push(0xFF); // lone 0xFF: never valid UTF-8
    bad.extend_from_slice(b"\n");
    store.seed_bytes("sources/a-import.md", &bad);
    // A clean linker that sorts AFTER the bad one and MUST still be rewritten.
    store.seed(
        "sources/z-late.md",
        "---\ntype: note\nsummary: s\n---\nMentions [[records/contacts/sarah|Sarah]].\n",
    );

    let out = store.run(&[
        "rename",
        "records/contacts/sarah.md",
        "records/contacts/sarah-chen.md",
    ]);
    assert_eq!(
        out.code,
        Some(0),
        "rename must complete; stderr: {}",
        out.stderr
    );

    // The later-sorting clean linker is rewritten (display preserved) — proof
    // the bad linker did not abort the loop before reaching it.
    let late = std::fs::read_to_string(store.abs("sources/z-late.md")).unwrap();
    assert!(
        late.contains("[[records/contacts/sarah-chen|Sarah]]"),
        "a clean linker sorting after a non-UTF8 linker must still be rewritten; got: {late}"
    );

    // And the move completed.
    assert!(!store.abs("records/contacts/sarah.md").exists());
    assert!(store.abs("records/contacts/sarah-chen.md").exists());
}

/// Finding #6 — the self-link case must keep working after the reorder. A file
/// that links to ITSELF is in `find_links_to`'s result; the fix rewrites it
/// in place at `<old>` (the move hasn't happened yet) and then the deferred
/// move carries the rewritten file to `<new>`. Final state: the file at `<new>`
/// with a `[[new]]` self-link. This guards against the reorder regressing the
/// self-link path (e.g. trying to rewrite at the post-move path before the move).
#[test]
fn regression_rename_rewrites_self_link_through_the_deferred_move() {
    let store = Store::new();
    store.seed(
        "records/contacts/sarah.md",
        "---\ntype: contact\nsummary: x\nlinks:\n  - [[records/contacts/sarah]]\n---\nI am [[records/contacts/sarah]].\n",
    );

    let out = store.run(&[
        "--json",
        "rename",
        "records/contacts/sarah.md",
        "records/contacts/sarah-chen.md",
    ]);
    assert_eq!(
        out.code,
        Some(0),
        "self-link rename must succeed; stderr: {}",
        out.stderr
    );

    assert!(!store.abs("records/contacts/sarah.md").exists());
    let moved = std::fs::read_to_string(store.abs("records/contacts/sarah-chen.md")).unwrap();
    assert!(
        moved.contains("[[records/contacts/sarah-chen]]"),
        "the self-link must be retargeted to the new path; got: {moved}"
    );
    assert!(
        !moved.contains("[[records/contacts/sarah]]"),
        "no stale self-link to the old path may remain; got: {moved}"
    );
}

/// Finding fm.rs:79 (rename surface) — a rename IS an edit of the moved file, so
/// its auto-maintained `updated` must be re-stamped to "now" (the same way
/// `write` seeds it on create and `fm set` bumps it on edit). Without this the
/// moved file keeps a stale `updated`, so `index.md` recency ordering and
/// `dbmd search --updated-after` never reflect the move.
///
/// The companion guarantee is the *no-cascade* rule: rewriting a linker's
/// `[[old]]` → `[[new]]` must NOT bump that linker's `updated`. A link target
/// being renamed is not a fresh edit of every record that mentions it; cascading
/// the bump would pollute recency ordering (every linking record would surface
/// as just-edited). The linker's link text changes, its `updated` does not.
///
/// `DBMD_NOW` is pinned so the re-stamp is byte-for-byte assertable.
#[test]
fn regression_rename_restamps_moved_file_updated_but_not_linkers() {
    let store = Store::new();
    // The moved file carries an OLD `created` + `updated`. After the rename its
    // `created` must survive and its `updated` must advance to the pinned now.
    store.seed(
        "records/contacts/sarah.md",
        "---\ntype: contact\ncreated: 2026-01-01T00:00:00+00:00\nupdated: 2026-01-01T00:00:00+00:00\nsummary: x\n---\n# Sarah\n",
    );
    // A linker whose `updated` is OLD and must stay old: rewriting its link text
    // is not an edit of the linker for recency purposes.
    store.seed(
        "records/concepts/clean.md",
        "---\ntype: concept\nmeta-type: conclusion\ncreated: 2026-01-01T00:00:00+00:00\nupdated: 2026-01-01T00:00:00+00:00\nsummary: s\n---\nSee [[records/contacts/sarah]].\n",
    );

    let now = "2026-05-29T18:00:00Z";
    let out = store.run_now(
        now,
        &[
            "rename",
            "records/contacts/sarah.md",
            "records/contacts/sarah-chen.md",
        ],
    );
    assert_eq!(
        out.code,
        Some(0),
        "rename must succeed; stderr: {}",
        out.stderr
    );

    // The moved file's `updated` is re-stamped to the pinned now; `created` is
    // preserved (a move is not a creation).
    let moved = std::fs::read_to_string(store.abs("records/contacts/sarah-chen.md")).unwrap();
    assert!(
        moved.contains("created: 2026-01-01T00:00:00+00:00"),
        "the moved file's `created` must be preserved; got: {moved}"
    );
    assert!(
        moved.contains("updated: 2026-05-29T18:00:00+00:00"),
        "the moved file's `updated` must be re-stamped to now; got: {moved}"
    );
    assert!(
        !moved.contains("updated: 2026-01-01T00:00:00+00:00"),
        "the stale `updated` must be gone from the moved file; got: {moved}"
    );

    // The linker's link text was rewritten, but its `updated` must NOT be bumped:
    // a renamed link target is not a fresh edit of the linking record.
    let clean = std::fs::read_to_string(store.abs("records/concepts/clean.md")).unwrap();
    assert!(
        clean.contains("[[records/contacts/sarah-chen]]"),
        "the linker's link text must be retargeted; got: {clean}"
    );
    assert!(
        clean.contains("updated: 2026-01-01T00:00:00+00:00"),
        "the linker's `updated` must NOT be cascaded by the rename; got: {clean}"
    );
}

/// Finding fm.rs:79 (rename surface) — the re-stamp must degrade gracefully when
/// the moved file has no parseable frontmatter. A bare file (no `---` block) is
/// a legal rename source; `read_file` errors on it, and the handler skips the
/// re-stamp rather than failing the rename. The file must still move, and its
/// bytes must survive verbatim (no frontmatter is invented).
#[test]
fn regression_rename_moved_file_without_frontmatter_is_not_restamped() {
    let store = Store::new();
    let raw = "no frontmatter here, just text\n";
    store.seed("records/notes/plain.md", raw);

    let out = store.run_now(
        "2026-05-29T18:00:00Z",
        &[
            "rename",
            "records/notes/plain.md",
            "records/notes/plain2.md",
        ],
    );
    assert_eq!(
        out.code,
        Some(0),
        "rename of a frontmatter-less file must still succeed; stderr: {}",
        out.stderr
    );

    assert!(!store.abs("records/notes/plain.md").exists());
    let moved = std::fs::read_to_string(store.abs("records/notes/plain2.md")).unwrap();
    assert_eq!(
        moved, raw,
        "a frontmatter-less moved file must survive byte-for-byte (no re-stamp)"
    );
}

/// Adversarial review — `rename` MUST enforce store containment like `write`.
/// A destination whose parent is an in-store symlink pointing OUTSIDE the store
/// (the store legitimately accepts externally-dropped content carrying symlinks)
/// must be refused, not silently moved out of the store. Pre-fix `rename` ran
/// only the lexical `require_store_relative` gate (which follows symlinks), so
/// `create_dir_all` + `fs::rename` landed the moved file — and a stale index
/// entry pointing at it — outside the store root.
#[cfg(unix)]
#[test]
fn regression_rename_refuses_destination_through_in_store_symlink() {
    use std::os::unix::fs::symlink;

    let store = Store::new();
    store.seed(
        "records/contacts/sarah.md",
        "---\ntype: contact\nsummary: x\n---\n# Sarah\n",
    );
    // A directory OUTSIDE the store, plus an in-store symlink pointing at it.
    let outside = TempDir::new().expect("outside tempdir");
    std::fs::create_dir_all(store.abs("records/links")).unwrap();
    symlink(outside.path(), store.abs("records/links/escape")).unwrap();

    let out = store.run(&[
        "rename",
        "records/contacts/sarah.md",
        "records/links/escape/pwned.md",
    ]);

    assert_ne!(
        out.code,
        Some(0),
        "rename through a symlinked-out dir must be refused; code={:?} stderr={}",
        out.code,
        out.stderr
    );
    assert!(
        out.stderr.contains("PATH_OUTSIDE_STORE") || out.stderr.to_lowercase().contains("outside"),
        "the refusal should name the containment failure; stderr: {}",
        out.stderr
    );
    // The file must NOT have escaped the store, and the source must survive.
    assert!(
        !outside.path().join("pwned.md").exists(),
        "the moved file escaped the store root"
    );
    assert!(
        store.abs("records/contacts/sarah.md").exists(),
        "the source must survive a refused rename"
    );
}

/// Adversarial review (incomplete d14d182 fix) — `rename` must contain the
/// `<old>` SOURCE, not only the destination. An `<old>` reached through an
/// in-store symlink to a directory OUTSIDE the store resolves out of the root;
/// the pre-fix handler guarded only `<new>`, so `fs::rename(old_abs, new_abs)`
/// MOVED the out-of-store file into the store and unlinked its origin —
/// irreversible data loss outside the root. The source guard must refuse it.
#[cfg(unix)]
#[test]
fn regression_rename_refuses_source_through_in_store_symlink() {
    use std::os::unix::fs::symlink;

    let store = Store::new();
    // A precious file OUTSIDE the store, reachable through an in-store symlink.
    let outside = TempDir::new().expect("outside tempdir");
    let precious = outside.path().join("precious.md");
    std::fs::write(
        &precious,
        "---\ntype: contact\nsummary: secret\n---\n# Precious\n",
    )
    .unwrap();
    std::fs::create_dir_all(store.abs("records")).unwrap();
    symlink(outside.path(), store.abs("records/linkdir")).unwrap();

    let out = store.run(&[
        "rename",
        "records/linkdir/precious.md",
        "records/contacts/moved.md",
    ]);

    assert_ne!(
        out.code,
        Some(0),
        "rename of a symlinked-out <old> must be refused; code={:?} stderr={}",
        out.code,
        out.stderr
    );
    assert!(
        out.stderr.contains("PATH_OUTSIDE_STORE") || out.stderr.to_lowercase().contains("outside"),
        "the refusal should name the containment failure; stderr: {}",
        out.stderr
    );
    // The out-of-store file must be untouched (not moved, not unlinked) and the
    // destination must not exist.
    assert!(
        precious.exists(),
        "the out-of-store source must NOT be moved/destroyed by a refused rename"
    );
    assert_eq!(
        std::fs::read_to_string(&precious).unwrap(),
        "---\ntype: contact\nsummary: secret\n---\n# Precious\n",
        "the out-of-store source bytes must survive verbatim"
    );
    assert!(
        !store.abs("records/contacts/moved.md").exists(),
        "nothing must land at the destination"
    );
}

/// Adversarial review (incomplete d14d182 fix) — the linker-rewrite loop must
/// contain each linker too. `find_links_to` walks with `follow_links(true)`, so
/// ripgrep can match a `[[old]]` line in a file that physically lives OUTSIDE
/// the store via a symlinked-in directory; the pre-fix loop `write_atomic`'d the
/// rewrite, mutating bytes outside the root. The fix skips+warns such a linker
/// while the in-store rename still completes.
#[cfg(unix)]
#[test]
fn regression_rename_does_not_rewrite_out_of_store_linker() {
    use std::os::unix::fs::symlink;

    let store = Store::new();
    store.seed(
        "records/contacts/old-name.md",
        "---\ntype: contact\nsummary: x\n---\n# Old\n",
    );
    // An out-of-store linker referencing the in-store record, reachable through
    // an in-store symlinked directory.
    let outside = TempDir::new().expect("outside tempdir");
    let outside_linker = outside.path().join("linker.md");
    std::fs::write(
        &outside_linker,
        "---\ntype: note\nsummary: s\n---\nSee [[records/contacts/old-name]].\n",
    )
    .unwrap();
    std::fs::create_dir_all(store.abs("sources")).unwrap();
    symlink(outside.path(), store.abs("sources/extlink")).unwrap();

    let out = store.run(&[
        "rename",
        "records/contacts/old-name.md",
        "records/contacts/new-name.md",
    ]);

    // The in-store rename still succeeds (one stray out-of-store linker must not
    // abort it).
    assert_eq!(
        out.code,
        Some(0),
        "the in-store rename must complete; stderr: {}",
        out.stderr
    );
    assert!(store.abs("records/contacts/new-name.md").exists());
    assert!(!store.abs("records/contacts/old-name.md").exists());

    // The OUT-OF-STORE linker must be left byte-for-byte unchanged — never
    // rewritten outside the store root — and the skip surfaced as a warning.
    assert_eq!(
        std::fs::read_to_string(&outside_linker).unwrap(),
        "---\ntype: note\nsummary: s\n---\nSee [[records/contacts/old-name]].\n",
        "the out-of-store linker must NOT be rewritten"
    );
    assert!(
        out.stderr.to_lowercase().contains("out-of-store")
            || out.stderr.to_lowercase().contains("symlink"),
        "a skipped out-of-store linker should surface a warning; stderr: {}",
        out.stderr
    );
}

/// Adversarial review — `rename` must fail fast on a non-creatable destination
/// BEFORE mutating any authored linker. A destination whose parent component is
/// an existing FILE (`records/contacts/blocker.md/inner.md`) passes the lexical +
/// containment gates but makes `create_dir_all` fail. The pre-fix handler ran
/// `create_dir_all` AFTER the rewrite loop, so every incoming linker was already
/// rewritten to a `<new>` that never got created — stranding dangling links in
/// authored content. The fix creates the destination parent up-front; on failure
/// the store is left completely untouched.
#[test]
fn regression_rename_non_creatable_destination_leaves_linkers_untouched() {
    let store = Store::new();
    store.seed(
        "records/contacts/sarah.md",
        "---\ntype: contact\nsummary: x\n---\n# Sarah\n",
    );
    // An incoming linker whose body must NOT be mutated by the failed rename.
    let linker_before = "---\ntype: note\nsummary: s\n---\nMet [[records/contacts/sarah]] today.\n";
    store.seed("records/meetings/2026/06/m.md", linker_before);
    // An existing FILE that will be the (invalid) parent component of <new>.
    store.seed(
        "records/contacts/blocker.md",
        "---\ntype: contact\nsummary: b\n---\n# Blocker\n",
    );

    let out = store.run(&[
        "rename",
        "records/contacts/sarah.md",
        "records/contacts/blocker.md/inner.md",
    ]);

    assert_ne!(
        out.code,
        Some(0),
        "a rename onto a file-as-parent destination must fail; stderr: {}",
        out.stderr
    );
    // Zero authored mutations: the linker body is byte-for-byte unchanged.
    assert_eq!(
        std::fs::read_to_string(store.abs("records/meetings/2026/06/m.md")).unwrap(),
        linker_before,
        "a failed rename must not rewrite any authored linker"
    );
    // The source survives in place.
    assert!(
        store.abs("records/contacts/sarah.md").exists(),
        "the source must survive a failed rename"
    );
}

/// Path-safety / data-loss — `rename` must rewrite incoming wiki-links ONLY in
/// db.md content files (under `sources/` or `records/`), never in files outside
/// the two content layers (SPEC § content files). The pre-fix handler fed
/// `find_links_to` — which rides `walk_all_md`, a walk from the store ROOT — into
/// the rewrite loop unfiltered, so a `[[old]]` line in a store-root file, a
/// `scratch/` draft, an `EXPECTED/` test golden, or an `archive/` frozen copy was
/// silently mutated `old → new`, corrupting files the tool does not own. The
/// sibling `graph backlinks` already filters the same scan through the content
/// predicate and correctly returns `[]` for those files; this test pins that the
/// MUTATING surface now agrees:
///   - the real content-layer linker IS rewritten,
///   - the four non-layer files are byte-for-byte UNCHANGED,
///   - `links_rewritten` counts ONLY the content rewrite (no over-count),
///   - `graph backlinks` for the old target stays empty (the contrast surface).
///
/// Pre-fix every one of the four non-layer files is rewritten and
/// `links_rewritten` is inflated to 5; post-fix only the content linker changes.
#[test]
fn regression_rename_rewrites_only_content_layer_files() {
    let store = Store::new();
    store.seed(
        "records/contacts/alice.md",
        "---\ntype: contact\nsummary: Alice\n---\n# Alice\n",
    );
    // A REAL content-layer linker that MUST be rewritten.
    let content_before =
        "---\ntype: note\nsummary: s\n---\nMet [[records/contacts/alice]] today.\n";
    store.seed("records/meetings/2026/06/m.md", content_before);

    // Four files OUTSIDE the two content layers, each carrying a `[[old]]` line.
    // None of these is db.md content; `rename` must never touch their bytes.
    let root_before = "Top-level note linking [[records/contacts/alice]].\n";
    store.seed("NOTES.md", root_before); // store-root file
    let scratch_before =
        "---\ntype: note\nsummary: draft\n---\nMentions [[records/contacts/alice]].\n";
    store.seed("scratch/draft.md", scratch_before); // non-layer dir
    let expected_before = "GOLDEN references [[records/contacts/alice]] verbatim.\n";
    store.seed("EXPECTED/snapshot.md", expected_before); // test golden
    let archive_before = "ARCHIVE frozen: [[records/contacts/alice]]\n";
    store.seed("archive/old.md", archive_before); // frozen copy

    let out = store.run(&[
        "--json",
        "rename",
        "records/contacts/alice.md",
        "records/contacts/bob.md",
    ]);
    assert_eq!(
        out.code,
        Some(0),
        "the rename must succeed; stderr: {}",
        out.stderr
    );

    // The real content-layer linker WAS retargeted.
    let content_after =
        std::fs::read_to_string(store.abs("records/meetings/2026/06/m.md")).unwrap();
    assert!(
        content_after.contains("[[records/contacts/bob]]")
            && !content_after.contains("[[records/contacts/alice]]"),
        "the content linker must be retargeted; got: {content_after}"
    );

    // The four NON-LAYER files are byte-for-byte UNCHANGED — the data-loss fix.
    assert_eq!(
        std::fs::read_to_string(store.abs("NOTES.md")).unwrap(),
        root_before,
        "a store-root file must NOT be rewritten by rename"
    );
    assert_eq!(
        std::fs::read_to_string(store.abs("scratch/draft.md")).unwrap(),
        scratch_before,
        "a scratch/ file (non-layer dir) must NOT be rewritten by rename"
    );
    assert_eq!(
        std::fs::read_to_string(store.abs("EXPECTED/snapshot.md")).unwrap(),
        expected_before,
        "an EXPECTED/ test golden must NOT be rewritten by rename"
    );
    assert_eq!(
        std::fs::read_to_string(store.abs("archive/old.md")).unwrap(),
        archive_before,
        "an archive/ frozen copy must NOT be rewritten by rename"
    );

    // The reported rewrite count reflects ONLY the content rewrite — not the four
    // non-layer files (pre-fix this was 5, the over-count called out in the bug).
    let v = out.stdout_json();
    assert_eq!(
        v["links_rewritten"], 1,
        "only the content-layer linker counts as rewritten; got {v}"
    );

    // Contrast surface: `graph backlinks` for the OLD target already ignored the
    // non-layer files. After the rename the old path has no backlinks at all.
    let bl = store.run(&["--json", "graph", "backlinks", "records/contacts/alice"]);
    assert_eq!(
        bl.code,
        Some(0),
        "graph backlinks must succeed; stderr: {}",
        bl.stderr
    );
    // `graph backlinks --json` emits a bare JSON array of store-relative paths.
    let bl_json = bl.stdout_json();
    let count = bl_json
        .as_array()
        .map(|a| a.len())
        .expect("graph backlinks --json must be an array");
    assert_eq!(
        count, 0,
        "the renamed-away target must have no backlinks; got {bl_json}"
    );
}
