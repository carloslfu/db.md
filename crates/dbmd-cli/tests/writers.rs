//! Integration tests for the Block 5 writer subcommands — `write`, `link`,
//! `rename`, and the `spec` bootstrap — driven end-to-end through the compiled
//! `dbmd` binary against throwaway temp stores.
//!
//! These are **intent-derived**: each test pins a behavior the SPEC / plan
//! requires of the writers, exercised the way an agent harness invokes the CLI
//! (process + args + exit code + stdout/stderr + on-disk effect), not a library
//! call. They never touch the committed `tests/corpora/` fixtures.
//!
//! The two load-bearing refusals — **collision** and **frozen-page** — assert
//! not just the structured error and exit code but that *no write happened*
//! (the existing file is untouched / no new file appears). A refusal that still
//! writes would be a silent data-corruption bug.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Absolute path to the `dbmd` binary Cargo built for this integration-test
/// target (Cargo sets `CARGO_BIN_EXE_<name>` for the crate's `[[bin]]`).
const DBMD: &str = env!("CARGO_BIN_EXE_dbmd");

/// A throwaway store: a `TempDir` with a `DB.md` marker plus whatever
/// `## Policies` the test needs.
struct Store {
    dir: TempDir,
}

impl Store {
    /// A store with no policies — the common happy-path case.
    fn new() -> Self {
        Self::with_db_md("---\ntype: db-md\nscope: company\nowner: T\n---\n\n# Store\n")
    }

    /// A store whose `DB.md` body is exactly `db_md` (so a test can declare
    /// frozen pages / ignored types).
    fn with_db_md(db_md: &str) -> Self {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("DB.md"), db_md).expect("write DB.md");
        Store { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn abs(&self, rel: &str) -> std::path::PathBuf {
        self.dir.path().join(rel)
    }

    /// Write a content file verbatim (bypasses the CLI — used to set up
    /// preconditions like "this file already exists").
    fn seed(&self, rel: &str, contents: &str) {
        let abs = self.abs(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, contents).unwrap();
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

    /// Run `dbmd <args>` from **inside** the store with the default `--dir .`,
    /// so `store.root` is the literal `.`. This is the configuration that
    /// exposes the absolute-path frozen-page bypass: with a `.` root, an
    /// absolute target only matches the relative frozen entry if the path
    /// resolution canonicalizes both sides. `args` carry no `--dir`.
    fn run_from_store_dir(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(DBMD);
        cmd.args(args).current_dir(self.root());
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
    /// Parse stdout as JSON (for `--json` success output).
    fn stdout_json(&self) -> serde_json::Value {
        serde_json::from_str(self.stdout.trim())
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {:?}", self.stdout))
    }

    /// Parse stderr as the structured `{"error": {...}}` object (for `--json`
    /// error output).
    fn error_json(&self) -> serde_json::Value {
        serde_json::from_str(self.stderr.trim())
            .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {:?}", self.stderr))
    }
}

/// Pull a single scalar frontmatter value out of a written file's text by key,
/// trimming the `key: value` line. Panics if the key is absent — a test that
/// asks for a field the writer was supposed to seed wants a hard failure, not a
/// silent empty string.
fn fm_value(file_text: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    file_text
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix))
        .map(|v| v.trim().trim_matches(['"', '\'']).to_string())
        .unwrap_or_else(|| panic!("frontmatter key `{key}` not found in:\n{file_text}"))
}

// ── write: happy path + auto-shard + summary ──────────────────────────────────

#[test]
fn write_flat_contact_composes_summary_and_prints_resolved_path() {
    // The store declares a `summary_template` for `contact`; `write` composes the
    // default summary from it (v0.2 has no built-in per-type composer — the
    // template is the store's to declare).
    let store = Store::with_db_md(
        "---\ntype: db-md\nscope: company\nowner: T\n---\n\n## Schemas\n\n### contact\n- summary_template: {role}\n",
    );
    let out = store.run(&[
        "write",
        "records/contacts/sarah.md",
        "--type",
        "contact",
        "--fm",
        "role=VP Sales",
    ]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    // Flat type: path is used as-is, printed back.
    assert_eq!(out.stdout.trim(), "records/contacts/sarah.md");
    // The file exists with a composed summary derived from `role` via the template.
    let written = std::fs::read_to_string(store.abs("records/contacts/sarah.md")).unwrap();
    assert!(written.contains("type: contact"), "{written}");
    assert!(written.contains("summary: VP Sales"), "{written}");
    assert!(written.contains("created:"), "{written}");
    assert!(written.contains("updated:"), "{written}");
}

#[test]
fn write_applies_schema_defaults_without_overwriting_explicit_fields() {
    let store = Store::with_db_md(
        "---\ntype: db-md\nscope: company\nowner: T\n---\n\n## Schemas\n\n### expense\n- currency (default USD)\n- status (default draft)\n",
    );

    let out = store.run(&[
        "write",
        "records/expenses/e1.md",
        "--type",
        "expense",
        "--summary",
        "Office chairs",
        "--fm",
        "date=2026-05-22",
        "--fm",
        "status=approved",
    ]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);

    let written = std::fs::read_to_string(store.abs("records/expenses/2026/05/e1.md")).unwrap();
    assert!(written.contains("currency: USD"), "{written}");
    assert!(
        written.contains("status: approved"),
        "explicit --fm must win over schema default:\n{written}"
    );
}

#[test]
fn write_seeds_created_and_updated_as_valid_rfc3339() {
    // Regression guard for the timestamp-seeding source of truth: `dbmd write`
    // seeds `created`/`updated` from `dbmd_core::now()`. Assert the on-disk
    // values are present AND parse as RFC3339 — not just that the keys appear.
    // A malformed or empty seed (the failure mode when the "now" computation
    // changes) would still satisfy a `contains("created:")` check but break
    // every downstream consumer that parses the field; this test catches it.
    let store = Store::new();
    let out = store.run(&[
        "write",
        "records/contacts/sarah.md",
        "--type",
        "contact",
        "--summary",
        "VP Sales",
    ]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);

    let written = std::fs::read_to_string(store.abs("records/contacts/sarah.md")).unwrap();
    let created = fm_value(&written, "created");
    let updated = fm_value(&written, "updated");

    assert!(
        chrono::DateTime::parse_from_rfc3339(&created).is_ok(),
        "seeded `created` must be valid RFC3339, got {created:?}"
    );
    assert!(
        chrono::DateTime::parse_from_rfc3339(&updated).is_ok(),
        "seeded `updated` must be valid RFC3339, got {updated:?}"
    );
    // Both fields seed from a single `now()` call, so they are identical.
    assert_eq!(
        created, updated,
        "`created` and `updated` must seed from the same instant"
    );
}

#[test]
fn write_source_email_auto_shards_by_date_and_prints_sharded_path() {
    let store = Store::new();
    let out = store.run(&[
        "write",
        "anything/e1.md", // the folder part is ignored; shard_path_for rebuilds it
        "--type",
        "email",
        // v0.2: no built-in per-type composer, so a summary-less record needs an
        // explicit one. This test is about date-sharding, not summary composition.
        "--summary",
        "a@x.com to b@y.com re Renewal",
        "--fm",
        "date=2026-05-22",
        "--fm",
        "from=a@x.com",
        "--fm",
        "to=b@y.com",
        "--fm",
        "subject=Renewal",
    ]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    // Auto-sharded under sources/emails/<YYYY>/<MM>/.
    assert_eq!(out.stdout.trim(), "sources/emails/2026/05/e1.md");
    assert!(store.abs("sources/emails/2026/05/e1.md").exists());
}

#[test]
fn write_conclusion_record_honours_explicit_subfolder() {
    // Conclusion records (the former wiki pages) live in the records layer with a
    // real type + `meta-type: conclusion`: a `synthesis` is filed under
    // `records/synthesis/`, but corpus-a also files conclusions as `profile`s under
    // `records/profiles/` and `project`s under `records/projects/`. An explicit
    // conforming `records/<sub>/<file>` path (right layer for the type) must be
    // honoured verbatim, not rewritten to the type's canonical default — otherwise
    // those folders are unreachable via `dbmd write`.
    for sub in ["profiles", "projects", "synthesis"] {
        let store = Store::new();
        let path = format!("records/{sub}/page.md");
        let out = store.run(&[
            "write",
            &path,
            "--type",
            "synthesis",
            "--summary",
            "s",
            "--fm",
            "meta-type=conclusion",
        ]);
        assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
        assert_eq!(
            out.stdout.trim(),
            path,
            "explicit records/{sub}/ must be honoured"
        );
        assert!(
            store.abs(&path).exists(),
            "file must land in records/{sub}/"
        );
    }
}

#[test]
fn write_conclusion_record_bare_filename_falls_back_to_type_default() {
    // An under-specified path (no conforming type-folder) still resolves to the
    // type's deterministic canonical default — for the `synthesis` conclusion type
    // that is `records/synthesis/` (an unrecognized type maps to `records/<type>`).
    let store = Store::new();
    let out = store.run(&[
        "write",
        "just-a-name.md",
        "--type",
        "synthesis",
        "--summary",
        "s",
        "--fm",
        "meta-type=conclusion",
    ]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "records/synthesis/just-a-name.md");
    assert!(store.abs("records/synthesis/just-a-name.md").exists());
}

#[test]
fn write_conclusion_record_wrong_layer_path_falls_back_to_default_folder() {
    // A path whose layer doesn't match the type's canonical layer is not an
    // honoured type-folder: a records-layer `synthesis` conclusion handed a
    // `sources/…` path falls back to the `records/synthesis` default rather than
    // landing in the wrong layer.
    let store = Store::new();
    let out = store.run(&[
        "write",
        "sources/emails/weird.md",
        "--type",
        "synthesis",
        "--summary",
        "s",
        "--fm",
        "meta-type=conclusion",
    ]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "records/synthesis/weird.md");
    assert!(store.abs("records/synthesis/weird.md").exists());
}

#[test]
fn write_event_record_honours_explicit_subfolder_and_still_shards() {
    // A sharding event type written to its explicit canonical type-folder keeps
    // date-sharding under that folder — the agent's folder is honoured, the
    // `<YYYY>/<MM>` segment is still derived (here from the meeting `date`).
    let store = Store::new();
    let out = store.run(&[
        "write",
        "records/meetings/m1.md",
        "--type",
        "meeting",
        "--summary",
        "s",
        "--fm",
        "date=2026-04-14",
    ]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert_eq!(out.stdout.trim(), "records/meetings/2026/04/m1.md");
    assert!(store.abs("records/meetings/2026/04/m1.md").exists());
}

#[test]
fn write_json_emits_written_path_and_type() {
    let store = Store::new();
    let out = store.run(&[
        "--json",
        "write",
        "records/contacts/sarah.md",
        "--type",
        "contact",
        "--summary",
        "Director of Ops",
    ]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    let v = out.stdout_json();
    assert_eq!(v["written"], "records/contacts/sarah.md");
    assert_eq!(v["type"], "contact");
}

#[test]
fn write_maintains_index_write_through() {
    let store = Store::new();
    let out = store.run(&[
        "write",
        "records/contacts/sarah.md",
        "--type",
        "contact",
        "--summary",
        "VP Sales",
    ]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    // Both catalog artifacts appear in the type-folder, plus the root index.
    assert!(
        store.abs("records/contacts/index.md").exists(),
        "index.md missing"
    );
    assert!(
        store.abs("records/contacts/index.jsonl").exists(),
        "index.jsonl missing"
    );
    assert!(store.abs("index.md").exists(), "root index.md missing");
    let jsonl = std::fs::read_to_string(store.abs("records/contacts/index.jsonl")).unwrap();
    assert!(
        jsonl.contains("\"records/contacts/sarah.md\""),
        "jsonl: {jsonl}"
    );
}

#[test]
fn write_refuses_when_not_a_store() {
    // A bare temp dir with no DB.md.
    let dir = TempDir::new().unwrap();
    let out = Command::new(DBMD)
        .args([
            "--json",
            "write",
            "records/contacts/x.md",
            "--type",
            "contact",
            "--summary",
            "s",
        ])
        .arg("--dir")
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "NOT_A_STORE is exit 3");
    let v: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(v["error"]["code"], "NOT_A_STORE");
}

#[test]
fn write_refuses_malformed_db_md_instead_of_using_default_config() {
    let store =
        Store::with_db_md("---\ntype: db-md\n  bad: : : :\n: : nope\n---\n\n# Broken config\n");
    let out = store.run(&[
        "--json",
        "write",
        "records/contacts/sarah.md",
        "--type",
        "contact",
        "--summary",
        "Sarah",
    ]);

    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(out.error_json()["error"]["code"], "PARSE_ERROR");
    assert!(!store.abs("records/contacts/sarah.md").exists());
}

#[test]
fn write_refuses_paths_outside_the_store() {
    let store = Store::new();
    let outside_dir = TempDir::new().unwrap();
    let outside = outside_dir.path().join("outside.md");
    let out = store.run(&[
        "--json",
        "write",
        outside.to_str().unwrap(),
        "--type",
        "contact",
        "--summary",
        "outside",
    ]);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(out.error_json()["error"]["code"], "PATH_OUTSIDE_STORE");
    assert!(
        !outside.exists(),
        "a refused outside-store write must not create the requested file"
    );
}

// ── write: COLLISION REFUSAL (assert no write happened) ────────────────────────

#[test]
fn write_collision_refuses_with_structured_error_and_does_not_overwrite() {
    let store = Store::new();
    // An existing contact the second write would collide with.
    let original = "---\ntype: contact\nsummary: ORIGINAL SUMMARY\nname: Sarah\n---\n\n# Sarah\n\nOriginal body.\n";
    store.seed("records/contacts/sarah.md", original);

    let out = store.run(&[
        "--json",
        "write",
        "records/contacts/sarah.md",
        "--type",
        "contact",
        "--summary",
        "A DIFFERENT SUMMARY",
    ]);

    // Exit 5 (collision), structured PATH_COLLISION carrying the existing
    // type + summary so the agent can decide update-vs-disambiguate.
    assert_eq!(
        out.code,
        Some(5),
        "collision is exit 5; stderr: {}",
        out.stderr
    );
    let err = out.error_json();
    assert_eq!(err["error"]["code"], "PATH_COLLISION");
    let msg = err["error"]["message"].as_str().unwrap();
    assert!(msg.contains("already exists"), "msg: {msg}");
    assert!(
        msg.contains("contact"),
        "message must carry existing type: {msg}"
    );
    assert!(
        msg.contains("ORIGINAL SUMMARY"),
        "message must carry existing summary: {msg}"
    );

    // CRITICAL: the existing file is byte-for-byte untouched — no overwrite.
    let after = std::fs::read_to_string(store.abs("records/contacts/sarah.md")).unwrap();
    assert_eq!(
        after, original,
        "a refused write must NOT modify the existing file"
    );
}

// ── write: FROZEN-PAGE REFUSAL (assert no write happened) ──────────────────────

#[test]
fn write_frozen_page_refuses_and_creates_no_file() {
    let store = Store::with_db_md(
        "---\ntype: db-md\n---\n\n# Store\n\n## Policies\n\n### Frozen pages\n- records/decisions/frozen.md\n",
    );

    let out = store.run(&[
        "--json",
        "write",
        "records/decisions/frozen.md",
        "--type",
        "decision",
        "--summary",
        "should never be written",
    ]);

    // Exit 4 (policy), structured POLICY_FROZEN_PAGE.
    assert_eq!(
        out.code,
        Some(4),
        "frozen-page refusal is exit 4; stderr: {}",
        out.stderr
    );
    let err = out.error_json();
    assert_eq!(err["error"]["code"], "POLICY_FROZEN_PAGE");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("records/decisions/frozen.md"),
        "error names the frozen path"
    );

    // CRITICAL: nothing was written — the frozen file does not exist, and the
    // refusal happened before any index side effect.
    assert!(
        !store.abs("records/decisions/frozen.md").exists(),
        "a frozen-page refusal must NOT create the file"
    );
    assert!(
        !store.abs("records/decisions/index.md").exists(),
        "a frozen-page refusal must NOT touch the index"
    );
}

#[test]
fn write_frozen_page_refuses_dot_slash_spelling_too() {
    // The frozen list names `records/decisions/frozen.md`; a write addressed as
    // `./records/decisions/frozen.md` must still be caught (path normalization).
    let store = Store::with_db_md(
        "---\ntype: db-md\n---\n\n# S\n\n## Policies\n\n### Frozen pages\n- records/decisions/frozen.md\n",
    );
    let out = store.run(&[
        "write",
        "./records/decisions/frozen.md",
        "--type",
        "decision",
        "--summary",
        "x",
    ]);
    assert_eq!(out.code, Some(4), "stderr: {}", out.stderr);
    assert!(!store.abs("records/decisions/frozen.md").exists());
}

#[test]
fn write_and_rename_refuse_an_extensionless_frozen_entry_identically() {
    // Regression for the divergent-frozen-policy finding: the `### Frozen pages`
    // entry is spelled WITHOUT `.md` (`records/decisions/q1`), the natural
    // extensionless spelling `parse_db_md` stores verbatim. Every write surface
    // must still refuse the real `.md` file. `write` and `rename` historically
    // used a `.md`-SENSITIVE local comparison and silently bypassed this entry
    // (`records/decisions/q1.md` != `records/decisions/q1`); both now funnel
    // through the single canonical `Config::frozen_match`. If either surface
    // regresses to a `.md`-sensitive check, exactly one of these arms flips to
    // exit 0 and the bypass is back — this test fails.
    let db_md =
        "---\ntype: db-md\n---\n\n# S\n\n## Policies\n\n### Frozen pages\n- records/decisions/q1\n";

    // write → the extensionless policy entry must freeze the `.md` target.
    let store = Store::with_db_md(db_md);
    let out = store.run(&[
        "--json",
        "write",
        "records/decisions/q1.md",
        "--type",
        "decision",
        "--summary",
        "should never be written",
    ]);
    assert_eq!(
        out.code,
        Some(4),
        "write must refuse an extensionless frozen entry (exit 4); stderr: {}",
        out.stderr
    );
    assert_eq!(out.error_json()["error"]["code"], "POLICY_FROZEN_PAGE");
    assert!(
        !store.abs("records/decisions/q1.md").exists(),
        "a refused write must NOT create the frozen file"
    );

    // rename → the same extensionless entry must block landing on the `.md`
    // destination, and the source must not move.
    let store = Store::with_db_md(db_md);
    store.seed(
        "records/decisions/draft.md",
        "---\ntype: decision\nsummary: a draft\n---\n# Draft\n",
    );
    let out = store.run(&[
        "rename",
        "records/decisions/draft.md",
        "records/decisions/q1.md",
    ]);
    assert_eq!(
        out.code,
        Some(4),
        "rename must refuse landing on an extensionless frozen entry (exit 4); stderr: {}",
        out.stderr
    );
    assert!(
        store.abs("records/decisions/draft.md").exists(),
        "a refused rename must leave the source in place"
    );
    assert!(
        !store.abs("records/decisions/q1.md").exists(),
        "a refused rename must NOT create the frozen destination"
    );
}

// ── write: ignored-type derivation warning (non-blocking) ──────────────────────

#[test]
fn write_conclusion_record_deriving_from_ignored_type_warns_but_writes() {
    let store = Store::with_db_md(
        "---\ntype: db-md\n---\n\n# S\n\n## Policies\n\n### Ignored types\n- secret\n",
    );
    // A record of an ignored type the conclusion record will derive from.
    store.seed(
        "records/secrets/s.md",
        "---\ntype: secret\nsummary: hush\n---\n\n# secret\n",
    );

    // The policy (POLICY_IGNORED_TYPE_DERIVED) gates on a `meta-type: conclusion`
    // record whose `derived_from` points at an ignored-type record.
    let out = store.run(&[
        "write",
        "records/synthesis/derived.md",
        "--type",
        "synthesis",
        "--summary",
        "A synthesis",
        "--fm",
        "meta-type=conclusion",
        "--fm",
        "derived_from=[[records/secrets/s]]",
    ]);

    // The write SUCCEEDS (ignored-type derivation is a warning, not a block) …
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(store.abs("records/synthesis/derived.md").exists());
    // … and a warning naming the policy is surfaced on stderr.
    assert!(
        out.stderr.contains("ignored-type") && out.stderr.contains("records/secrets/s"),
        "expected an ignored-type-derivation warning on stderr, got: {:?}",
        out.stderr
    );
}

// ── link ──────────────────────────────────────────────────────────────────────

#[test]
fn link_appends_full_path_wiki_link_to_body() {
    let store = Store::new();
    store.seed(
        "records/contacts/sarah.md",
        "---\ntype: contact\nsummary: x\n---\n# Sarah\n\nNotes.\n",
    );
    store.seed(
        "records/companies/acme.md",
        "---\ntype: company\nsummary: y\n---\n# Acme\n",
    );

    let out = store.run(&[
        "link",
        "records/contacts/sarah.md",
        "records/companies/acme",
    ]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);

    let body = std::fs::read_to_string(store.abs("records/contacts/sarah.md")).unwrap();
    assert!(body.contains("[[records/companies/acme]]"), "{body}");
    // Frontmatter + prior body preserved.
    assert!(body.contains("type: contact"));
    assert!(body.contains("Notes."));
}

#[test]
fn link_rejects_short_form_target() {
    let store = Store::new();
    store.seed(
        "records/contacts/sarah.md",
        "---\ntype: contact\nsummary: x\n---\n# Sarah\n",
    );
    let out = store.run(&["--json", "link", "records/contacts/sarah.md", "sarah-chen"]);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(out.error_json()["error"]["code"], "WIKI_LINK_SHORT_FORM");
    // The file must not have gained a short-form link.
    let body = std::fs::read_to_string(store.abs("records/contacts/sarah.md")).unwrap();
    assert!(!body.contains("[[sarah-chen]]"), "{body}");
}

#[test]
fn link_refuses_paths_outside_the_store() {
    let store = Store::new();
    store.seed(
        "records/companies/acme.md",
        "---\ntype: company\nsummary: y\n---\n# Acme\n",
    );
    let outside_dir = TempDir::new().unwrap();
    let outside = outside_dir.path().join("outside.md");
    let outside_body = "---\ntype: note\nsummary: outside\n---\n# Outside\n";
    std::fs::write(&outside, outside_body).unwrap();

    let out = store.run(&[
        "--json",
        "link",
        outside.to_str().unwrap(),
        "records/companies/acme",
    ]);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(out.error_json()["error"]["code"], "PATH_OUTSIDE_STORE");
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        outside_body,
        "a refused outside-store link must not mutate the outside file"
    );
}

#[test]
fn link_refuses_traversal_target() {
    let store = Store::new();
    let original = "---\ntype: contact\nsummary: x\n---\n# Sarah\n";
    store.seed("records/contacts/sarah.md", original);

    let out = store.run(&[
        "--json",
        "link",
        "records/contacts/sarah.md",
        "records/../companies/acme",
    ]);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(out.error_json()["error"]["code"], "PATH_OUTSIDE_STORE");
    assert_eq!(
        std::fs::read_to_string(store.abs("records/contacts/sarah.md")).unwrap(),
        original,
        "a refused traversal target must not append a malformed link"
    );
}

#[test]
fn link_refuses_frozen_from_file() {
    let store = Store::with_db_md(
        "---\ntype: db-md\n---\n\n# S\n\n## Policies\n\n### Frozen pages\n- records/decisions/d.md\n",
    );
    let frozen = "---\ntype: decision\nsummary: x\n---\n# D\n";
    store.seed("records/decisions/d.md", frozen);
    store.seed(
        "records/companies/acme.md",
        "---\ntype: company\nsummary: y\n---\n# A\n",
    );

    let out = store.run(&["link", "records/decisions/d.md", "records/companies/acme"]);
    assert_eq!(out.code, Some(4), "stderr: {}", out.stderr);
    // The frozen file is untouched.
    assert_eq!(
        std::fs::read_to_string(store.abs("records/decisions/d.md")).unwrap(),
        frozen
    );
}

#[test]
fn link_refuses_frozen_from_file_passed_as_absolute_path() {
    // Regression: the frozen-page gate must hold when `<from>` is an ABSOLUTE
    // path and the store is opened from the CWD (default `--dir .`, so
    // `store.root` is the literal `.`). Before the canonicalizing path
    // resolution, `strip_prefix(".")` failed on the absolute target, the raw
    // absolute path never matched the relative frozen entry, and `link` appended
    // to the frozen file with exit 0.
    let store = Store::with_db_md(
        "---\ntype: db-md\n---\n\n# S\n\n## Policies\n\n### Frozen pages\n- records/decisions/d.md\n",
    );
    let frozen = "---\ntype: decision\nsummary: x\n---\n# D\n";
    store.seed("records/decisions/d.md", frozen);
    store.seed(
        "records/companies/acme.md",
        "---\ntype: company\nsummary: y\n---\n# A\n",
    );

    let abs_from = store.abs("records/decisions/d.md");
    let out =
        store.run_from_store_dir(&["link", abs_from.to_str().unwrap(), "records/companies/acme"]);
    assert_eq!(
        out.code,
        Some(4),
        "absolute frozen <from> must be refused; stderr: {}",
        out.stderr
    );
    // No write happened — the frozen file is byte-for-byte unchanged.
    assert_eq!(std::fs::read_to_string(&abs_from).unwrap(), frozen);
}

#[test]
fn link_refuses_an_extensionless_frozen_from_entry() {
    // Regression for the divergent-frozen-policy finding (the `link` arm): the
    // `### Frozen pages` entry is spelled WITHOUT `.md` (`records/decisions/q1`).
    // `link` shares the write surface's frozen gate, which historically used a
    // `.md`-SENSITIVE comparison — so a `link` FROM the real `.md` file was not
    // refused and silently appended a wiki-link to the frozen page. The gate now
    // funnels through the single canonical `Config::frozen_match`; if `link`
    // regresses, this flips to exit 0 and the appended link shows up.
    let store = Store::with_db_md(
        "---\ntype: db-md\n---\n\n# S\n\n## Policies\n\n### Frozen pages\n- records/decisions/q1\n",
    );
    let frozen = "---\ntype: decision\nsummary: finalized\n---\n# Q1\n";
    store.seed("records/decisions/q1.md", frozen);
    store.seed(
        "records/companies/acme.md",
        "---\ntype: company\nsummary: y\n---\n# A\n",
    );

    let out = store.run(&["link", "records/decisions/q1.md", "records/companies/acme"]);
    assert_eq!(
        out.code,
        Some(4),
        "link from a file frozen by an extensionless entry must be refused; stderr: {}",
        out.stderr
    );
    // No wiki-link was appended — the frozen file is byte-for-byte unchanged.
    assert_eq!(
        std::fs::read_to_string(store.abs("records/decisions/q1.md")).unwrap(),
        frozen,
        "a refused link must NOT append to the frozen file"
    );
}

// ── rename ─────────────────────────────────────────────────────────────────────

#[test]
fn rename_moves_file_and_rewrites_incoming_links() {
    let store = Store::new();
    store.seed(
        "records/contacts/sarah.md",
        "---\ntype: contact\nsummary: x\n---\n# Sarah\n",
    );
    // Two linkers reference the old path (one with display text).
    store.seed(
        "records/concepts/a.md",
        "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\nSee [[records/contacts/sarah]].\n",
    );
    store.seed(
        "records/concepts/b.md",
        "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\nWith [[records/contacts/sarah|Sarah]].\n",
    );

    let out = store.run(&[
        "--json",
        "rename",
        "records/contacts/sarah.md",
        "records/contacts/sarah-chen.md",
    ]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);

    // The file moved.
    assert!(!store.abs("records/contacts/sarah.md").exists());
    assert!(store.abs("records/contacts/sarah-chen.md").exists());

    // Both incoming links were rewritten (display text preserved).
    let a = std::fs::read_to_string(store.abs("records/concepts/a.md")).unwrap();
    let b = std::fs::read_to_string(store.abs("records/concepts/b.md")).unwrap();
    assert!(a.contains("[[records/contacts/sarah-chen]]"), "{a}");
    assert!(b.contains("[[records/contacts/sarah-chen|Sarah]]"), "{b}");

    // The reported rewrite count covers both linkers.
    let v = out.stdout_json();
    assert_eq!(v["links_rewritten"], 2);
    assert_eq!(v["renamed"]["from"], "records/contacts/sarah.md");
    assert_eq!(v["renamed"]["to"], "records/contacts/sarah-chen.md");
}

#[test]
fn rename_refuses_when_destination_exists() {
    let store = Store::new();
    store.seed(
        "records/contacts/a.md",
        "---\ntype: contact\nsummary: x\n---\n# A\n",
    );
    let dest = "---\ntype: contact\nsummary: y\n---\n# B\n";
    store.seed("records/contacts/b.md", dest);

    let out = store.run(&["rename", "records/contacts/a.md", "records/contacts/b.md"]);
    assert_eq!(
        out.code,
        Some(5),
        "destination collision is exit 5; stderr: {}",
        out.stderr
    );
    // Neither file changed; the source is still there.
    assert!(
        store.abs("records/contacts/a.md").exists(),
        "source must survive a refused rename"
    );
    assert_eq!(
        std::fs::read_to_string(store.abs("records/contacts/b.md")).unwrap(),
        dest
    );
}

#[test]
fn rename_refuses_paths_outside_the_store() {
    let store = Store::new();
    store.seed(
        "records/contacts/a.md",
        "---\ntype: contact\nsummary: x\n---\n# A\n",
    );
    let outside_dir = TempDir::new().unwrap();
    let outside = outside_dir.path().join("moved.md");

    let out = store.run(&[
        "--json",
        "rename",
        "records/contacts/a.md",
        outside.to_str().unwrap(),
    ]);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(out.error_json()["error"]["code"], "PATH_OUTSIDE_STORE");
    assert!(
        store.abs("records/contacts/a.md").exists(),
        "source must survive a refused outside-store rename"
    );
    assert!(
        !outside.exists(),
        "a refused outside-store rename must not create the destination"
    );
}

#[test]
fn rename_refuses_frozen_source() {
    let store = Store::with_db_md(
        "---\ntype: db-md\n---\n\n# S\n\n## Policies\n\n### Frozen pages\n- records/decisions/d.md\n",
    );
    let frozen = "---\ntype: decision\nsummary: x\n---\n# D\n";
    store.seed("records/decisions/d.md", frozen);
    let out = store.run(&["rename", "records/decisions/d.md", "records/decisions/e.md"]);
    assert_eq!(out.code, Some(4), "stderr: {}", out.stderr);
    // The frozen source is untouched and the destination was never created.
    assert!(store.abs("records/decisions/d.md").exists());
    assert!(!store.abs("records/decisions/e.md").exists());
}

#[test]
fn rename_refuses_frozen_source_passed_as_absolute_path() {
    // Regression: the frozen-page gate must hold when `<old>` is an ABSOLUTE
    // path and the store is opened from the CWD (default `--dir .`). Before the
    // canonicalizing path resolution this MOVED the frozen file with exit 0 —
    // the most destructive of the four bypass vectors.
    let store = Store::with_db_md(
        "---\ntype: db-md\n---\n\n# S\n\n## Policies\n\n### Frozen pages\n- records/decisions/d.md\n",
    );
    let frozen = "---\ntype: decision\nsummary: x\n---\n# D\n";
    store.seed("records/decisions/d.md", frozen);

    let abs_old = store.abs("records/decisions/d.md");
    let out = store.run_from_store_dir(&[
        "rename",
        abs_old.to_str().unwrap(),
        "records/decisions/e.md",
    ]);
    assert_eq!(
        out.code,
        Some(4),
        "absolute frozen <old> must be refused; stderr: {}",
        out.stderr
    );
    // The frozen source did NOT move and the destination was never created.
    assert!(
        abs_old.exists(),
        "frozen source must survive a refused absolute-path rename"
    );
    assert_eq!(std::fs::read_to_string(&abs_old).unwrap(), frozen);
    assert!(!store.abs("records/decisions/e.md").exists());
}

// ── spec ───────────────────────────────────────────────────────────────────────

#[test]
fn spec_prints_bundled_standard() {
    // `spec` needs no store; run the binary directly.
    let out = Command::new(DBMD).arg("spec").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("db.md"), "bundled SPEC should mention db.md");
    assert!(!text.trim().is_empty(), "SPEC output must not be empty");
}

#[test]
fn spec_json_wraps_text_in_object() {
    let out = Command::new(DBMD)
        .args(["--json", "spec"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["spec"].is_string(), "json spec must be a string field");
    assert!(v["spec"].as_str().unwrap().contains("db.md"));
}

#[test]
fn spec_honors_dbmd_spec_env_override() {
    let tmp = TempDir::new().unwrap();
    let custom = tmp.path().join("custom-spec.md");
    std::fs::write(&custom, "CUSTOM SPEC OVERRIDE\n").unwrap();

    let out = Command::new(DBMD)
        .arg("spec")
        .env("DBMD_SPEC", &custom)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "CUSTOM SPEC OVERRIDE\n"
    );
}

#[test]
fn spec_flag_overrides_env() {
    let tmp = TempDir::new().unwrap();
    let env_spec = tmp.path().join("env.md");
    let flag_spec = tmp.path().join("flag.md");
    std::fs::write(&env_spec, "ENV\n").unwrap();
    std::fs::write(&flag_spec, "FLAG\n").unwrap();

    let out = Command::new(DBMD)
        .arg("spec")
        .arg("--spec")
        .arg(&flag_spec)
        .env("DBMD_SPEC", &env_spec)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    // --spec wins over DBMD_SPEC.
    assert_eq!(String::from_utf8_lossy(&out.stdout), "FLAG\n");
}

#[test]
fn spec_missing_override_path_is_a_runtime_error() {
    let out = Command::new(DBMD)
        .args(["--json", "spec", "--spec", "/no/such/spec.md"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "a missing --spec path is exit 1"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(v["error"]["code"], "SPEC_READ_FAILED");
}
