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
        "wiki/topics/clean.md",
        "---\ntype: wiki-page\nsummary: s\n---\nSee [[records/contacts/sarah]].\n",
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
    let clean = std::fs::read_to_string(store.abs("wiki/topics/clean.md")).unwrap();
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
/// (`sources/...` sorts before `wiki/...`) would abort the loop, so the
/// `wiki/topics/late.md` linker that sorts AFTER it would be left dangling at
/// `[[old]]` while the file had already moved. Post-fix every clean linker is
/// rewritten regardless of where the bad one falls in iteration order.
#[test]
fn regression_rename_non_utf8_linker_does_not_strand_later_linkers() {
    let store = Store::new();
    store.seed(
        "records/contacts/sarah.md",
        "---\ntype: contact\nsummary: x\n---\n# Sarah\n",
    );
    // A non-UTF8 linker under `sources/` — sorts BEFORE `wiki/` in the
    // BTreeSet order `find_links_to` returns, so pre-fix it aborts the loop
    // before the `wiki/` linker below is ever reached.
    let mut bad: Vec<u8> = Vec::new();
    bad.extend_from_slice(b"---\ntype: source\nsummary: s\n---\n");
    bad.extend_from_slice(b"[[records/contacts/sarah]] ");
    bad.push(0xFF); // lone 0xFF: never valid UTF-8
    bad.extend_from_slice(b"\n");
    store.seed_bytes("sources/a-import.md", &bad);
    // A clean linker that sorts AFTER the bad one and MUST still be rewritten.
    store.seed(
        "wiki/topics/late.md",
        "---\ntype: wiki-page\nsummary: s\n---\nMentions [[records/contacts/sarah|Sarah]].\n",
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
    let late = std::fs::read_to_string(store.abs("wiki/topics/late.md")).unwrap();
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
        "wiki/topics/clean.md",
        "---\ntype: wiki-page\ncreated: 2026-01-01T00:00:00+00:00\nupdated: 2026-01-01T00:00:00+00:00\nsummary: s\n---\nSee [[records/contacts/sarah]].\n",
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
    let clean = std::fs::read_to_string(store.abs("wiki/topics/clean.md")).unwrap();
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
