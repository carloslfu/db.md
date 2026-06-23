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

use chrono::{DateTime, FixedOffset};
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
    /// The candidate set comes from the most selective frozen sidecar reader,
    /// always **layer-scoped** when [`with_layer`] is set, so an `--in <layer>`
    /// scope confines the sidecar walk to that layer's subtree
    /// (O(entities-in-layer), the interactive-loop contract):
    ///
    /// - a `type` predicate reads the sidecars across the named layer (or the
    ///   whole store when unscoped) and filters by the frontmatter `type`. The
    ///   folder layout is convention, not enforcement (SPEC), so a record whose
    ///   `type` is filed outside that type's canonical layer — a `contact` in
    ///   `sources/`, a custom `screenshot` that only ever lives in `sources/` —
    ///   is still found, and `--type X --in <other-layer>` returns exactly the
    ///   records of that type filed under the other layer rather than always
    ///   being empty;
    /// - otherwise the first `where` clause picks the sidecars and pre-filters,
    ///   scoped to the layer when set;
    /// - otherwise (a layer scope but no `type`/`where`) the layer's own
    ///   sidecar records are the candidate set, so `--in <layer>` on its own
    ///   enumerates that layer instead of silently returning empty.
    ///
    /// Every remaining predicate is then applied in memory over the returned
    /// records — no extra sidecar reads, no walk.
    ///
    /// [`with_layer`]: Query::with_layer
    ///
    /// A fully bare query (no `type`, no `where`, no layer) constrains nothing
    /// and has no selective candidate set, so it returns an empty result.
    pub fn execute(&self, store: &Store) -> Result<Vec<IndexRecord>, StoreError> {
        // Pick the candidate set from the cheapest frozen sidecar reader, and
        // remember which predicates that reader has already satisfied so the
        // in-memory pass doesn't re-test them.
        let (candidates, type_done, where_done) = if self.type_.is_some() {
            // A `type` predicate resolves over the named layer's sidecars (or
            // the whole store when unscoped), filtering by the frontmatter
            // `type` rather than guessing a single canonical type-folder. This
            // keeps the result complete across every folder — and every layer —
            // the type is filed under, so a record filed outside the type's
            // canonical layer is still returned and `--type X --in <layer>`
            // resolves correctly. The in-memory pass below applies the `type`
            // (and layer, when scoped via `--in`) predicate.
            (store.sidecar_records(self.layer)?, false, 0)
        } else if let Some((key, value)) = self.wheres.first() {
            // No type to scope on: let the first `where` clause pick the
            // sidecars and pre-filter. `self.layer` (when set) confines the
            // sidecar walk to that layer's subtree, so a `--where`-only query
            // is O(entities-in-layer), not O(store records) — the in-memory
            // layer filter below then becomes a no-op for this path. The
            // remaining clauses AND in memory.
            (store.find_by_where_in(key, value, self.layer)?, false, 1)
        } else if let Some(layer) = self.layer {
            // Layer-only (`--in <layer>` with no type/where): enumerate that
            // layer's sidecar records. The in-memory layer filter below is a
            // no-op for this path (the read is already layer-scoped).
            (store.sidecar_records(Some(layer))?, false, 0)
        } else {
            // Nothing selects a sidecar: no walk-free candidate set exists.
            return Ok(Vec::new());
        };

        Ok(self.filter_candidates(candidates, type_done, where_done))
    }

    /// Apply the in-memory predicate pass over a candidate set returned by a
    /// sidecar reader: the `type` predicate (unless `type_already_applied`,
    /// when a reader has already guaranteed it), the [`with_layer`] scope, and
    /// every remaining `where` clause (skipping the first
    /// `wheres_already_applied`, which [`Store::find_by_where_in`] pre-filtered).
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
        // Timestamps compare as instants (both sides parsed as RFC3339) so a
        // `Z`-form query matches a `+00:00`-form stored value and vice versa.
        // A plain string compare of `to_rfc3339()` would disagree with the
        // `Store::find_by_where_in` sidecar pre-filter — which this in-memory
        // pass re-runs over — and silently drop real matches.
        "created" => timestamp_value_matches(record.created, value),
        "updated" => timestamp_value_matches(record.updated, value),
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
/// - `null` never matches (a present-but-null field is treated as no value).
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

/// Match a stored instant against a `key=value` predicate by parsing `value` as
/// RFC3339 and comparing instants. A plain string compare of `to_rfc3339()`
/// (which always emits the numeric `+00:00` offset, never `Z`) would reject a
/// `…Z` query against the identical moment, and disagree with the sidecar
/// pre-filter [`Store::find_by_where_in`], silently dropping real matches.
fn timestamp_value_matches(stored: Option<DateTime<FixedOffset>>, value: &str) -> bool {
    match (stored, DateTime::parse_from_rfc3339(value)) {
        (Some(stored), Ok(queried)) => stored == queried,
        _ => false,
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
            .with_layer(Layer::Records);
        assert_eq!(q.type_.as_deref(), Some("company"));
        assert_eq!(q.layer, Some(Layer::Records));
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
            .with_layer(Layer::Sources)
            .with_where("city", "denver")
            .execute(&store)
            .unwrap();
        assert!(wrong_layer.is_empty());
    }

    #[test]
    fn execute_bare_query_selects_no_sidecar() {
        // A fully bare query (no type, no where, no layer) constrains nothing
        // and has no selective candidate set, so it returns empty WITHOUT
        // resolving to every record in the store.
        let contacts = [rec("records/contacts/sarah.md", "contact", &[])];
        let (_dir, store) = store_with_sidecars(&[("records/contacts", &contacts)]);

        let got = Query::new().execute(&store).unwrap();
        assert!(
            got.is_empty(),
            "an unconstrained query resolves to empty, not to every record"
        );
    }

    #[test]
    fn execute_layer_only_enumerates_that_layer() {
        // Regression (finding #47): a layer-only query (`--in <layer>` with no
        // type/where) must enumerate that layer's records, not silently return
        // []. Records live in two layers; the scope keeps only the named one.
        let contacts = [rec("records/contacts/sarah.md", "contact", &[])];
        let emails = [rec("sources/emails/e.md", "email", &[])];
        let (_dir, store) =
            store_with_sidecars(&[("records/contacts", &contacts), ("sources/emails", &emails)]);

        let records = Query::new()
            .with_layer(Layer::Records)
            .execute(&store)
            .unwrap();
        assert_eq!(
            paths(&records),
            path_set(&["records/contacts/sarah.md"]),
            "a layer-only query enumerates that layer, excluding other layers"
        );

        let sources = Query::new()
            .with_layer(Layer::Sources)
            .execute(&store)
            .unwrap();
        assert_eq!(
            paths(&sources),
            path_set(&["sources/emails/e.md"]),
            "the sources-layer scope returns the sources records"
        );
    }

    #[test]
    fn execute_type_finds_records_filed_outside_canonical_layer() {
        // Regression (finding #42): the folder layout is convention, not
        // enforcement (SPEC). A `contact` filed under sources/ and a custom
        // `screenshot` that only ever lives under sources/ must both be found
        // by `--type`, which filters on the frontmatter type — not the type's
        // canonical layer.
        let source_contacts = [rec("sources/foo/jane.md", "contact", &[])];
        let record_contacts = [rec("records/contacts/sarah.md", "contact", &[])];
        let screenshots = [rec("sources/screenshots/shot1.md", "screenshot", &[])];
        let (_dir, store) = store_with_sidecars(&[
            ("sources/foo", &source_contacts),
            ("records/contacts", &record_contacts),
            ("sources/screenshots", &screenshots),
        ]);

        // `--type contact` returns BOTH the canonical and the non-canonical-
        // layer record (jane under sources/, sarah under records/).
        let contacts = Query::new().with_type("contact").execute(&store).unwrap();
        assert_eq!(
            paths(&contacts),
            path_set(&["records/contacts/sarah.md", "sources/foo/jane.md"]),
            "a type query spans every layer the type is filed under"
        );

        // A custom type that only ever lives under sources/ is still found.
        let shots = Query::new()
            .with_type("screenshot")
            .execute(&store)
            .unwrap();
        assert_eq!(
            paths(&shots),
            path_set(&["sources/screenshots/shot1.md"]),
            "a type filed entirely under sources/ is visible to --type"
        );

        // `--type contact --in sources` resolves to the sources-layer contact,
        // not [] (the previously-dead --type/--in combination).
        let in_sources = Query::new()
            .with_type("contact")
            .with_layer(Layer::Sources)
            .execute(&store)
            .unwrap();
        assert_eq!(
            paths(&in_sources),
            path_set(&["sources/foo/jane.md"]),
            "--type X --in <layer> returns the records of that type under the layer"
        );

        // And `--type contact --in records` keeps only the records-layer one.
        let in_records = Query::new()
            .with_type("contact")
            .with_layer(Layer::Records)
            .execute(&store)
            .unwrap();
        assert_eq!(
            paths(&in_records),
            path_set(&["records/contacts/sarah.md"]),
            "the layer scope confines a type query to the named layer"
        );
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
    fn record_matches_where_timestamp_z_and_offset_spellings_are_equal() {
        // Regression: the in-memory filter compared `to_rfc3339()` (always the
        // `+00:00` form) to the raw predicate string, so a `Z`-spelled query of
        // the identical instant silently failed — and disagreed with the
        // `Store::find_by_where_in` sidecar pre-filter (instant-based),
        // dropping real matches. Both spellings must compare equal now.
        let mut stored_z = rec("records/meetings/m.md", "meeting", &[]);
        stored_z.created =
            Some(chrono::DateTime::parse_from_rfc3339("2026-05-29T12:00:00Z").unwrap());
        assert!(record_matches_where(
            &stored_z,
            "created",
            "2026-05-29T12:00:00Z"
        ));
        assert!(record_matches_where(
            &stored_z,
            "created",
            "2026-05-29T12:00:00+00:00"
        ));

        // Stored as `+00:00`, queried as `Z` — this is the spelling pair that
        // failed before the fix.
        let mut stored_offset = rec("records/meetings/n.md", "meeting", &[]);
        stored_offset.created =
            Some(chrono::DateTime::parse_from_rfc3339("2026-05-29T12:00:00+00:00").unwrap());
        assert!(record_matches_where(
            &stored_offset,
            "created",
            "2026-05-29T12:00:00Z"
        ));

        // A different instant still does not match; an unparseable value is false.
        assert!(!record_matches_where(
            &stored_z,
            "created",
            "2026-05-29T13:00:00Z"
        ));
        assert!(!record_matches_where(
            &stored_z,
            "created",
            "not-a-timestamp"
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
        // A conclusion record (the former wiki-page) lives in the records layer.
        let c = rec("records/profiles/p.md", "profile", &[]);

        assert!(record_in_layer(&s, Layer::Sources));
        assert!(!record_in_layer(&s, Layer::Records));
        assert!(record_in_layer(&r, Layer::Records));
        assert!(!record_in_layer(&r, Layer::Sources));
        assert!(record_in_layer(&c, Layer::Records));
        assert!(!record_in_layer(&c, Layer::Sources));
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
