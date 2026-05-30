//! `query` — Dataview-style filters, **sidecar-backed**.
//!
//! Resolves against the type-folder `index.jsonl` sidecar(s) via
//! [`Store::find_by_type`] / [`Store::find_by_where`] /
//! [`Store::read_type_index`] — one sequential, complete read per type-folder,
//! cold-cache-proof — **never** a walk-and-parse. Returns full
//! [`IndexRecord`]s straight from the sidecar (path + fields + summary +
//! links); the caller opens the underlying file only if it needs the body.
//!
//! Backs `dbmd search --type/--where`, `dbmd fm query`, `dbmd index query`, and
//! `dbmd graph backlinks --type/--in`.

use serde_json::Value;

use crate::index::IndexRecord;
use crate::store::{Layer, Store, StoreError};

/// A composable, sidecar-backed filter over a store's records.
///
/// Build with [`Query::new`] and the `with_*` methods, then [`Query::execute`].
/// Multiple [`Query::with_where`] clauses AND together (intersection over the
/// sidecar records).
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// `type` predicate (`with_type`).
    type_: Option<String>,
    /// Layer scope (`with_layer` / `--in <layer>`).
    layer: Option<Layer>,
    /// `key=value` frontmatter predicates, ANDed.
    wheres: Vec<(String, String)>,
}

impl Query {
    /// Start a new, empty query (matches everything until narrowed).
    pub fn new() -> Self {
        Self::default()
    }

    /// Restrict to a single `type` (frontmatter `type` predicate).
    ///
    /// Setting it again replaces the previous value — a query has at most one
    /// `type` (a record carries exactly one `type`, so two types would never
    /// intersect).
    pub fn with_type(mut self, type_: &str) -> Self {
        self.type_ = Some(type_.to_string());
        self
    }

    /// Restrict to one layer (`Sources` / `Records` / `Wiki`) — scopes which
    /// sidecars' records survive. Setting it again replaces the previous layer.
    pub fn with_layer(mut self, layer: Layer) -> Self {
        self.layer = Some(layer);
        self
    }

    /// Add a `key=value` frontmatter predicate; chains as AND with any others
    /// (intersection over the sidecar records). Repeating the same `key` adds a
    /// second clause — both must hold — rather than replacing the first.
    pub fn with_where(mut self, key: &str, value: &str) -> Self {
        self.wheres.push((key.to_string(), value.to_string()));
        self
    }

    /// Resolve the query against the relevant type-folder `index.jsonl`
    /// sidecar(s) and return the matching [`IndexRecord`]s — complete, one
    /// sequential read per type-folder, no whole-store walk.
    ///
    /// The candidate set comes from the most selective frozen sidecar reader:
    /// [`Store::find_by_type`] when a `type` is set (one type-folder's
    /// sidecars), otherwise [`Store::find_by_where_in`] on the first `where`
    /// clause — and that reader is **layer-scoped** when [`with_layer`] is set,
    /// so a `--where`-only query reads only the named layer's sidecars instead
    /// of the whole store (O(entities-in-layer), the interactive-loop contract).
    /// The layer scope and every remaining predicate are then applied in memory
    /// over the returned records — no extra sidecar reads, no walk.
    ///
    /// [`with_layer`]: Query::with_layer
    ///
    /// A query that constrains neither `type` nor any `where` clause selects no
    /// sidecar (a bare or layer-only query has no walk-free candidate set under
    /// the sidecar API) and returns an empty result; the CLI always supplies a
    /// `--type` or a `--where`.
    pub fn execute(&self, store: &Store) -> Result<Vec<IndexRecord>, StoreError> {
        // Pick the candidate set from the cheapest frozen sidecar reader, and
        // remember which predicates that reader has already satisfied so the
        // in-memory pass doesn't re-test them.
        let (candidates, type_done, where_done) = if let Some(type_) = &self.type_ {
            // `find_by_type` reads the type's canonical sidecar (or, when that
            // folder isn't indexed yet, the sidecars of just that type's layer —
            // never the whole store); every record it returns already has the
            // right `type`.
            (store.find_by_type(type_)?, true, 0)
        } else if let Some((key, value)) = self.wheres.first() {
            // No type to scope on: let the first `where` clause pick the
            // sidecars and pre-filter. `self.layer` (when set) confines the
            // sidecar walk to that layer's subtree, so a `--where`-only query
            // is O(entities-in-layer), not O(store records) — the in-memory
            // layer filter below then becomes a no-op for this path. The
            // remaining clauses AND in memory.
            (store.find_by_where_in(key, value, self.layer)?, false, 1)
        } else {
            // Nothing selects a sidecar: no walk-free candidate set exists.
            return Ok(Vec::new());
        };

        Ok(self.filter_candidates(candidates, type_done, where_done))
    }

    /// Apply the in-memory predicate pass over a candidate set returned by a
    /// sidecar reader: the `type` predicate (unless `type_already_applied`,
    /// because [`Store::find_by_type`] guarantees it), the [`with_layer`] scope,
    /// and every remaining `where` clause (skipping the first
    /// `wheres_already_applied`, which [`Store::find_by_where`] pre-filtered).
    /// All surviving predicates AND together.
    ///
    /// Split out from [`Query::execute`] so the composition is exercisable over
    /// hand-built [`IndexRecord`]s independent of the sidecar I/O.
    ///
    /// [`with_layer`]: Query::with_layer
    fn filter_candidates(
        &self,
        candidates: Vec<IndexRecord>,
        type_already_applied: bool,
        wheres_already_applied: usize,
    ) -> Vec<IndexRecord> {
        candidates
            .into_iter()
            .filter(|record| {
                if !type_already_applied {
                    if let Some(type_) = &self.type_ {
                        if record.type_ != *type_ {
                            return false;
                        }
                    }
                }
                if let Some(layer) = self.layer {
                    if !record_in_layer(record, layer) {
                        return false;
                    }
                }
                self.wheres
                    .iter()
                    .skip(wheres_already_applied)
                    .all(|(key, value)| record_matches_where(record, key, value))
            })
            .collect()
    }
}

/// True if `record`'s store-relative `path` lives under `layer`'s top-level
/// folder (`sources/` / `records/` / `wiki/`). The sidecar readers can return
/// records from any layer (a `type` folder name is not unique across layers),
/// so a `with_layer` scope is enforced here on the record's path.
fn record_in_layer(record: &IndexRecord, layer: Layer) -> bool {
    record
        .path
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        == Some(layer_dir_name(layer))
}

/// The top-level folder name for a [`Layer`] (`"sources"` / `"records"` /
/// `"wiki"`). Kept local so the layer-scope filter is self-contained and does
/// not couple `query` to the store-walk module's dir-name helpers.
fn layer_dir_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Sources => "sources",
        Layer::Records => "records",
        Layer::Wiki => "wiki",
    }
}

/// True if `record` satisfies a single `key=value` frontmatter predicate.
///
/// The universal-contract keys map to their typed [`IndexRecord`] columns
/// (`type`, `summary`, `created`, `updated`, plus the list-valued `tags` /
/// `links` which match when `value` is one of the members); every other key is
/// looked up in [`IndexRecord::fields`] and compared with
/// [`json_value_matches`]. An absent key never matches.
fn record_matches_where(record: &IndexRecord, key: &str, value: &str) -> bool {
    match key {
        "type" => record.type_ == value,
        "summary" => record.summary == value,
        "path" => record.path.to_str() == Some(value),
        // List-valued columns match on membership: `tags=urgent` is true when
        // `urgent` is one of the file's tags.
        "tags" => record.tags.iter().any(|t| t == value),
        "links" => record.links.iter().any(|l| l == value),
        // Timestamps compare on their canonical RFC3339 string form so a query
        // can pin an exact `created` / `updated`.
        "created" => record.created.map(|t| t.to_rfc3339()).as_deref() == Some(value),
        "updated" => record.updated.map(|t| t.to_rfc3339()).as_deref() == Some(value),
        _ => record
            .fields
            .get(key)
            .is_some_and(|v| json_value_matches(v, value)),
    }
}

/// Compare a sidecar [`Value`] against the string `value` from a `key=value`
/// predicate. The CLI surface is all strings, so matching is defined against
/// the value's natural string form:
///
/// - a string matches when equal;
/// - a number matches when its canonical render equals `value` (so `42` matches
///   `"42"`, and `12.5` matches `"12.5"`);
/// - a bool matches `"true"` / `"false"`;
/// - an array matches when **any** element matches (so a list-valued custom
///   field behaves like `tags` — membership, not whole-list equality);
/// - `null` never matches.
fn json_value_matches(value: &Value, target: &str) -> bool {
    match value {
        Value::String(s) => s == target,
        Value::Number(n) => n.to_string() == target,
        Value::Bool(b) => b.to_string() == target,
        Value::Array(items) => items.iter().any(|item| json_value_matches(item, target)),
        Value::Null => false,
        // Objects have no scalar form a `key=value` predicate can match.
        Value::Object(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // ── Fixtures ─────────────────────────────────────────────────────────────

    /// Build an [`IndexRecord`] with the given store-relative path, type, and
    /// extra (`fields`) frontmatter, leaving the timestamp/list columns empty.
    /// Tests that need `tags`/`links`/`created` set them on the returned value.
    fn rec(path: &str, type_: &str, fields: &[(&str, Value)]) -> IndexRecord {
        IndexRecord {
            path: PathBuf::from(path),
            type_: type_.to_string(),
            summary: format!("summary of {path}"),
            tags: Vec::new(),
            links: Vec::new(),
            created: None,
            updated: None,
            fields: fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }

    /// Serialize one record to a single JSONL line (what a real sidecar holds).
    fn jsonl_line(record: &IndexRecord) -> String {
        serde_json::to_string(record).expect("serialize IndexRecord")
    }

    /// A minimal but valid `DB.md` marker (a `---` frontmatter block, which
    /// `parse_db_md` requires; the body is empty so the config is the default).
    const DB_MD: &str = "---\ntype: db-md\n---\n\n# Test store\n";

    /// Write a temp store: a `DB.md` marker plus an `index.jsonl` sidecar at
    /// each `(store-relative folder, records)` entry. Returns the temp dir
    /// (kept alive by the caller) and the opened [`Store`].
    fn store_with_sidecars(sidecars: &[(&str, &[IndexRecord])]) -> (TempDir, Store) {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path();
        fs::write(root.join("DB.md"), DB_MD).expect("write DB.md");

        for (folder, records) in sidecars {
            let folder_abs = root.join(folder);
            fs::create_dir_all(&folder_abs).expect("create type folder");
            let body: String = records
                .iter()
                .map(|r| format!("{}\n", jsonl_line(r)))
                .collect();
            fs::write(folder_abs.join("index.jsonl"), body).expect("write index.jsonl");
        }

        let store = Store::open(root).expect("open store");
        (dir, store)
    }

    /// The set of store-relative path strings in a result set, for order-
    /// independent assertions.
    fn paths(records: &[IndexRecord]) -> std::collections::BTreeSet<String> {
        records
            .iter()
            .map(|r| r.path.to_string_lossy().into_owned())
            .collect()
    }

    fn path_set(items: &[&str]) -> std::collections::BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── Builder state ────────────────────────────────────────────────────────

    #[test]
    fn builder_accumulates_predicates() {
        let q = Query::new()
            .with_type("contact")
            .with_layer(Layer::Records)
            .with_where("company", "acme")
            .with_where("status", "active");

        assert_eq!(q.type_.as_deref(), Some("contact"));
        assert_eq!(q.layer, Some(Layer::Records));
        assert_eq!(
            q.wheres,
            vec![
                ("company".to_string(), "acme".to_string()),
                ("status".to_string(), "active".to_string()),
            ],
            "each with_where appends a distinct clause"
        );
    }

    #[test]
    fn with_type_and_with_layer_replace_rather_than_stack() {
        let q = Query::new()
            .with_type("contact")
            .with_type("company")
            .with_layer(Layer::Sources)
            .with_layer(Layer::Wiki);
        assert_eq!(q.type_.as_deref(), Some("company"));
        assert_eq!(q.layer, Some(Layer::Wiki));
    }

    #[test]
    fn repeated_with_where_same_key_keeps_both_clauses() {
        // Two clauses on the same key must both be retained (range-style AND),
        // not collapsed to the last one.
        let q = Query::new()
            .with_where("updated", "2026-01-01T00:00:00+00:00")
            .with_where("updated", "2026-02-01T00:00:00+00:00");
        assert_eq!(q.wheres.len(), 2);
    }

    // ── execute: real sidecars on disk ───────────────────────────────────────

    #[test]
    fn execute_with_type_returns_only_that_types_folder() {
        let contacts = [
            rec("records/contacts/sarah.md", "contact", &[]),
            rec("records/contacts/mara.md", "contact", &[]),
        ];
        let companies = [rec("records/companies/acme.md", "company", &[])];
        let (_dir, store) = store_with_sidecars(&[
            ("records/contacts", &contacts),
            ("records/companies", &companies),
        ]);

        let got = Query::new().with_type("contact").execute(&store).unwrap();

        assert_eq!(
            paths(&got),
            path_set(&["records/contacts/sarah.md", "records/contacts/mara.md"]),
            "a type query reads its own type-folder sidecar and excludes other types"
        );
    }

    #[test]
    fn execute_type_plus_where_intersects_on_a_custom_field() {
        let contacts = [
            rec(
                "records/contacts/sarah.md",
                "contact",
                &[("company", Value::String("acme".into()))],
            ),
            rec(
                "records/contacts/mara.md",
                "contact",
                &[("company", Value::String("globex".into()))],
            ),
            rec("records/contacts/no-company.md", "contact", &[]),
        ];
        let (_dir, store) = store_with_sidecars(&[("records/contacts", &contacts)]);

        let got = Query::new()
            .with_type("contact")
            .with_where("company", "acme")
            .execute(&store)
            .unwrap();

        assert_eq!(
            paths(&got),
            path_set(&["records/contacts/sarah.md"]),
            "the where clause narrows the type's records to the matching field; \
             a record missing the key does not match"
        );
    }

    #[test]
    fn execute_multiple_where_clauses_and_together() {
        let contacts = [
            rec(
                "records/contacts/a.md",
                "contact",
                &[
                    ("company", Value::String("acme".into())),
                    ("status", Value::String("active".into())),
                ],
            ),
            rec(
                "records/contacts/b.md",
                "contact",
                &[
                    ("company", Value::String("acme".into())),
                    ("status", Value::String("churned".into())),
                ],
            ),
            rec(
                "records/contacts/c.md",
                "contact",
                &[
                    ("company", Value::String("globex".into())),
                    ("status", Value::String("active".into())),
                ],
            ),
        ];
        let (_dir, store) = store_with_sidecars(&[("records/contacts", &contacts)]);

        let got = Query::new()
            .with_type("contact")
            .with_where("company", "acme")
            .with_where("status", "active")
            .execute(&store)
            .unwrap();

        // Only `a` satisfies BOTH clauses. If the clauses were OR'd, `b` and `c`
        // would leak in.
        assert_eq!(paths(&got), path_set(&["records/contacts/a.md"]));
    }

    #[test]
    fn execute_where_without_type_reads_across_sidecars() {
        // `find_by_where` scans every sidecar; the same `domain` value lives in
        // both a contact and a company record, and both come back.
        let contacts = [rec(
            "records/contacts/sarah.md",
            "contact",
            &[("domain", Value::String("acme.com".into()))],
        )];
        let companies = [
            rec(
                "records/companies/acme.md",
                "company",
                &[("domain", Value::String("acme.com".into()))],
            ),
            rec(
                "records/companies/globex.md",
                "company",
                &[("domain", Value::String("globex.com".into()))],
            ),
        ];
        let (_dir, store) = store_with_sidecars(&[
            ("records/contacts", &contacts),
            ("records/companies", &companies),
        ]);

        let got = Query::new()
            .with_where("domain", "acme.com")
            .execute(&store)
            .unwrap();

        assert_eq!(
            paths(&got),
            path_set(&["records/contacts/sarah.md", "records/companies/acme.md"]),
            "a where-only query matches the field across every type-folder sidecar"
        );
    }

    #[test]
    fn execute_with_layer_scopes_by_path() {
        // Same custom field value present in two layers; the layer scope must
        // keep only the records under the named layer folder.
        let source_recs = [rec(
            "sources/notes/n1.md",
            "note",
            &[("topic", Value::String("billing".into()))],
        )];
        let record_recs = [rec(
            "records/notes/n2.md",
            "note",
            &[("topic", Value::String("billing".into()))],
        )];
        let (_dir, store) = store_with_sidecars(&[
            ("sources/notes", &source_recs),
            ("records/notes", &record_recs),
        ]);

        // Without a layer scope, both layers' records match.
        let unscoped = Query::new()
            .with_where("topic", "billing")
            .execute(&store)
            .unwrap();
        assert_eq!(
            paths(&unscoped),
            path_set(&["sources/notes/n1.md", "records/notes/n2.md"]),
        );

        // Scoped to Sources, only the sources-layer record survives.
        let scoped = Query::new()
            .with_where("topic", "billing")
            .with_layer(Layer::Sources)
            .execute(&store)
            .unwrap();
        assert_eq!(
            paths(&scoped),
            path_set(&["sources/notes/n1.md"]),
            "with_layer(Sources) drops the records/-layer record"
        );
    }

    #[test]
    fn execute_where_only_with_layer_confines_sidecar_io_not_just_result() {
        // The O(entities-in-layer) contract for a `--where`-only query (no
        // `--type`): `--in <layer>` must scope the *sidecar read*, not merely
        // filter the result after a whole-store read. Proven structurally — a
        // corrupt sidecar in another layer would make the read error if it were
        // touched, so a layer-scoped query that SUCCEEDS is proof the
        // out-of-scope layer's I/O never happened.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("DB.md"), DB_MD).unwrap();

        // In-scope layer: a valid sidecar with the matching record.
        let records_dir = root.join("records/contacts");
        fs::create_dir_all(&records_dir).unwrap();
        let match_rec = rec(
            "records/contacts/sarah.md",
            "contact",
            &[("domain", Value::String("acme.com".into()))],
        );
        fs::write(
            records_dir.join("index.jsonl"),
            format!("{}\n", jsonl_line(&match_rec)),
        )
        .unwrap();

        // Out-of-scope layer: a CORRUPT sidecar. If a `--in records` query read
        // it, `read_type_index` would error.
        let sources_dir = root.join("sources/emails");
        fs::create_dir_all(&sources_dir).unwrap();
        fs::write(sources_dir.join("index.jsonl"), "{ not valid json }\n").unwrap();

        let store = Store::open(root).unwrap();

        // Scoped to records: succeeds and returns only the records-layer match,
        // because the corrupt sources sidecar was never walked.
        let scoped = Query::new()
            .with_where("domain", "acme.com")
            .with_layer(Layer::Records)
            .execute(&store)
            .expect("a records-scoped where query must not read the sources sidecar");
        assert_eq!(paths(&scoped), path_set(&["records/contacts/sarah.md"]));

        // Unscoped: the same query DOES walk every layer and trips over the
        // corrupt sidecar — proving the corrupt file is real and that only the
        // layer scope spared the scoped read from reading it.
        let unscoped = Query::new()
            .with_where("domain", "acme.com")
            .execute(&store);
        assert!(
            unscoped.is_err(),
            "an unscoped where query reads every sidecar, including the corrupt one"
        );
    }

    #[test]
    fn execute_full_composition_type_layer_where() {
        let contacts = [
            rec(
                "records/contacts/match.md",
                "contact",
                &[("city", Value::String("denver".into()))],
            ),
            rec(
                "records/contacts/wrong-city.md",
                "contact",
                &[("city", Value::String("austin".into()))],
            ),
        ];
        let (_dir, store) = store_with_sidecars(&[("records/contacts", &contacts)]);

        let got = Query::new()
            .with_type("contact")
            .with_layer(Layer::Records)
            .with_where("city", "denver")
            .execute(&store)
            .unwrap();
        assert_eq!(paths(&got), path_set(&["records/contacts/match.md"]));

        // The same query scoped to the wrong layer yields nothing, proving the
        // layer predicate is live in the composed path.
        let wrong_layer = Query::new()
            .with_type("contact")
            .with_layer(Layer::Wiki)
            .with_where("city", "denver")
            .execute(&store)
            .unwrap();
        assert!(wrong_layer.is_empty());
    }

    #[test]
    fn execute_empty_query_selects_no_sidecar() {
        // A query with neither a type nor a where clause has no walk-free
        // candidate set and must return empty WITHOUT touching the store walk.
        let contacts = [rec("records/contacts/sarah.md", "contact", &[])];
        let (_dir, store) = store_with_sidecars(&[("records/contacts", &contacts)]);

        let got = Query::new().execute(&store).unwrap();
        assert!(
            got.is_empty(),
            "an unconstrained query resolves to empty, not to every record"
        );

        // A layer-only query likewise selects no sidecar (no type/where to pick
        // one), so it is empty too — even though records exist in that layer.
        let layer_only = Query::new()
            .with_layer(Layer::Records)
            .execute(&store)
            .unwrap();
        assert!(layer_only.is_empty());
    }

    #[test]
    fn execute_tag_membership_via_where() {
        let mut urgent = rec("records/tasks/t1.md", "task", &[]);
        urgent.tags = vec!["urgent".into(), "ops".into()];
        let mut calm = rec("records/tasks/t2.md", "task", &[]);
        calm.tags = vec!["ops".into()];
        let recs = [urgent, calm];
        let (_dir, store) = store_with_sidecars(&[("records/tasks", &recs)]);

        let got = Query::new()
            .with_type("task")
            .with_where("tags", "urgent")
            .execute(&store)
            .unwrap();
        assert_eq!(
            paths(&got),
            path_set(&["records/tasks/t1.md"]),
            "tags match on membership: only the record carrying the tag matches"
        );
    }

    #[test]
    fn execute_matches_numeric_and_bool_fields_from_string_predicate() {
        let recs = [
            rec(
                "records/invoices/paid.md",
                "invoice",
                &[
                    ("amount", Value::Number(42.into())),
                    ("paid", Value::Bool(true)),
                ],
            ),
            rec(
                "records/invoices/unpaid.md",
                "invoice",
                &[
                    ("amount", Value::Number(99.into())),
                    ("paid", Value::Bool(false)),
                ],
            ),
        ];
        let (_dir, store) = store_with_sidecars(&[("records/invoices", &recs)]);

        let by_amount = Query::new()
            .with_type("invoice")
            .with_where("amount", "42")
            .execute(&store)
            .unwrap();
        assert_eq!(
            paths(&by_amount),
            path_set(&["records/invoices/paid.md"]),
            "a JSON number matches the string form of the predicate"
        );

        let by_paid = Query::new()
            .with_type("invoice")
            .with_where("paid", "true")
            .execute(&store)
            .unwrap();
        assert_eq!(
            paths(&by_paid),
            path_set(&["records/invoices/paid.md"]),
            "a JSON bool matches \"true\"/\"false\""
        );
    }

    #[test]
    fn execute_honors_last_write_wins_in_sidecar() {
        // Two JSONL lines for the same path: the later supersedes the earlier
        // (read_type_index applies last-write-wins). A query on the superseding
        // field must match, and one on the superseded field must not.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join("DB.md"), DB_MD).unwrap();
        let folder = root.join("records/contacts");
        fs::create_dir_all(&folder).unwrap();

        let old = rec(
            "records/contacts/sarah.md",
            "contact",
            &[("status", Value::String("lead".into()))],
        );
        let new = rec(
            "records/contacts/sarah.md",
            "contact",
            &[("status", Value::String("customer".into()))],
        );
        fs::write(
            folder.join("index.jsonl"),
            format!("{}\n{}\n", jsonl_line(&old), jsonl_line(&new)),
        )
        .unwrap();
        let store = Store::open(root).unwrap();

        let superseding = Query::new()
            .with_type("contact")
            .with_where("status", "customer")
            .execute(&store)
            .unwrap();
        assert_eq!(superseding.len(), 1, "the superseding line's value matches");

        let superseded = Query::new()
            .with_type("contact")
            .with_where("status", "lead")
            .execute(&store)
            .unwrap();
        assert!(
            superseded.is_empty(),
            "the superseded line's value no longer matches after last-write-wins"
        );
    }

    #[test]
    fn execute_returns_full_records_not_just_paths() {
        // The contract returns full IndexRecords straight from the sidecar:
        // summary, tags, links, and fields must survive the round-trip.
        let mut r = rec(
            "records/contacts/sarah.md",
            "contact",
            &[("company", Value::String("acme".into()))],
        );
        r.summary = "Renewal champion".into();
        r.tags = vec!["vip".into()];
        r.links = vec!["wiki/people/sarah-chen.md".into()];
        let recs = [r];
        let (_dir, store) = store_with_sidecars(&[("records/contacts", &recs)]);

        let got = Query::new().with_type("contact").execute(&store).unwrap();
        assert_eq!(got.len(), 1);
        let only = &got[0];
        assert_eq!(only.summary, "Renewal champion");
        assert_eq!(only.tags, vec!["vip".to_string()]);
        assert_eq!(only.links, vec!["wiki/people/sarah-chen.md".to_string()]);
        assert_eq!(
            only.fields.get("company"),
            Some(&Value::String("acme".into())),
            "type-specific fields come back verbatim for on-demand use"
        );
    }

    // ── Pure matcher logic (no store I/O) ────────────────────────────────────

    #[test]
    fn record_matches_where_on_typed_columns() {
        let mut r = rec("records/contacts/x.md", "contact", &[]);
        r.summary = "hello".into();

        assert!(record_matches_where(&r, "type", "contact"));
        assert!(!record_matches_where(&r, "type", "company"));
        assert!(record_matches_where(&r, "summary", "hello"));
        assert!(!record_matches_where(&r, "summary", "goodbye"));
        assert!(record_matches_where(&r, "path", "records/contacts/x.md"));
        assert!(!record_matches_where(&r, "path", "records/contacts/y.md"));
    }

    #[test]
    fn record_matches_where_on_timestamps_uses_rfc3339() {
        let mut r = rec("records/meetings/m.md", "meeting", &[]);
        let ts = chrono::DateTime::parse_from_rfc3339("2026-05-29T12:00:00+00:00").unwrap();
        r.created = Some(ts);

        assert!(record_matches_where(
            &r,
            "created",
            "2026-05-29T12:00:00+00:00"
        ));
        assert!(!record_matches_where(
            &r,
            "created",
            "2026-05-29T13:00:00+00:00"
        ));
        // `updated` is unset → never matches, even the same instant.
        assert!(!record_matches_where(
            &r,
            "updated",
            "2026-05-29T12:00:00+00:00"
        ));
    }

    #[test]
    fn record_matches_where_absent_field_is_false() {
        let r = rec("records/contacts/x.md", "contact", &[]);
        assert!(
            !record_matches_where(&r, "nonexistent", "anything"),
            "an absent frontmatter key never matches"
        );
    }

    #[test]
    fn json_value_matches_covers_scalars_and_arrays() {
        assert!(json_value_matches(&Value::String("acme".into()), "acme"));
        assert!(!json_value_matches(&Value::String("acme".into()), "globex"));

        assert!(json_value_matches(&Value::Number(42.into()), "42"));
        assert!(!json_value_matches(&Value::Number(42.into()), "43"));

        assert!(json_value_matches(&Value::Bool(true), "true"));
        assert!(json_value_matches(&Value::Bool(false), "false"));
        assert!(!json_value_matches(&Value::Bool(true), "false"));

        let arr = Value::Array(vec![Value::String("a".into()), Value::String("b".into())]);
        assert!(json_value_matches(&arr, "b"), "array matches on membership");
        assert!(!json_value_matches(&arr, "c"));
    }

    #[test]
    fn json_value_matches_null_and_object_never_match() {
        assert!(!json_value_matches(&Value::Null, ""));
        assert!(!json_value_matches(&Value::Null, "null"));
        let obj = serde_json::json!({"k": "v"});
        assert!(!json_value_matches(&obj, "v"));
    }

    #[test]
    fn record_in_layer_keys_off_first_path_component() {
        let s = rec("sources/emails/e.md", "email", &[]);
        let r = rec("records/contacts/c.md", "contact", &[]);
        let w = rec("wiki/people/p.md", "wiki-page", &[]);

        assert!(record_in_layer(&s, Layer::Sources));
        assert!(!record_in_layer(&s, Layer::Records));
        assert!(record_in_layer(&r, Layer::Records));
        assert!(!record_in_layer(&r, Layer::Wiki));
        assert!(record_in_layer(&w, Layer::Wiki));
        assert!(!record_in_layer(&w, Layer::Sources));
    }

    #[test]
    fn filter_candidates_skips_already_applied_where_clause() {
        // Simulate the find_by_where path: the first clause is "already applied"
        // by the sidecar reader, so filter_candidates must skip it and only
        // enforce the remaining clause. A record satisfying only the (skipped)
        // first clause but NOT the second must still be dropped.
        let q = Query::new()
            .with_where("company", "acme")
            .with_where("status", "active");

        let keep = rec(
            "records/contacts/keep.md",
            "contact",
            &[
                ("company", Value::String("acme".into())),
                ("status", Value::String("active".into())),
            ],
        );
        let drop = rec(
            "records/contacts/drop.md",
            "contact",
            &[
                ("company", Value::String("acme".into())),
                ("status", Value::String("churned".into())),
            ],
        );

        let out = q.filter_candidates(vec![keep, drop], false, 1);
        assert_eq!(
            paths(&out),
            path_set(&["records/contacts/keep.md"]),
            "the second clause is enforced even when the first is pre-applied"
        );
    }

    #[test]
    fn filter_candidates_enforces_type_when_not_preapplied() {
        // When the candidate set did NOT come from find_by_type (type_applied =
        // false), filter_candidates must still drop records of the wrong type.
        let q = Query::new().with_type("contact");
        let contact = rec("records/contacts/c.md", "contact", &[]);
        let company = rec("records/companies/co.md", "company", &[]);

        let out = q.filter_candidates(vec![contact, company], false, 0);
        assert_eq!(paths(&out), path_set(&["records/contacts/c.md"]));
    }

    /// Local guard: the test fixtures write sidecars under the same canonical
    /// folders the store reader derives, so a `with_type` query finds them.
    /// If this drifts, the integration tests above silently weaken — assert the
    /// convention explicitly.
    #[test]
    fn fixture_canonical_folders_match_store_expectations() {
        let contacts = [rec("records/contacts/x.md", "contact", &[])];
        let (_dir, store) = store_with_sidecars(&[("records/contacts", &contacts)]);
        // `contact` records live at records/contacts/ — the same folder the
        // fixture wrote — so the type read is non-empty.
        let got = store.find_by_type("contact").unwrap();
        assert_eq!(got.len(), 1, "fixture folder == store's canonical folder");
    }
}
