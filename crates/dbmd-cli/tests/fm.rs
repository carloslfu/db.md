//! Integration tests for `dbmd fm` — frontmatter get / set / query / init.
//!
//! Intent-derived from plan Block 5: `get` reads one value; `set` mutates one
//! value atomically and re-sorts the type-folder index write-through, refusing a
//! `DB.md` frozen page; `query` is the sidecar-backed dedup primitive; `init`
//! detects type, seeds timestamps, composes a default `summary`, and folds the
//! file into its index. Read paths run against the committed corpus-a; write
//! paths run against a temp copy so the committed corpora are never mutated. The
//! frozen-page refusal is checked against both corpora's `## Policies`.

mod common;

use std::collections::BTreeSet;

use common::{copy_store_to_temp, corpus_a, corpus_b, dbmd};

// ── fm get ─────────────────────────────────────────────────────────────────────

#[test]
fn get_reads_a_universal_field_from_db_md() {
    // `dbmd fm get DB.md scope` — the SPEC's store-identity example.
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let out = dbmd()
        .current_dir(&store)
        .args(["fm", "get", "DB.md", "scope"])
        .assert()
        .success();
    let v = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(v.trim(), "company");
}

#[test]
fn get_reads_a_type_specific_field() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let out = dbmd()
        .current_dir(&store)
        .args(["fm", "get", "records/contacts/sarah-chen.md", "role"])
        .assert()
        .success();
    let v = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert_eq!(v.trim(), "Director of Operations");
}

#[test]
fn get_json_carries_typed_value() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let out = dbmd()
        .current_dir(&store)
        .args([
            "--json",
            "fm",
            "get",
            "records/contacts/sarah-chen.md",
            "tags",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["key"], serde_json::json!("tags"));
    // `tags` is a YAML list → a JSON array, not a stringified scalar.
    assert_eq!(v["value"], serde_json::json!(["customer", "renewal"]));
}

#[test]
fn get_missing_key_is_a_runtime_error() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    dbmd()
        .current_dir(&store)
        .args(["fm", "get", "records/contacts/sarah-chen.md", "no-such-key"])
        .assert()
        .failure()
        .code(1);
}

// ── fm set ─────────────────────────────────────────────────────────────────────

#[test]
fn set_updates_value_and_keeps_index_in_sync() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());

    dbmd()
        .current_dir(&store)
        .args([
            "fm",
            "set",
            "records/contacts/marcus-okafor.md",
            "status=archived",
        ])
        .assert()
        .success();

    // The file's frontmatter now carries the new value.
    let file = std::fs::read_to_string(store.join("records/contacts/marcus-okafor.md")).unwrap();
    assert!(file.contains("status: archived"), "value written:\n{file}");

    // Write-through: the type-folder sidecar reflects the change (no rebuild).
    let jsonl = std::fs::read_to_string(store.join("records/contacts/index.jsonl")).unwrap();
    let marcus_line = jsonl
        .lines()
        .find(|l| l.contains("marcus-okafor"))
        .expect("marcus still indexed");
    assert!(
        marcus_line.contains("\"status\":\"archived\""),
        "index.jsonl updated write-through; got: {marcus_line}"
    );
}

#[test]
fn set_resorts_index_when_recency_changes() {
    // Bumping `updated` to the newest instant must move the entry to the FRONT
    // of the type-folder index.md (recency-ordered), proving the write-through
    // re-sort, not just an in-place field edit.
    let (_tmp, store) = copy_store_to_temp(&corpus_a());

    dbmd()
        .current_dir(&store)
        .args([
            "fm",
            "set",
            "records/contacts/david-kim.md",
            "updated=2099-01-01T00:00:00Z",
        ])
        .assert()
        .success();

    let md = std::fs::read_to_string(store.join("records/contacts/index.md")).unwrap();
    let first_entry_line = md
        .lines()
        .find(|l| l.starts_with("- [["))
        .expect("index.md has entries");
    assert!(
        first_entry_line.contains("records/contacts/david-kim"),
        "the freshly-bumped record sorts to the top; first entry was: {first_entry_line}"
    );
}

#[test]
fn set_refuses_a_frozen_page_corpus_a() {
    // corpus-a freezes wiki/synthesis/2026-renewal-plan.md.
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let frozen = "wiki/synthesis/2026-renewal-plan.md";
    let before = std::fs::read_to_string(store.join(frozen)).unwrap();

    let out = dbmd()
        .current_dir(&store)
        .args(["--json", "fm", "set", frozen, "status=draft"])
        .assert()
        .failure()
        .code(4);

    // Structured POLICY_FROZEN_PAGE error on stderr (JSON mode).
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(stderr.trim()).expect("json error on stderr");
    assert_eq!(v["error"]["code"], serde_json::json!("POLICY_FROZEN_PAGE"));

    // No write occurred — the frozen file is byte-for-byte unchanged.
    let after = std::fs::read_to_string(store.join(frozen)).unwrap();
    assert_eq!(
        before, after,
        "a frozen-page refusal must not mutate the file"
    );
}

#[test]
fn set_refuses_a_frozen_page_corpus_b() {
    // corpus-b freezes records/decisions/2026-q1-strategy.md (the policy-refusal
    // EXPECTED fixture). Refusal is exit 4 with the same code.
    let (_tmp, store) = copy_store_to_temp(&corpus_b());
    let frozen = "records/decisions/2026-q1-strategy.md";
    let before = std::fs::read_to_string(store.join(frozen)).unwrap();

    dbmd()
        .current_dir(&store)
        .args(["fm", "set", frozen, "status=draft"])
        .assert()
        .failure()
        .code(4);

    let after = std::fs::read_to_string(store.join(frozen)).unwrap();
    assert_eq!(before, after);
}

#[test]
fn set_refuses_a_frozen_page_passed_as_absolute_path() {
    // Regression: `fm set` always opens the store at the CWD (`store.root` is
    // the literal `.`), so an ABSOLUTE `<file>` only matches the relative frozen
    // entry if the path resolution canonicalizes both sides. Before the fix this
    // exited 0 and mutated the frozen file (it added `status: draft`).
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let frozen = "wiki/synthesis/2026-renewal-plan.md";
    let abs = store.join(frozen);
    let before = std::fs::read_to_string(&abs).unwrap();

    dbmd()
        .current_dir(&store)
        .args(["fm", "set", abs.to_str().unwrap(), "status=draft"])
        .assert()
        .failure()
        .code(4);

    let after = std::fs::read_to_string(&abs).unwrap();
    assert_eq!(
        before, after,
        "an absolute-path frozen refusal must not mutate the file"
    );
}

#[test]
fn set_refuses_an_extensionless_frozen_entry() {
    // Regression for the divergent-frozen-policy finding: `fm set` historically
    // used a byte-exact `PathBuf` equality against `config.frozen_pages`, which
    // is `.md`-SENSITIVE. An operator who froze a page with the natural
    // extensionless spelling (`records/decisions/q1`) was silently NOT
    // protected on this surface — `fm set records/decisions/q1.md ...` compared
    // `records/decisions/q1.md != records/decisions/q1` and mutated the frozen
    // file. `fm set` now funnels through the single canonical
    // `Config::frozen_match` (`.md`-insensitive), so the refusal must hold.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = tmp.path();
    common::write_file(
        store,
        "DB.md",
        "---\ntype: db-md\nscope: company\nowner: T\n---\n\n# S\n\n\
         ## Policies\n\n### Frozen pages\n- records/decisions/q1\n",
    );
    let frozen = "records/decisions/q1.md";
    common::write_file(
        store,
        frozen,
        "---\ntype: decision\nsummary: a finalized decision\n---\n# Q1\n",
    );
    let before = std::fs::read_to_string(store.join(frozen)).unwrap();

    let out = dbmd()
        .current_dir(store)
        .args(["--json", "fm", "set", frozen, "status=draft"])
        .assert()
        .failure()
        .code(4);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(stderr.trim()).expect("json error on stderr");
    assert_eq!(v["error"]["code"], serde_json::json!("POLICY_FROZEN_PAGE"));

    let after = std::fs::read_to_string(store.join(frozen)).unwrap();
    assert_eq!(
        before, after,
        "an extensionless frozen entry must still block `fm set` (no mutation)"
    );
}

#[test]
fn set_rejects_a_non_assignment_argument() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    dbmd()
        .current_dir(&store)
        .args([
            "fm",
            "set",
            "records/contacts/sarah-chen.md",
            "no-equals-sign",
        ])
        .assert()
        .failure()
        .code(1);
}

// ── fm query ───────────────────────────────────────────────────────────────────

/// Run `dbmd fm query <args> --dir corpus_a` and return stdout path lines.
fn query_paths(args: &[&str]) -> BTreeSet<String> {
    let out = dbmd()
        .arg("fm")
        .arg("query")
        .args(args)
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    stdout.lines().map(str::to_string).collect()
}

#[test]
fn query_by_typed_field_with_type_scope() {
    let got = query_paths(&["name=Sarah Chen", "--type", "contact"]);
    assert!(got.contains("records/contacts/sarah-chen.md"));
    assert_eq!(got.len(), 1, "exactly the one matching contact: {got:?}");
}

#[test]
fn query_returns_full_records_in_json() {
    let out = dbmd()
        .args([
            "fm",
            "query",
            "email=sarah.chen@northstar.io",
            "--type",
            "contact",
        ])
        .arg("--json")
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arr = v.as_array().expect("array of records");
    assert_eq!(arr.len(), 1);
    // Full record fields come straight from the sidecar.
    assert_eq!(arr[0]["type"], serde_json::json!("contact"));
    assert_eq!(arr[0]["name"], serde_json::json!("Sarah Chen"));
    assert_eq!(
        arr[0]["path"],
        serde_json::json!("records/contacts/sarah-chen.md")
    );
}

#[test]
fn query_limit_caps_results() {
    // tags=vendor spans many expenses; --limit 2 caps the path list.
    let capped = query_paths(&["tags=vendor", "--type", "expense", "--limit", "2"]);
    assert_eq!(capped.len(), 2, "limit caps the result set: {capped:?}");
}

#[test]
fn query_rejects_a_non_assignment() {
    dbmd()
        .args(["fm", "query", "no-equals"])
        .arg("--dir")
        .arg(corpus_a())
        .assert()
        .failure()
        .code(1);
}

// ── fm init ────────────────────────────────────────────────────────────────────

#[test]
fn init_infers_type_seeds_timestamps_and_composes_summary() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    // An externally-dropped contact: no type, no summary, no timestamps.
    common::write_file(
        &store,
        "records/contacts/tom-vega.md",
        "---\nname: Tom Vega\nrole: VP Finance\ncompany: \"[[records/companies/northstar]]\"\n---\n\n# Tom Vega\n\nNew finance contact.\n",
    );

    dbmd()
        .current_dir(&store)
        .args(["fm", "init", "records/contacts/tom-vega.md"])
        .assert()
        .success();

    let file = std::fs::read_to_string(store.join("records/contacts/tom-vega.md")).unwrap();
    // Type inferred from the records/contacts/ folder.
    assert!(file.contains("type: contact"), "type inferred:\n{file}");
    // Timestamps seeded.
    assert!(file.contains("created:"), "created seeded:\n{file}");
    assert!(file.contains("updated:"), "updated seeded:\n{file}");
    // Deterministic default summary composed (resolves the company link to its
    // name); the contact composer is `<role> at <company-name>`.
    assert!(
        file.contains("summary: VP Finance at Northstar Logistics"),
        "default summary composed + company resolved:\n{file}"
    );

    // Folded into the type-folder index write-through (both artifacts).
    let jsonl = std::fs::read_to_string(store.join("records/contacts/index.jsonl")).unwrap();
    assert!(jsonl.contains("tom-vega"), "new file indexed in jsonl");
    let md = std::fs::read_to_string(store.join("records/contacts/index.md")).unwrap();
    assert!(
        md.contains("records/contacts/tom-vega"),
        "new file in index.md"
    );
}

#[test]
fn init_honors_explicit_summary_override() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    common::write_file(
        &store,
        "records/contacts/nina-ray.md",
        "---\nname: Nina Ray\nrole: Analyst\n---\n\n# Nina Ray\n",
    );

    dbmd()
        .current_dir(&store)
        .args([
            "fm",
            "init",
            "records/contacts/nina-ray.md",
            "--summary",
            "Hand-written override summary",
        ])
        .assert()
        .success();

    let file = std::fs::read_to_string(store.join("records/contacts/nina-ray.md")).unwrap();
    assert!(
        file.contains("summary: Hand-written override summary"),
        "explicit --summary wins over the composed default:\n{file}"
    );
}

#[test]
fn init_errors_when_type_cannot_be_inferred() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    // A 2-component path under records/ with no usable type-folder shape and no
    // `type` field — init can't classify it.
    common::write_file(&store, "records/loose.md", "---\nfoo: bar\n---\n\nbody\n");

    dbmd()
        .current_dir(&store)
        .args(["fm", "init", "records/loose.md"])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn init_refuses_a_frozen_page() {
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let frozen = "wiki/synthesis/2026-renewal-plan.md";
    let before = std::fs::read_to_string(store.join(frozen)).unwrap();

    dbmd()
        .current_dir(&store)
        .args(["fm", "init", frozen])
        .assert()
        .failure()
        .code(4);

    let after = std::fs::read_to_string(store.join(frozen)).unwrap();
    assert_eq!(before, after, "init must not touch a frozen page");
}

#[test]
fn init_refuses_a_frozen_page_passed_as_absolute_path() {
    // Regression: same absolute-path bypass as `fm set`. `fm init` opens the
    // store at the CWD, so an ABSOLUTE `<file>` must still canonicalize to the
    // store-relative key the frozen entry uses. Before the fix this exited 0 and
    // rewrote the frozen file's frontmatter.
    let (_tmp, store) = copy_store_to_temp(&corpus_a());
    let frozen = "wiki/synthesis/2026-renewal-plan.md";
    let abs = store.join(frozen);
    let before = std::fs::read_to_string(&abs).unwrap();

    dbmd()
        .current_dir(&store)
        .args(["fm", "init", abs.to_str().unwrap()])
        .assert()
        .failure()
        .code(4);

    let after = std::fs::read_to_string(&abs).unwrap();
    assert_eq!(
        before, after,
        "init must not touch a frozen page addressed by absolute path"
    );
}

#[test]
fn init_refuses_an_extensionless_frozen_entry() {
    // Regression for the divergent-frozen-policy finding (the `fm init` arm):
    // like `fm set`, `fm init` used a byte-exact `.md`-SENSITIVE `PathBuf`
    // equality, so a page frozen with the natural extensionless spelling
    // (`records/decisions/q1`) was silently NOT protected — `fm init
    // records/decisions/q1.md` exited 0 and rewrote the frozen file's
    // frontmatter. Both `fm` write arms now funnel through the single canonical
    // `Config::frozen_match` (`.md`-insensitive), so the refusal must hold.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = tmp.path();
    common::write_file(
        store,
        "DB.md",
        "---\ntype: db-md\nscope: company\nowner: T\n---\n\n# S\n\n\
         ## Policies\n\n### Frozen pages\n- records/decisions/q1\n",
    );
    let frozen = "records/decisions/q1.md";
    common::write_file(
        store,
        frozen,
        "---\ntype: decision\nsummary: a finalized decision\n---\n# Q1\n",
    );
    let before = std::fs::read_to_string(store.join(frozen)).unwrap();

    let out = dbmd()
        .current_dir(store)
        .args(["--json", "fm", "init", frozen])
        .assert()
        .failure()
        .code(4);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(stderr.trim()).expect("json error on stderr");
    assert_eq!(v["error"]["code"], serde_json::json!("POLICY_FROZEN_PAGE"));

    let after = std::fs::read_to_string(store.join(frozen)).unwrap();
    assert_eq!(
        before, after,
        "an extensionless frozen entry must still block `fm init` (no mutation)"
    );
}
