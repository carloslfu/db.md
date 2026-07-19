// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests for `dbmd emit` — the whole-store structured dump —
//! driven through the real `dbmd` binary against a synthetic temp store.
//!
//! Intent-derived: each test pins a property the dump contract requires
//! (the JSON envelope shape, layer classification, link normalization,
//! meta-type defaulting, body extraction, the file-bytes SHA-256) the way a
//! hosting hub or indexer would consume it — parse stdout, address fields
//! structurally, never string-compare the whole document.

mod common;

use std::path::{Path, PathBuf};

use common::{dbmd, write_db_md, write_file};

// ─────────────────────────────────────────────────────────────────────────────
// The fixture store
// ─────────────────────────────────────────────────────────────────────────────

/// A testimonial source with links: a dup pair (`.md` + alias spellings of the
/// same target), a dangling target, and a frontmatter passthrough field.
const NOTE_BODY: &str = "\nCarlos confirmed with [[records/contacts/sarah-chen]] and\n[[records/contacts/sarah-chen.md|Sarah]]; see [[records/decisions/ghost-call]].\n";
const NOTE_FM: &str = "type: note\ncreated: 2026-05-27T08:00:00Z\nupdated: 2026-06-01T09:30:00Z\nsummary: Carlos said the pivot is on\ntold_by: Carlos\n";

/// An entity record: no `meta-type` (⇒ effective `fact`), `name` as the title
/// source, a type-specific field for the verbatim-frontmatter assertion.
const CONTACT: &str = "---\ntype: contact\nname: Sarah Chen\ncreated: 2026-05-01T12:00:00Z\nupdated: 2026-06-02T10:00:00Z\nsummary: Design lead at Acme\ncompany: Acme\n---\n\nMet at the pivot call.\n";

/// A conclusion record with NO summary, an H1 title, and a fenced code block
/// whose `[[...]]` must not count as a link.
const DECISION: &str = "---\ntype: decision\nmeta-type: conclusion\ncreated: 2026-06-03T08:00:00Z\nupdated: 2026-06-03T08:00:00Z\n---\n\n# Ship the pivot\n\nDecided with [[records/contacts/sarah-chen]].\n\n```\n[[records/contacts/fenced-not-a-link]]\n```\n";

/// A hand-written file with no frontmatter block at all.
const PLAIN: &str = "Just a plain note that predates the store discipline.\n";

/// Seed the fixture store into `root` (which already carries `DB.md`).
fn seed_store(root: &Path) {
    let note = format!("---\n{NOTE_FM}---\n{NOTE_BODY}");
    write_file(root, "sources/notes/pivot-call.md", &note);
    write_file(root, "sources/notes/plain.md", PLAIN);
    write_file(root, "records/contacts/sarah-chen.md", CONTACT);
    write_file(root, "records/decisions/pivot.md", DECISION);
    // Derived catalogs must be skipped, per the store's discovery rules.
    write_file(root, "records/contacts/index.md", "# Contacts\n");
    write_file(root, "records/contacts/index.jsonl", "");
}

/// A fresh fixture store in a tempdir: `(guard, root)`.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    write_db_md(&root);
    seed_store(&root);
    (tmp, root)
}

/// Run `dbmd --json emit <root>` and parse stdout as the dump document.
fn emit_json(root: &Path) -> serde_json::Value {
    let out = dbmd().args(["--json", "emit"]).arg(root).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).expect("utf8 stdout");
    serde_json::from_str(stdout.trim()).expect("stdout is one JSON document")
}

/// The emitted file object for `path`, or panic.
fn file<'a>(dump: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    dump["files"]
        .as_array()
        .expect("files is an array")
        .iter()
        .find(|f| f["path"] == path)
        .unwrap_or_else(|| panic!("no emitted file {path} in {dump}"))
}

/// Lowercase-hex SHA-256 — the independent recomputation the dump's `sha256`
/// is checked against.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

// ─────────────────────────────────────────────────────────────────────────────
// The envelope: shape, membership, order, summary counts
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn dump_envelope_has_store_files_and_summary_counts() {
    let (_tmp, root) = fixture();
    let dump = emit_json(&root);

    // Top-level shape.
    assert_eq!(dump["store"], root.to_string_lossy().as_ref());
    assert!(dump["files"].is_array());

    // Membership + order: content files plus DB.md, sorted by path; the
    // derived index.md catalog (and the non-markdown sidecar) never appear.
    let paths: Vec<&str> = dump["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().expect("path is a string"))
        .collect();
    assert_eq!(
        paths,
        vec![
            "DB.md",
            "records/contacts/sarah-chen.md",
            "records/decisions/pivot.md",
            "sources/notes/pivot-call.md",
            "sources/notes/plain.md",
        ]
    );

    // Summary counts: DB.md is a file but belongs to neither layer.
    assert_eq!(dump["summary"]["files"], 5);
    assert_eq!(dump["summary"]["sources"], 2);
    assert_eq!(dump["summary"]["records"], 2);
}

#[test]
fn layers_classify_source_record_and_null_for_db_md() {
    let (_tmp, root) = fixture();
    let dump = emit_json(&root);

    assert_eq!(
        file(&dump, "sources/notes/pivot-call.md")["layer"],
        "source"
    );
    assert_eq!(
        file(&dump, "records/contacts/sarah-chen.md")["layer"],
        "record"
    );
    let db = file(&dump, "DB.md");
    assert!(db["layer"].is_null(), "DB.md carries no layer: {db}");
    // DB.md is still a full member: its config frontmatter and H1 title ride
    // along so a host needs no separate DB.md parse.
    assert_eq!(db["type"], "db-md");
    assert_eq!(db["frontmatter"]["scope"], "company");
    assert_eq!(db["title"], "Test store");
    assert!(db["meta_type"].is_null());
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-file projection: frontmatter, meta-type, title, summary, body, times
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn frontmatter_is_verbatim_and_derived_fields_are_typed() {
    let (_tmp, root) = fixture();
    let dump = emit_json(&root);

    let note = file(&dump, "sources/notes/pivot-call.md");
    // Full frontmatter, values verbatim — including the custom passthrough
    // field and the raw timestamp spellings.
    assert_eq!(note["frontmatter"]["type"], "note");
    assert_eq!(note["frontmatter"]["told_by"], "Carlos");
    assert_eq!(note["frontmatter"]["created"], "2026-05-27T08:00:00Z");
    // Derived typed fields: canonical RFC3339 timestamps, coerced scalars.
    assert_eq!(note["type"], "note");
    assert_eq!(note["summary"], "Carlos said the pivot is on");
    assert_eq!(note["created"], "2026-05-27T08:00:00+00:00");
    assert_eq!(note["updated"], "2026-06-01T09:30:00+00:00");
    // A source carries no meta-type.
    assert!(note["meta_type"].is_null());

    let contact = file(&dump, "records/contacts/sarah-chen.md");
    assert_eq!(contact["frontmatter"]["company"], "Acme");
    assert_eq!(
        contact["title"], "Sarah Chen",
        "title from the `name` field"
    );
}

#[test]
fn meta_type_defaults_to_fact_for_records_and_keeps_declared_values() {
    let (_tmp, root) = fixture();
    let dump = emit_json(&root);

    // No declared meta-type on a record ⇒ the SPEC default.
    assert_eq!(
        file(&dump, "records/contacts/sarah-chen.md")["meta_type"],
        "fact"
    );
    // A declared value passes through verbatim.
    assert_eq!(
        file(&dump, "records/decisions/pivot.md")["meta_type"],
        "conclusion"
    );
}

#[test]
fn missing_summary_is_null_and_title_falls_back_to_first_h1() {
    let (_tmp, root) = fixture();
    let dump = emit_json(&root);

    let decision = file(&dump, "records/decisions/pivot.md");
    assert!(
        decision["summary"].is_null(),
        "no summary field ⇒ null, never an invented one: {decision}"
    );
    assert_eq!(decision["title"], "Ship the pivot");
}

#[test]
fn body_is_the_verbatim_text_after_the_frontmatter_block() {
    let (_tmp, root) = fixture();
    let dump = emit_json(&root);

    // With a frontmatter block: everything after the closing fence, verbatim.
    assert_eq!(
        file(&dump, "sources/notes/pivot-call.md")["body"],
        NOTE_BODY
    );

    // Without one: the whole text is the body and the frontmatter is empty.
    let plain = file(&dump, "sources/notes/plain.md");
    assert_eq!(plain["body"], PLAIN);
    assert_eq!(
        plain["frontmatter"],
        serde_json::json!({}),
        "no frontmatter block ⇒ empty object"
    );
    assert!(plain["type"].is_null());
    assert_eq!(plain["layer"], "source");
}

// ─────────────────────────────────────────────────────────────────────────────
// Links: normalization, dedup, fence-awareness, dangling targets
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn links_are_normalized_md_paths_deduped_with_dangling_kept() {
    let (_tmp, root) = fixture();
    let dump = emit_json(&root);

    // Alias stripped (text before `|`), `.md` appended, the bare and `.md`
    // spellings of one target collapsed, first-appearance order; the dangling
    // target is still emitted (existence is validate's concern, not the dump's).
    assert_eq!(
        file(&dump, "sources/notes/pivot-call.md")["links"],
        serde_json::json!([
            "records/contacts/sarah-chen.md",
            "records/decisions/ghost-call.md",
        ])
    );

    // A `[[...]]` inside a fenced code block is code, not an edge.
    assert_eq!(
        file(&dump, "records/decisions/pivot.md")["links"],
        serde_json::json!(["records/contacts/sarah-chen.md"])
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// sha256: the digest of the exact file bytes
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn sha256_is_the_hex_digest_of_the_raw_file_bytes() {
    let (_tmp, root) = fixture();
    let dump = emit_json(&root);

    // Recompute independently from the fixture constants (the bytes on disk).
    assert_eq!(
        file(&dump, "records/contacts/sarah-chen.md")["sha256"],
        sha256_hex(CONTACT.as_bytes())
    );
    assert_eq!(
        file(&dump, "sources/notes/plain.md")["sha256"],
        sha256_hex(PLAIN.as_bytes())
    );

    // And against the actual on-disk bytes for a composed file, so the
    // assertion cannot drift from what was written.
    let note_bytes = std::fs::read(root.join("sources/notes/pivot-call.md")).unwrap();
    assert_eq!(
        file(&dump, "sources/notes/pivot-call.md")["sha256"],
        sha256_hex(&note_bytes)
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Text mode + the not-a-store failure
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn text_mode_prints_the_emitted_paths_one_per_line() {
    let (_tmp, root) = fixture();
    let out = dbmd().arg("emit").arg(&root).assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(
        stdout,
        "DB.md\nrecords/contacts/sarah-chen.md\nrecords/decisions/pivot.md\nsources/notes/pivot-call.md\nsources/notes/plain.md\n"
    );
}

#[test]
fn a_non_store_dir_fails_with_not_a_store_exit_3() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let out = dbmd()
        .args(["--json", "emit"])
        .arg(tmp.path())
        .assert()
        .failure()
        .code(3);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    let err: serde_json::Value = serde_json::from_str(stderr.trim()).expect("structured error");
    assert_eq!(err["error"]["code"], "NOT_A_STORE");
}
