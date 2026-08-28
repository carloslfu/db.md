// SPDX-License-Identifier: Apache-2.0

//! `dbmd schema` — the declared type contracts, parsed and printed.
//!
//! Read paths run against the committed corpus-a store; the directive
//! round-trip runs against a scratch store. Assertions pin properties (the
//! parsed schema values, exit codes), not incidental prose.

mod common;

use common::{corpus_a, dbmd, write_file};

/// The text form renders every declared type back in the `DB.md ## Schemas`
/// bullet syntax, in the parser's deterministic (alphabetical) type order.
#[test]
fn schema_text_renders_all_declared_types() {
    let assert = dbmd()
        .args(["schema", "--dir"])
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    let expected = "\
### company
- name (required, string)
- domain (required, string)
- industry (string)
- relationship (enum: customer, vendor, partner, prospect)

### contact
- name (required, string)
- email (required, email)
- company (required, link to records/companies/)
- role (string)
- first_touch (date)
- last_touch (date)

### expense
- date (required, date)
- amount (required, currency)
- currency (default USD)
- category (string)
- vendor (required, link to records/companies/)

### invoice
- date (required, date)
- amount (required, currency)
- vendor (required, link to records/companies/)
- status (required, enum: paid, unpaid, void)
- paid_at (date)

### meeting
- date (required, date)
- attendees (required)
- location (string)
- duration_min (int)
";
    assert_eq!(stdout, expected);
}

/// `dbmd schema <type> --json` narrows to one type and emits the uniform
/// per-field shape (absent modifiers as null / empty arrays).
#[test]
fn schema_single_type_json_shape() {
    let assert = dbmd()
        .args(["--json", "schema", "contact", "--dir"])
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    let expected = serde_json::json!({
        "types": {
            "contact": {
                "fields": [
                    {"name": "name", "required": true, "shape": "string",
                     "link_prefix": null, "default": null, "enum": null,
                     "unknown_modifiers": []},
                    {"name": "email", "required": true, "shape": "email",
                     "link_prefix": null, "default": null, "enum": null,
                     "unknown_modifiers": []},
                    {"name": "company", "required": true, "shape": null,
                     "link_prefix": "records/companies", "default": null,
                     "enum": null, "unknown_modifiers": []},
                    {"name": "role", "required": false, "shape": "string",
                     "link_prefix": null, "default": null, "enum": null,
                     "unknown_modifiers": []},
                    {"name": "first_touch", "required": false, "shape": "date",
                     "link_prefix": null, "default": null, "enum": null,
                     "unknown_modifiers": []},
                    {"name": "last_touch", "required": false, "shape": "date",
                     "link_prefix": null, "default": null, "enum": null,
                     "unknown_modifiers": []},
                ],
                "unique": [],
                "summary_template": null,
                "shard": null,
            }
        }
    });
    assert_eq!(parsed, expected);
}

/// An undeclared type is unconstrained: empty text output (pipe-safe), an
/// empty `types` object under `--json`, exit 0 in both modes.
#[test]
fn schema_undeclared_type_prints_nothing() {
    let assert = dbmd()
        .args(["schema", "no-such-type", "--dir"])
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout, ""); // pipe-safe

    let assert = dbmd()
        .args(["--json", "schema", "no-such-type", "--dir"])
        .arg(corpus_a())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed, serde_json::json!({"types": {}}));
}

/// The `unique:` / `summary_template:` / `shard:` directives and a bare
/// modifier-less field all survive the parse, and the text form round-trips
/// as valid `## Schemas` source.
#[test]
fn schema_directives_round_trip() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = dir.path();
    write_file(
        store,
        "DB.md",
        "---\ntype: db-md\nscope: test\nowner: T\n---\n\n# T\n\n## Schemas\n\n\
### order\n\
- placed (required, date)\n\
- customer (required, link to records/contacts/)\n\
- total (currency)\n\
- channel (enum: web, phone)\n\
- notes\n\
- unique: placed, customer\n\
- summary_template: {channel} order for {customer}\n\
- shard: by-date\n",
    );

    let assert = dbmd()
        .args(["schema", "order", "--dir"])
        .arg(store)
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let expected = "\
### order
- placed (required, date)
- customer (required, link to records/contacts/)
- total (currency)
- channel (enum: web, phone)
- notes
- unique: placed, customer
- summary_template: {channel} order for {customer}
- shard: by-date
";
    assert_eq!(stdout, expected);

    let assert = dbmd()
        .args(["--json", "schema", "order", "--dir"])
        .arg(store)
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        parsed["types"]["order"]["unique"],
        serde_json::json!([["placed", "customer"]])
    );
    assert_eq!(
        parsed["types"]["order"]["summary_template"],
        serde_json::json!("{channel} order for {customer}")
    );
    assert_eq!(
        parsed["types"]["order"]["shard"],
        serde_json::json!("by-date")
    );
    assert_eq!(
        parsed["types"]["order"]["fields"][4],
        serde_json::json!({"name": "notes", "required": false, "shape": null,
                           "link_prefix": null, "default": null, "enum": null,
                           "unknown_modifiers": []})
    );
}

/// Outside any store, `schema` fails with the stable `NOT_A_STORE` contract
/// (exit 3), like every store-scoped verb.
#[test]
fn schema_outside_store_is_not_a_store() {
    let dir = tempfile::TempDir::new().unwrap();
    dbmd()
        .args(["schema", "--dir"])
        .arg(dir.path())
        .assert()
        .failure()
        .code(3); // ExitCode::NotAStore
}
