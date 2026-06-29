//! `store` — walk, locate, and shard a db.md store.
//!
//! A db.md store is one directory marked by an uppercase `DB.md` at its root.
//! [`Store::open`] is the single gate every store-walking subcommand goes
//! through; a missing `DB.md` is the [`NotAStore`] error (`NOT_A_STORE`). The
//! toolkit never guesses a store root.
//!
//! Scale discipline lives here: [`Store::walk`] and the layer/type-folder
//! walks are **SWEEP** primitives used only by `validate --all`,
//! `index rebuild`, and `stats`. The interactive loop instead uses
//! [`Store::find_links_to`] / [`Store::find_links_to_any`] (a single
//! presence-only content scan) and the `index.jsonl` sidecar readers
//! ([`Store::find_by_type`] / [`Store::find_by_where`] /
//! [`Store::read_type_index`]) — never a whole-store parse. The batch
//! [`Store::find_links_to_any`] is what keeps the working-set validate's
//! incoming-linker discovery a single store scan rather than one scan per
//! changed object.
//!
//! Link edges are defined once, here, by the shared [`extract_edge_targets`] /
//! [`canonical_link_target`] / [`link_edge_key`] helpers (fence-aware,
//! whitespace-trimmed, case-folded to the filesystem), so the forward view
//! (`graph::forwardlinks`), the backward view ([`Store::find_links_to_any`]),
//! `rename`, and `validate` all agree on exactly which `[[...]]` is an edge.
//! [`ensure_path_within_store`] is the within-store containment gate every
//! caller-influenced path passes through before it is read or traversed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, FixedOffset};
use ignore::WalkBuilder;

use crate::index::IndexRecord;
use crate::parser::{parse_db_md, Config, Frontmatter};

/// Basenames that are never content files: the config marker and the two
/// curator-maintained catalogs. The store walks skip these so a SWEEP over the
/// content layers never mistakes a catalog for a record.
///
/// Only `index.md` is excluded by basename, because the content walks traverse
/// the layer dirs (`sources/`/`records/`) and `index.md` is the only
/// meta file that appears INSIDE them. The root `DB.md` / `log.md` (and the
/// `log/` archive) live at the store root, outside every layer, so they are
/// never reached by these walks — and a content file that merely happens to be
/// named `DB.md` or `log.md` inside a layer (e.g. `records/docs/DB.md`) is real
/// content the SPEC does NOT reserve at type-folder depth.
const NON_CONTENT_BASENAMES: [&str; 1] = ["index.md"];

/// The complete machine-twin sidecar that backs every structured read.
const TYPE_INDEX_FILE: &str = "index.jsonl";

/// Returned when a path is opened as a store but has no `DB.md` at its root.
/// Surfaced as the structured code `NOT_A_STORE` with a non-zero exit.
#[derive(Debug, thiserror::Error)]
#[error("not a db.md store: {path} has no DB.md")]
pub struct NotAStore {
    /// The path that was inspected.
    pub path: PathBuf,
}

/// Errors from store-level operations (walk, locate, shard, sidecar read).
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A sidecar `index.jsonl` could not be read or parsed.
    #[error("failed to read type index {path}: {message}")]
    BadTypeIndex {
        /// The sidecar file.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },

    /// A required date field for sharding was absent or unparseable, and there
    /// was no usable fallback.
    #[error("cannot compute shard path for {file}: no usable date field")]
    NoShardDate {
        /// The file being placed.
        file: PathBuf,
    },

    /// An embedded-ripgrep scan failed to start or run.
    #[error("search failed under {root}: {message}")]
    Search {
        /// The root the scan ran under.
        root: PathBuf,
        /// What went wrong.
        message: String,
    },

    /// An underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The three canonical layers of a db.md store.
///
/// `Ord`/`PartialOrd` are derived (additively) because sibling modules key
/// `BTreeMap`s on `Layer` (e.g. `stats::Stats::files_per_layer`); the canonical
/// declaration order (`Sources` < `Records`) is the sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    /// `sources/` — raw evidence (documentary + testimonial); immutable; date-sharded at scale.
    Sources,
    /// `records/` — everything the agent authors; meta-typed fact/operational/conclusion; entity types flat, event types sharded.
    Records,
}

impl Layer {
    /// The on-disk folder name for this layer (`"sources"` / `"records"`).
    pub fn dir_name(self) -> &'static str {
        match self {
            Layer::Sources => "sources",
            Layer::Records => "records",
        }
    }

    /// Parse a layer from its folder name; `None` for anything else.
    pub fn from_dir_name(name: &str) -> Option<Self> {
        match name {
            "sources" => Some(Layer::Sources),
            "records" => Some(Layer::Records),
            _ => None,
        }
    }

    /// Every layer, in canonical order.
    pub fn all() -> [Layer; 2] {
        [Layer::Sources, Layer::Records]
    }
}

/// An opened db.md store: its root path plus the parsed `DB.md` [`Config`].
///
/// Construct via [`Store::open`]; that is the only path in, and it validates
/// the `DB.md` marker so downstream code can assume a real store.
#[derive(Debug, Clone)]
pub struct Store {
    /// The store root (the directory containing `DB.md`).
    pub root: PathBuf,
    /// The parsed `DB.md` config (agent instructions, policies, schemas).
    pub config: Config,
}

impl Store {
    /// True if `path` is a db.md store root: an uppercase `DB.md` file exists
    /// at `path`. On case-sensitive filesystems a lowercase `db.md` must NOT
    /// count (the lowercase name refers to the project/spec, not the marker).
    pub fn is_db_md_store(path: &Path) -> bool {
        // Read the directory and match the *stored* filename byte-for-byte.
        // `path.join("DB.md").exists()` would lie on a case-insensitive
        // filesystem (macOS default), where a lowercase `db.md` answers a
        // `DB.md` probe. `read_dir` returns the real on-disk name, so the
        // exact-match check is correct on both case-sensitive (Linux) and
        // case-insensitive filesystems.
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        for entry in entries.flatten() {
            if entry.file_name() == "DB.md" {
                // A directory literally named `DB.md` is not the marker.
                match entry.file_type() {
                    Ok(ft) if ft.is_dir() => return false,
                    Ok(_) => return true,
                    Err(_) => return false,
                }
            }
        }
        false
    }

    /// Open `path` as a db.md store and require `DB.md` to be readable and
    /// parseable. Normal commands should enter through this strict gate so a
    /// damaged config cannot silently disable schema or policy rules.
    pub fn open_strict(path: &Path) -> crate::Result<Store> {
        if !Store::is_db_md_store(path) {
            return Err(NotAStore {
                path: path.to_path_buf(),
            }
            .into());
        }
        let db_md = path.join("DB.md");
        let text = std::fs::read_to_string(&db_md)?;
        let config = parse_db_md(&text, &db_md)?;
        Ok(Store {
            root: path.to_path_buf(),
            config,
        })
    }

    /// Open `path` as a db.md store: confirm the `DB.md` marker (else
    /// [`NotAStore`]) and parse the `DB.md` config when possible. This is the
    /// lenient validation-oriented open path: a damaged `DB.md` still marks the
    /// directory as a store so `dbmd validate` can report the config error as an
    /// issue. Normal CLI commands should use [`Store::open_strict`] instead.
    pub fn open(path: &Path) -> Result<Store, NotAStore> {
        if !Store::is_db_md_store(path) {
            return Err(NotAStore {
                path: path.to_path_buf(),
            });
        }
        let db_md = path.join("DB.md");
        // The marker exists; parse its config. A read or parse failure leaves
        // the store openable with default config rather than masquerading as
        // NOT_A_STORE — the marker is present, so this *is* a store; a damaged
        // DB.md is `dbmd validate`'s job to report, not `open`'s.
        let config = match std::fs::read_to_string(&db_md) {
            Ok(text) => parse_db_md(&text, &db_md).unwrap_or_default(),
            Err(_) => Config::default(),
        };
        Ok(Store {
            root: path.to_path_buf(),
            config,
        })
    }

    /// **SWEEP.** Recursively iterate every `.md` content file across
    /// `sources/` and `records/`, skipping hidden dirs and `log/`.
    /// Used only by `validate --all`, `index rebuild`, and `stats` — never on
    /// the interactive loop.
    pub fn walk(&self) -> Result<Vec<PathBuf>, StoreError> {
        // Only the three content layers — never root meta files (`DB.md`,
        // `index.md`, `log.md`) and never `log/`, which live at root and are
        // outside every layer dir.
        let mut out = Vec::new();
        for layer in Layer::all() {
            out.extend(self.walk_layer(layer)?);
        }
        out.sort();
        Ok(out)
    }

    /// **SWEEP.** Like [`Store::walk`] but scoped to a single layer.
    pub fn walk_layer(&self, layer: Layer) -> Result<Vec<PathBuf>, StoreError> {
        let layer_root = self.root.join(layer.dir_name());
        if !layer_root.is_dir() {
            return Ok(Vec::new());
        }
        self.walk_content_md(&layer_root)
    }

    /// Enumerate every `.md` file in a single type-folder, **recursing through
    /// its date-shards** (`sources/emails/**/*.md`). The unit the index builder
    /// and per-folder rebuild operate on. SWEEP-class (scoped to one folder).
    pub fn walk_type_folder(&self, type_folder: &Path) -> Result<Vec<PathBuf>, StoreError> {
        let abs = self.resolve_under_root(type_folder);
        if !abs.is_dir() {
            return Ok(Vec::new());
        }
        self.walk_content_md(&abs)
    }

    /// The ≤`n` most-recent files in a type-folder by frontmatter `updated`
    /// (descending), ties broken by store-relative path (ascending) — a total
    /// order, so write-through and rebuild never disagree on #500 vs #501.
    ///
    /// Reads `updated` across the folder's shards — a SWEEP cost absorbed into
    /// `index rebuild`. The write-through path never calls this. The
    /// cap-selection primitive for the 500-entry `index.md` browse view.
    pub fn recent_in_type_folder(
        &self,
        type_folder: &Path,
        n: usize,
    ) -> Result<Vec<PathBuf>, StoreError> {
        let files = self.walk_type_folder(type_folder)?;
        // (updated, rel-path) for each file. Files missing/unparseable
        // `updated` sort *after* dated ones (None last), then by path — so they
        // are deterministically the lowest-priority candidates for the cap, not
        // dropped silently. The total order (updated desc, path asc) is what
        // keeps write-through and rebuild agreeing on #500 vs #501.
        let mut keyed: Vec<(Option<DateTime<FixedOffset>>, PathBuf)> = files
            .into_iter()
            .map(|rel| {
                let updated = self.read_updated(&self.abs_path(&rel));
                (updated, rel)
            })
            .collect();
        keyed.sort_by(|a, b| {
            // `updated` descending: newest first. `None` is treated as the
            // oldest possible, so dated files always win a cap slot over
            // undated ones.
            let by_updated = b.0.cmp(&a.0);
            by_updated.then_with(|| a.1.cmp(&b.1))
        });
        keyed.truncate(n);
        Ok(keyed.into_iter().map(|(_, rel)| rel).collect())
    }

    /// The shard/flat predicate: true if the type date-shards, false if it
    /// stays flat. True for source types and event record types
    /// (`expense`/`invoice`/`meeting` + custom `order`/`ticket`/`transaction`),
    /// or when `DB.md ## Schemas` declares `shard: by-date`. False for
    /// dedup-bounded entity types (`contact`/`company`/`decision`) and
    /// conclusion records (`profile`/`concept`/`synthesis`).
    pub fn type_shards(&self, type_: &str) -> bool {
        // A `DB.md ## Schemas` `### <type>` block with a `shard:` directive is
        // authoritative — it is the v0.2 generic-model way to declare sharding,
        // so it overrides the built-in default below (in either direction).
        if let Some(shard) = self.config.schemas.get(type_).and_then(|s| s.shard) {
            return shard;
        }
        // Built-in default for the example types. Sharding is a property of the
        // *type*:
        //  - source types carry a primary date field and shard;
        //  - event record types track business volume and shard;
        //  - dedup-bounded entity types and curation-bounded conclusion
        //    records (`profile`/`concept`/`synthesis`) stay flat.
        // Any type can override this via a `shard:` directive (above).
        matches!(
            type_,
            // source types (documentary + testimonial)
            "email" | "transcript" | "pdf-source" | "note"
            // event record types (canonical)
            | "expense" | "invoice" | "meeting"
            // event record types (recognized custom, per the plan)
            | "order" | "ticket" | "transaction"
        )
    }

    /// Compute the canonical write path for a new file. For a sharding type
    /// (per [`Store::type_shards`]) insert `<YYYY>/<MM>/` from the type's
    /// primary date field (`email.date`, `expense.date`, … fallback `created`)
    /// under the type folder; flat types (entity + conclusion records) get no
    /// shard segment.
    /// Deterministic + stable: same input → same path, so a record never moves
    /// once written.
    pub fn shard_path_for(
        &self,
        type_: &str,
        frontmatter: &Frontmatter,
        name: &str,
    ) -> Result<PathBuf, StoreError> {
        self.shard_path_in(&default_type_folder(type_), type_, frontmatter, name)
    }

    /// Like [`Store::shard_path_for`], but compute the path under an explicit,
    /// caller-resolved type-folder rather than the canonical default. This lets a
    /// write surface honour an agent-supplied conforming sub-folder — e.g. a
    /// conclusion record filed under `records/profiles/`, `records/concepts/`, or
    /// `records/synthesis/` (a conclusion record may be filed under ANY
    /// `records/<folder>/`, not only its canonical one) — while still applying
    /// date-sharding for sharding types. The folder must be a conforming
    /// `<layer>/<type-folder>` (2
    /// components, recognized layer); the caller is responsible for that (see the
    /// CLI's `resolve_write_path`), so it is taken as given here.
    ///
    /// Sharding is still a property of the *type*: a sharding type gets the
    /// `<YYYY>/<MM>` segment under `folder`; a flat type lands directly in it.
    pub fn shard_path_in(
        &self,
        folder: &Path,
        type_: &str,
        frontmatter: &Frontmatter,
        name: &str,
    ) -> Result<PathBuf, StoreError> {
        let folder = folder.to_path_buf();
        let filename = ensure_md_extension(name);

        if !self.type_shards(type_) {
            // Flat type (entity records, conclusion records, decisions): no
            // shard segment.
            return Ok(folder.join(filename));
        }

        // Sharding type: derive <YYYY>/<MM> from the primary date field, with
        // `created` as the universal fallback. Reading the public `Frontmatter`
        // fields directly (typed `created`/`updated` + raw `extra`) avoids the
        // not-yet-implemented `Frontmatter::get`/`parse` and keeps this pure.
        let (year, month) = self
            .primary_shard_segment(type_, frontmatter)
            .ok_or_else(|| StoreError::NoShardDate {
                file: folder.join(&filename),
            })?;

        Ok(folder.join(year).join(month).join(filename))
    }

    /// Find files with an incoming wiki-link to `target` via a **single
    /// presence-only content scan** for an edge to `target` across all layers,
    /// using the shared fence-aware/whitespace-trimmed/case-folded edge notion
    /// ([`extract_edge_targets`]). Loop-fast; no whole-graph build. Returns
    /// store-relative paths.
    pub fn find_links_to(&self, target: &Path) -> Result<Vec<PathBuf>, StoreError> {
        // A single target is just the degenerate batch case — one key, one store
        // scan. Routing through `find_links_to_any` keeps the
        // pattern construction and the scan loop in exactly one place. The
        // batch API takes `&[PathBuf]`, so the one-element slice is owned (a
        // single alloc on this single-target convenience path; the batch path
        // validate.rs rides is untouched).
        self.find_links_to_any(&[target.to_path_buf()])
    }

    /// Find every file with an incoming wiki-link to **any** of `targets`, in a
    /// **single content pass** over the store (one `.md` walk, one presence-only
    /// edge scan per file). This is the batch incoming-linker finder the
    /// working-set [`crate::validate::validate_working_set`] sits on: it must find
    /// the linkers for the *whole* changed set without paying a full store read
    /// per changed object. Cost is therefore one store scan (O(store)), NOT
    /// `targets.len() × store` — calling [`find_links_to`](Self::find_links_to)
    /// in a loop would reread every `.md` once per target and is the exact
    /// `O(changed × store)` blow-up this method exists to prevent. Returns
    /// store-relative paths (deduped, sorted).
    ///
    /// **One edge notion with `forwardlinks`/`rename`/`validate`.** A file links
    /// to a target iff [`extract_edge_targets`] (fence-aware, whitespace-trimmed)
    /// of its content yields a target whose [`link_edge_key`] equals the target's
    /// — the *same* definition the forward view and the rename rewriter use. The
    /// previous implementation used a literal-adjacency ripgrep regex that (a)
    /// matched `[[...]]` text inside fenced code examples (which validate treats
    /// as non-edges), (b) missed inner-whitespace padding (`[[ x ]]`), and (c)
    /// compared case-sensitively even where the filesystem resolves links
    /// case-insensitively — so backlinks/links/rename silently disagreed with
    /// forwardlinks and validate. Reading content and routing through the shared
    /// extractor removes all three divergences.
    ///
    /// Why content scan and not the sidecar `links` field: the sidecar projects
    /// only the frontmatter `links:` array, so it misses edges written in the
    /// body or in typed fields (`company: [[…]]`). Finding an incoming link to an
    /// arbitrary path therefore requires reading file content.
    pub fn find_links_to_any(&self, targets: &[PathBuf]) -> Result<Vec<PathBuf>, StoreError> {
        // Build the set of comparison keys for the requested targets, in the
        // canonical (case-folded where the filesystem is case-insensitive) form
        // the edge extractor emits. An empty key (a target that renders to no
        // link text, e.g. `""` or `"./"`) contributes nothing — and crucially the
        // empty set short-circuits below so we never report every file.
        let want: std::collections::HashSet<String> = targets
            .iter()
            .filter_map(|t| {
                let canonical = canonical_link_target(&t.to_string_lossy());
                if canonical.is_empty() {
                    None
                } else {
                    Some(link_edge_key(&canonical))
                }
            })
            .collect();
        if want.is_empty() {
            return Ok(Vec::new());
        }

        let mut hits = std::collections::BTreeSet::new();
        // Scan every `.md` file in the store (skip hidden + `log/`), including
        // `index.md` catalogs — an incoming reference is wherever the link text
        // lives; the caller decides relevance. ONE walk for the whole target set;
        // per file we stop at the first matching edge (presence is all we need),
        // so a file that links to several targets is read once, not once per
        // target.
        for rel in self.walk_all_md()? {
            let abs = self.abs_path(&rel);
            // Read lossily: a `.md` verbatim-ingested into `sources/` can carry a
            // stray non-UTF-8 byte (a mis-decoded Latin-1 import). Decoding
            // lossily substitutes replacement characters instead of erroring, so
            // one bad byte on a link-bearing line no longer aborts the whole
            // store scan (the historical `UTF8`-sink failure). The link syntax is
            // ASCII, so a replacement char elsewhere on the line never hides a
            // `[[...]]`. A read error (not a decode error) is genuine I/O trouble
            // and propagates.
            let bytes = match std::fs::read(&abs) {
                Ok(b) => b,
                Err(e) => {
                    return Err(StoreError::Search {
                        root: self.root.clone(),
                        message: format!("read failed in {}: {e}", abs.display()),
                    })
                }
            };
            let text = String::from_utf8_lossy(&bytes);
            for target in extract_edge_targets(&text) {
                if want.contains(&link_edge_key(&target)) {
                    hits.insert(rel);
                    break;
                }
            }
        }
        Ok(hits.into_iter().collect())
    }

    /// Candidate set for a `type` query: read every type-folder `index.jsonl`
    /// sidecar in the type's single layer and return the records of that
    /// `type`. Complete and cold-cache-proof — NOT a walk-and-parse or a
    /// frontmatter ripgrep scan, and **never a store-wide read**.
    ///
    /// The read is bounded to the type's one layer subtree
    /// (O(entities-in-layer)): a type lives in exactly one layer, and
    /// `default_type_folder` always encodes it (recognized → its SPEC layer;
    /// unrecognized → `records/`), so the walk never fans out across every
    /// sidecar in the store and stays inside the interactive loop's
    /// O(entities) contract.
    ///
    /// The whole-layer read — rather than reading only the type's canonical
    /// folder sidecar when it happens to exist — is what makes the result
    /// *complete*. A single `type` can legitimately be filed across several
    /// folders within its layer: a conclusion `profile` filed under any
    /// `records/<folder>/`, or a `contact` filed in `records/clients/` alongside
    /// the canonical `records/contacts/`. The previous code read only the
    /// canonical-guess sidecar whenever it was a file, which silently dropped
    /// those non-canonical records the moment the canonical sidecar existed —
    /// returning an incomplete set, and a *different* set as the store grew
    /// (the omission flipped on once one canonical record was added). That
    /// broke the dedup/enumeration premise this primitive backs and disagreed
    /// with `find_by_where_in`, which already walks the whole layer. Filtering
    /// the layer read by `type` keeps the result complete regardless of how the
    /// type's records are foldered.
    pub fn find_by_type(&self, type_: &str) -> Result<Vec<IndexRecord>, StoreError> {
        let canonical_folder = default_type_folder(type_);
        let records = self.read_all_type_indexes_in(layer_of_folder(&canonical_folder))?;
        Ok(records.into_iter().filter(|r| r.type_ == type_).collect())
    }

    /// Candidate set for a `key=value` frontmatter query, **store-wide**: read
    /// every type-folder `index.jsonl` sidecar and filter their records. The
    /// unscoped pre-write dedup primitive; prefer [`Store::find_by_where_in`]
    /// with a layer scope to stay O(entities-in-layer) on the interactive loop.
    pub fn find_by_where(&self, key: &str, value: &str) -> Result<Vec<IndexRecord>, StoreError> {
        self.find_by_where_in(key, value, None)
    }

    /// Candidate set for a `key=value` frontmatter query, **scoped to one
    /// layer** when `layer` is `Some`: the sidecar walk is confined to that
    /// layer's subtree (`<root>/<layer>/`), so the I/O is O(entities-in-layer),
    /// not O(store records). `None` keeps the store-wide read.
    ///
    /// This is what makes `--in <layer>` an I/O scope, not just a result
    /// filter: a `--where`-only query (no `--type`) used to read every sidecar
    /// in the store and narrow by layer in memory, breaking the O(entities)
    /// contract the interactive loop depends on. With a layer in hand we walk
    /// only that layer's sidecars.
    pub fn find_by_where_in(
        &self,
        key: &str,
        value: &str,
        layer: Option<Layer>,
    ) -> Result<Vec<IndexRecord>, StoreError> {
        // A `key=value` query can target any frontmatter field across any type,
        // so within the chosen subtree we still read every type-folder sidecar
        // and filter. The layer (when given) bounds *which* subtree, turning a
        // whole-store walk into a single-layer walk.
        let records = self.read_all_type_indexes_in(layer)?;
        Ok(records
            .into_iter()
            .filter(|r| record_matches_field(r, key, value))
            .collect())
    }

    /// Every record across the type-folder `index.jsonl` sidecars, scoped to one
    /// layer when `layer` is `Some` (the walk is confined to `<root>/<layer>/`)
    /// else store-wide. Sequential, complete sidecar reads — never a
    /// walk-and-parse of the content tree.
    ///
    /// This is the unfiltered sidecar-enumeration primitive the relationship
    /// loop sits on: [`crate::graph::backlinks_filtered`] uses it to bound its
    /// candidate set to the relevant layer (or the whole store) without opening
    /// the content tree, then confirms each candidate's edge by parsing the file.
    pub fn sidecar_records(&self, layer: Option<Layer>) -> Result<Vec<IndexRecord>, StoreError> {
        self.read_all_type_indexes_in(layer)
    }

    /// Parse a type-folder's `index.jsonl` into [`IndexRecord`]s, applying
    /// last-write-wins by `path` over any un-compacted lines. The sidecar-read
    /// primitive every structured query sits on.
    pub fn read_type_index(&self, index_jsonl: &Path) -> Result<Vec<IndexRecord>, StoreError> {
        let text = std::fs::read_to_string(index_jsonl).map_err(|e| StoreError::BadTypeIndex {
            path: index_jsonl.to_path_buf(),
            message: e.to_string(),
        })?;

        // Last-write-wins by `path` over un-compacted lines: a later line for
        // the same path supersedes an earlier one (the jsonl is append-mostly
        // and only compacted on rebuild). Blank lines are skipped; a non-blank
        // line that is not a valid IndexRecord is a hard parse error.
        let mut by_path: BTreeMap<PathBuf, IndexRecord> = BTreeMap::new();
        for (i, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let record: IndexRecord =
                serde_json::from_str(trimmed).map_err(|e| StoreError::BadTypeIndex {
                    path: index_jsonl.to_path_buf(),
                    message: format!("line {}: {e}", i + 1),
                })?;
            by_path.insert(record.path.clone(), record);
        }
        // BTreeMap keyed by path → records emerge sorted by path ascending,
        // a deterministic order independent of line order in the file.
        Ok(by_path.into_values().collect())
    }

    /// Resolve a store-relative path to its absolute on-disk path under
    /// [`root`](Store::root).
    pub fn abs_path(&self, store_relative: &Path) -> PathBuf {
        // `Path::join` returns `store_relative` unchanged if it is already
        // absolute, so passing an absolute path through is a no-op.
        self.root.join(store_relative)
    }

    /// Convert an absolute path under the store into its store-relative form.
    pub fn rel_path(&self, abs: &Path) -> Option<PathBuf> {
        abs.strip_prefix(&self.root).ok().map(|p| p.to_path_buf())
    }

    // ── Private helpers ─────────────────────────────────────────────────────

    /// Resolve a caller-supplied folder path (store-relative or absolute) to an
    /// absolute path under the store root.
    fn resolve_under_root(&self, folder: &Path) -> PathBuf {
        if folder.is_absolute() {
            folder.to_path_buf()
        } else {
            self.root.join(folder)
        }
    }

    /// Walk a subtree for content `.md` files (skip hidden dirs, skip `index.md`
    /// / `DB.md` / `log.md`), returning store-relative paths. Used by the layer
    /// and type-folder walks.
    fn walk_content_md(&self, root: &Path) -> Result<Vec<PathBuf>, StoreError> {
        let mut out = Vec::new();
        for entry in self.md_walker(root).build() {
            let entry = entry.map_err(|e| StoreError::Search {
                root: root.to_path_buf(),
                message: e.to_string(),
            })?;
            if !is_file_entry(&entry) {
                continue;
            }
            let path = entry.path();
            if !has_md_extension(path) {
                continue;
            }
            if is_non_content_basename(path) {
                continue;
            }
            if let Some(rel) = self.rel_path(path) {
                out.push(rel);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Walk the whole store for **every** `.md` file (including `index.md`),
    /// skipping hidden dirs and the `log/` archive tree. Used by the backlink
    /// scan, where the literal link text can live in any markdown file.
    fn walk_all_md(&self) -> Result<Vec<PathBuf>, StoreError> {
        let mut out = Vec::new();
        for entry in self.md_walker(&self.root).build() {
            let entry = entry.map_err(|e| StoreError::Search {
                root: self.root.clone(),
                message: e.to_string(),
            })?;
            if !is_file_entry(&entry) {
                continue;
            }
            let path = entry.path();
            if !has_md_extension(path) {
                continue;
            }
            if self.is_in_log_dir(path) {
                continue;
            }
            if let Some(rel) = self.rel_path(path) {
                out.push(rel);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Read and merge every type-folder `index.jsonl` sidecar under `layer`
    /// when given, else the whole store (skip hidden + `log/`). Each sidecar is
    /// read with last-write-wins by path; across sidecars, paths are disjoint by
    /// construction (one sidecar per folder), so a plain concatenation preserves
    /// completeness. A layer scope confines the walk to `<root>/<layer>/`, which
    /// is what keeps `find_by_where_in` O(entities-in-layer).
    fn read_all_type_indexes_in(
        &self,
        layer: Option<Layer>,
    ) -> Result<Vec<IndexRecord>, StoreError> {
        let mut out = Vec::new();
        for sidecar in self.find_type_index_files_in(layer)? {
            out.extend(self.read_type_index(&self.abs_path(&sidecar))?);
        }
        Ok(out)
    }

    /// Locate every `index.jsonl` sidecar under `layer` (when given) else the
    /// whole store (skip hidden + `log/`), returning store-relative paths. A
    /// scoped read walks `<root>/<layer>/`; the store-wide read enumerates the
    /// two canonical layer subtrees (`sources/`, `records/`) — the
    /// same store model [`Store::walk`] uses — rather than walking from
    /// `self.root`. Walking from root would descend into non-layer top-level
    /// dirs (`EXPECTED/` test goldens, an `archive/` of frozen index copies,
    /// any sibling folder holding store-relative `path`s), pulling their
    /// sidecars in and returning every record twice. A non-existent layer
    /// subtree yields no sidecars rather than walking a missing path.
    fn find_type_index_files_in(&self, layer: Option<Layer>) -> Result<Vec<PathBuf>, StoreError> {
        // Store-wide read: union the per-layer scoped reads so only the three
        // content layers are walked (never root meta files or non-layer dirs),
        // matching `Store::walk`. The per-layer paths are disjoint by folder, so
        // a plain concatenation preserves completeness.
        let Some(layer) = layer else {
            let mut out = Vec::new();
            for l in Layer::all() {
                out.extend(self.find_type_index_files_in(Some(l))?);
            }
            out.sort();
            return Ok(out);
        };
        let walk_root = self.root.join(layer.dir_name());
        // A scoped walk over a layer folder that does not exist yet must be an
        // empty result, mirroring `walk_layer`'s missing-dir guard — not a walk
        // error from `ignore` over a nonexistent path.
        if !walk_root.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut builder = WalkBuilder::new(&walk_root);
        builder
            .standard_filters(false)
            .hidden(true)
            .follow_links(true);
        for entry in builder.build() {
            let entry = entry.map_err(|e| StoreError::Search {
                root: walk_root.clone(),
                message: e.to_string(),
            })?;
            if !is_file_entry(&entry) {
                continue;
            }
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) != Some(TYPE_INDEX_FILE) {
                continue;
            }
            if self.is_in_log_dir(path) {
                continue;
            }
            if let Some(rel) = self.rel_path(path) {
                out.push(rel);
            }
        }
        out.sort();
        Ok(out)
    }

    /// A `WalkBuilder` configured for db.md SWEEPs: gitignore/global-ignore are
    /// OFF (a SWEEP must see every file even if the store is a git repo with a
    /// `.gitignore`), but hidden files/dirs are skipped. Symlinks are
    /// **followed** (`follow_links(true)`) so a symlinked `.md` content file or
    /// a symlinked type folder (e.g. `records/companies -> /other/disk/...`) is
    /// walked like any other content rather than silently vanishing; a symlinked
    /// layer dir was already traversed (the walk root is followed), so following
    /// symlinks one level deeper just removes that inconsistency.
    fn md_walker(&self, root: &Path) -> WalkBuilder {
        let mut builder = WalkBuilder::new(root);
        builder
            .standard_filters(false)
            .hidden(true)
            .follow_links(true);
        builder
    }

    /// True if an absolute path lives under the store's root-level `log/`
    /// rotation-archive directory.
    fn is_in_log_dir(&self, abs: &Path) -> bool {
        match self.rel_path(abs) {
            Some(rel) => rel.components().next().map(|c| c.as_os_str()) == Some("log".as_ref()),
            None => false,
        }
    }

    /// Read a file's frontmatter `updated` field as an RFC3339 timestamp,
    /// returning `None` when absent/unparseable. A self-contained reader (does
    /// not depend on the not-yet-implemented `parser::read_file`); parses the
    /// leading `---`-fenced YAML block with the same engine the parser uses.
    fn read_updated(&self, abs: &Path) -> Option<DateTime<FixedOffset>> {
        let text = std::fs::read_to_string(abs).ok()?;
        let yaml = frontmatter_block(&text)?;
        let value: serde_norway::Value = serde_norway::from_str(yaml).ok()?;
        let raw = value.get("updated")?;
        value_to_datetime(raw)
    }

    /// The `<YYYY>/<MM>` shard segment for a sharding type, from its primary
    /// date field with a `created` fallback. Reads the public `Frontmatter`
    /// fields directly. `None` when no usable date is present.
    fn primary_shard_segment(&self, type_: &str, fm: &Frontmatter) -> Option<(String, String)> {
        // Try the type's primary date field first.
        if let Some(field) = primary_date_field(type_) {
            if let Some(v) = fm.extra.get(field) {
                if let Some(seg) = value_to_year_month(v) {
                    return Some(seg);
                }
            }
        }
        // Universal fallback: the typed `created` timestamp.
        fm.created
            .map(|dt| (format!("{:04}", dt.year()), format!("{:02}", dt.month())))
    }
}

// ── Path containment (security) ─────────────────────────────────────────────

/// Canonicalize `candidate` (resolving symlinks; for a not-yet-existing leaf,
/// canonicalize its existing parent chain and re-append the leaf) and return it
/// only if it resolves inside `store_root`; otherwise `Err`.
///
/// This is the single within-store containment gate. A wiki-link target, a
/// rename destination, or any other caller-influenced path must pass through
/// here before it is read or traversed, so a `..`-laden or symlink-escaping
/// target can never turn a store operation into a read of an arbitrary file
/// outside the store. `store_root` itself is canonicalized first so the
/// `starts_with` comparison is symlink-stable on both sides (e.g. macOS's
/// `/tmp` → `/private/tmp`).
pub fn ensure_path_within_store(store_root: &Path, candidate: &Path) -> std::io::Result<PathBuf> {
    // The `..` rejection below must apply only to the *caller-influenced* tail of
    // the candidate — never to a `..` the trusted `store_root` itself carries.
    // Callers build the candidate as `store_root.join(rel)`, so a user-supplied
    // `--dir ../../some/store` legitimately seeds every candidate with leading
    // `..` components that belong to the root, not to the sidecar/link target.
    // Strip the trusted `store_root` prefix lexically and scrutinize only what
    // remains; the root's own `..` is resolved safely by `canonicalize()` just
    // below. A candidate that does NOT begin with `store_root` (an absolute
    // out-of-store path, a CWD-relative target) keeps the whole path under
    // scrutiny — there is no trusted prefix to exempt.
    let scrutinized = candidate.strip_prefix(store_root).unwrap_or(candidate);

    // Reject any `..` component in the scrutinized tail. A `ParentDir` can never
    // be resolved safely by lexical normalization: once a symlink sits earlier in
    // the path, `foo/../bar` does NOT equal `bar`, and canonicalizing the existing
    // prefix (below) would silently collapse `records/contacts/../../outside` down
    // to a path that *appears* inside the root, masking the traversal. There is no
    // legitimate in-store caller that needs `..` in the tail — wiki-link targets,
    // rename destinations, and graph reads are all forward (`Normal`-only) paths —
    // so a tail `..` is always either an escape attempt or a malformed target.
    if scrutinized
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "path {} contains a `..` component beyond the store root {} and cannot be contained",
                candidate.display(),
                store_root.display()
            ),
        ));
    }

    // Canonicalize the root so both sides of the containment check are in the
    // same (fully-resolved) namespace. This also resolves any `..` the root
    // itself carries (the user-supplied `--dir`), which the tail-only check above
    // deliberately left in place.
    let root = store_root.canonicalize()?;

    // Resolve the candidate as far as it exists on disk. `canonicalize` fails on
    // a not-yet-existing leaf, so peel trailing components until the remaining
    // prefix exists, canonicalize that, then re-append the peeled tail. This
    // resolves any symlink in the existing parent chain (an escape vector) while
    // still working for a target that does not exist yet (a rename destination).
    let mut existing = candidate.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let resolved_prefix = loop {
        match existing.canonicalize() {
            Ok(p) => break p,
            Err(_) => {
                // No existing prefix left to canonicalize → resolve relative to
                // the canonical root (the candidate is somewhere under, or
                // escaping from, the store) and let the containment check below
                // decide. Pop one component and keep peeling.
                match existing.file_name() {
                    Some(name) => {
                        tail.push(name.to_os_string());
                        if !existing.pop() {
                            // Ran out of components without finding an existing
                            // prefix: anchor the un-resolvable remainder at the
                            // canonical root so a relative candidate is judged
                            // against the store, not the process CWD.
                            break root.clone();
                        }
                    }
                    None => {
                        // A root/prefix component with no file name and no
                        // on-disk existence: anchor at the canonical root.
                        break root.clone();
                    }
                }
            }
        }
    };

    // Reassemble: canonical existing prefix + the peeled (still-virtual) tail,
    // in original order (the peel pushed them reversed).
    let mut resolved = resolved_prefix;
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }

    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "path {} resolves outside the store root {}",
                candidate.display(),
                store_root.display()
            ),
        ))
    }
}

// ── The shared wiki-link edge notion (graph / stats / validate / rename) ─────
//
// One definition of "what `[[...]]` text is a real edge" that every relationship
// op keys on, so `forwardlinks`, `backlinks`, `links`, `stats`, and `rename`
// never disagree with each other (or with `validate`'s body extractor):
//
//   1. **Fence-aware.** A `[[...]]` inside a ``` / ~~~ fenced code block is a
//      documentation example, not an edge — exactly `validate`'s rule. Counting
//      it as an edge over-reports backlinks, falsely un-orphans the page, and
//      (worst) lets `rename` rewrite verbatim example text.
//   2. **Whitespace-trimmed.** `[[ records/contacts/sarah ]]` is the same edge
//      as `[[records/contacts/sarah]]`. The inner padding is cosmetic; both the
//      forward and the backward view must resolve it identically.
//   3. **Case-folded to the filesystem.** Link *resolution* is `is_file()`,
//      which is case-insensitive on macOS/Windows. So on a case-insensitive
//      filesystem `[[records/contacts/Sarah-Chen]]` and the on-disk
//      `sarah-chen.md` are the SAME edge; the comparison key must case-fold to
//      match, or backlinks/rename silently miss the link while validate (which
//      resolves via the filesystem) considers it fine.

/// Canonicalize a raw `[[...]]` inner target into the wiki-link key: forward
/// slashes, no leading `./` or `/`, no trailing `.md`, inner whitespace trimmed.
/// The single key forward and backward edges are compared on. Pairs with
/// [`link_edge_key`] for the case-fold step.
pub fn canonical_link_target(raw: &str) -> String {
    let mut s = raw.trim().replace('\\', "/");
    while let Some(rest) = s.strip_prefix("./") {
        s = rest.to_string();
    }
    let s = s.trim_start_matches('/');
    let s = s.strip_suffix(".md").unwrap_or(s);
    s.trim().to_string()
}

/// The comparison key for a canonical link target. Two normalizations, applied
/// in order, so the string-keyed edge comparison agrees with how the filesystem
/// resolves the same link:
///
///   1. **Unicode NFC, always.** macOS/APFS folds NFC and NFD forms of a name to
///      the same file, so a file `records/contacts/josé.md` written NFC
///      (`é` = U+00E9) and a link `[[records/contacts/josé]]` written NFD
///      (`e` + U+0301) name the *same* file on disk — yet their raw UTF-8 bytes
///      differ. Without normalization the graph keys them as two different
///      targets, so `backlinks`/`forwardlinks` miss the edge and `orphans` flags
///      a linked-to file as an orphan, while `validate` (which resolves through
///      the filesystem) sees the link as live: the surfaces silently disagree.
///      Normalizing BOTH sides to NFC here makes the comparison
///      normalization-insensitive, matching the filesystem. This lives in the
///      comparison key — not in [`canonical_link_target`] — so the canonical
///      form stays byte/normalization-preserving (rename REWRITE output is never
///      silently re-normalized); both the link target and the file path pass
///      through this function, so NFC here is sufficient to unify them.
///   2. **ASCII case-fold on a case-insensitive filesystem.** Identity on a
///      case-sensitive FS, ASCII-lowercased on macOS/Windows, so the comparison
///      also agrees with the filesystem's case-folding `is_file()` resolution.
///
/// Callers compare `link_edge_key(a) == link_edge_key(b)`.
pub fn link_edge_key(canonical_target: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    // NFC first — always, on every platform: the graph must agree across hosts,
    // and the comparison must be normalization-insensitive regardless of which
    // host's filesystem folded the on-disk name.
    let nfc: String = canonical_target.nfc().collect();
    if fs_is_case_insensitive() {
        nfc.to_ascii_lowercase()
    } else {
        nfc
    }
}

/// Extract every wiki-link edge target from a markdown body, fence-aware and
/// whitespace-trimmed, in document order (duplicates kept — callers dedup).
/// Returns canonical targets (see [`canonical_link_target`]); the case-fold for
/// comparison is applied separately via [`link_edge_key`] so the canonical form
/// (used for rewrites/output) stays case-preserving.
///
/// Scans line-by-line tracking the fence state inline (no whole-body
/// allocation), exactly mirroring validate's `extract_wiki_links`: the fence
/// state is a `(fence char, run length)` tracked via [`fence_opens`] /
/// [`fence_closes`] — NOT a bool toggled on any ``` / `~~~` line. The naive
/// toggle inverts mid-block when a `~~~` block legally contains a ```` ``` ````
/// line (the standard way to document a backtick fence), or when a `>3`-space-
/// indented ``` is mistaken for a fence — both of which would let a fenced
/// example `[[…]]` leak out as a live edge (a false dependent for
/// backlinks/rename). Fenced lines never yield edges. Within a line, the text
/// before the first `|` is the target; a target whose trimmed form starts with
/// `[` is the rejected triple-bracket flow-form list mis-encoding
/// (`[[[a]], [[b]]]`), not a real link — skipped, matching validate.
///
/// Accepts a whole file's text *or* a body-only fragment. A leading `---`
/// frontmatter block is YAML, not markdown: it has no code fences, and a
/// `[[…]]` in any frontmatter field is a real edge. The frontmatter is therefore
/// scanned WITHOUT fence tracking, and the body is scanned with a FRESH fence
/// state — so a stray ``` / `~~~` inside a frontmatter value can never open a
/// fence that swallows the body's real wiki-links. (Callers `search_by_link`,
/// `forwardlinks`, and `dbmd links` all pass full file text; without this
/// boundary reset a fenced frontmatter value silently dropped every subsequent
/// body edge — under-reporting backlinks/forwardlinks/`links`.) A fragment with
/// no leading frontmatter takes the body path unchanged.
pub fn extract_edge_targets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Split off a leading `---`…`---` frontmatter block (raw — no YAML parse, so
    // a malformed file is still fully scanned). Frontmatter links are edges but
    // must not participate in code-fence state.
    let body = match split_frontmatter_raw(text) {
        Some((frontmatter, body)) => {
            for line in frontmatter.lines() {
                push_edges_in_line(line, &mut out);
            }
            body
        }
        None => text,
    };
    let mut fence: Option<(u8, usize)> = None;
    for line in body.lines() {
        let content = line.trim_end_matches('\r');
        if let Some(f) = fence {
            if fence_closes(content, f) {
                fence = None;
            }
            continue;
        }
        if let Some(opened) = fence_opens(content) {
            fence = Some(opened);
            continue;
        }
        push_edges_in_line(line, &mut out);
    }
    out
}

/// Push every `[[target]]` on one line into `out`, alias-stripped (`[[a|b]]` →
/// `a`), trimmed, and canonicalized. The triple-bracket flow-form mis-encoding
/// (`[[[a]], …]`) is skipped, matching validate. Shared by both the frontmatter
/// and body scans in [`extract_edge_targets`] so they honor one link grammar.
fn push_edges_in_line(line: &str, out: &mut Vec<String>) {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if let Some(close) = line[i + 2..].find("]]") {
                let inner = &line[i + 2..i + 2 + close];
                let raw_target = inner.split('|').next().unwrap_or(inner).trim();
                if !raw_target.is_empty() && !raw_target.starts_with('[') {
                    let canonical = canonical_link_target(raw_target);
                    if !canonical.is_empty() {
                        out.push(canonical);
                    }
                }
                i = i + 2 + close + 2;
                continue;
            }
        }
        i += 1;
    }
}

/// If `line` opens a fenced code block, return `(fence byte, run length)`. The
/// single fence-open rule shared by [`extract_edge_targets`] and graph's
/// `rewrite_links_to`, mirroring validate's `fence_opens` and the parser's
/// `opening_fence` so every link op tracks fences identically: a fence is
/// ```` ``` ```` or `~~~` (run ≥ 3) at ≤ 3 spaces of indent, and a backtick
/// fence's info string may not itself contain a backtick.
pub fn fence_opens(line: &str) -> Option<(u8, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let byte = rest.bytes().next()?;
    if byte != b'`' && byte != b'~' {
        return None;
    }
    let run = rest.len() - rest.trim_start_matches(byte as char).len();
    if run < 3 {
        return None;
    }
    // A backtick fence's info string may not itself contain a backtick.
    if byte == b'`' && rest[run..].contains('`') {
        return None;
    }
    Some((byte, run))
}

/// True if `line` closes the currently open `fence`: same char, run at least as
/// long, nothing but trailing whitespace after. Mirrors validate's
/// `fence_closes` / the parser's `is_closing_fence`, so an inner fence of the
/// *other* character (a ```` ``` ```` line inside a `~~~` block) does NOT close
/// the outer fence.
pub fn fence_closes(line: &str, fence: (u8, usize)) -> bool {
    let (byte, open_len) = fence;
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let run = rest.len() - rest.trim_start_matches(byte as char).len();
    if run < open_len {
        return false;
    }
    rest[run..].trim().is_empty()
}

/// True when the host filesystem resolves paths case-insensitively (macOS/
/// Windows default). Probed once per process against the OS temp dir by creating
/// a lowercase marker and stat-ing its uppercase spelling. A probe failure
/// conservatively reports `false` (case-sensitive) — the historical behavior —
/// so a transient temp-dir issue never silently widens matching.
fn fs_is_case_insensitive() -> bool {
    use std::sync::OnceLock;
    static CASE_INSENSITIVE: OnceLock<bool> = OnceLock::new();
    *CASE_INSENSITIVE.get_or_init(|| {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let lower = dir.join(format!(".dbmd-case-probe-{pid}-{nanos}"));
        let upper = dir.join(format!(".DBMD-CASE-PROBE-{pid}-{nanos}"));
        // Create the lowercase marker; if its uppercase spelling then resolves to
        // a file, the filesystem folded the case → case-insensitive.
        let result = match std::fs::File::create(&lower) {
            Ok(_) => upper.is_file(),
            Err(_) => false,
        };
        let _ = std::fs::remove_file(&lower);
        result
    })
}

// ── Free helpers (no `self`) ────────────────────────────────────────────────

/// True if a walk entry is a regular file, **following symlinks** so a
/// symlinked `.md` content file (or a file inside a symlinked type folder) is
/// counted like any other content file.
///
/// The store walks enable `follow_links(true)`, so a symlink entry's
/// `file_type()` still reports `is_symlink()` (the `ignore` walker does not
/// rewrite the entry's own type), not the followed target's type. Treat a
/// symlink whose target is a regular file as a file: `stat` (follow) the path
/// and check. A broken symlink (no target) is not a file.
fn is_file_entry(entry: &ignore::DirEntry) -> bool {
    match entry.file_type() {
        Some(ft) if ft.is_file() => true,
        Some(ft) if ft.is_symlink() => std::fs::metadata(entry.path())
            .map(|m| m.is_file())
            .unwrap_or(false),
        // A `None` file type (the walk root itself) or a non-file/non-symlink
        // entry is not a content file.
        _ => false,
    }
}

/// True if the path ends in a `.md` extension (case-sensitive — db.md files are
/// lowercase `.md`).
fn has_md_extension(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
}

/// True if the basename is a non-content meta file (`DB.md`, `index.md`,
/// `log.md`) that the content walks must skip.
fn is_non_content_basename(path: &Path) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => NON_CONTENT_BASENAMES.contains(&name),
        None => false,
    }
}

/// Append `.md` to a bare name; leave an existing `.md` untouched.
fn ensure_md_extension(name: &str) -> String {
    if name.ends_with(".md") {
        name.to_string()
    } else {
        format!("{name}.md")
    }
}

/// The canonical default folder for a recognized type, per the SPEC type table
/// (`email → sources/emails`, `expense → records/expenses`, …). Unrecognized
/// types fall back to `records/<type>` (the bare type name, no pluralization
/// guess) — see the store findings on the docstring's looser `<type>` phrasing.
fn default_type_folder(type_: &str) -> PathBuf {
    let path = match type_ {
        // sources — documentary
        "email" => "sources/emails",
        "transcript" => "sources/transcripts",
        "pdf-source" => "sources/docs",
        // sources — testimonial (a human told the agent X)
        "note" => "sources/notes",
        // records — entities
        "contact" => "records/contacts",
        "company" => "records/companies",
        // records — events
        "expense" => "records/expenses",
        "meeting" => "records/meetings",
        "decision" => "records/decisions",
        "invoice" => "records/invoices",
        // unrecognized: bare type name under records/ (conclusions and any
        // custom type land here, e.g. `concept` → `records/concept`).
        other => return PathBuf::from("records").join(other),
    };
    PathBuf::from(path)
}

/// The canonical [`Layer`] a `type_` belongs to, derived from its default
/// type-folder (`email` → `Sources`, `contact` → `Records`, a conclusion
/// `profile` → `Records`, unrecognized → `Records`). The write path uses this to decide whether
/// an agent-supplied folder is in the *right* layer for the type before honouring
/// its sub-folder choice.
pub fn layer_for_type(type_: &str) -> Layer {
    layer_of_folder(&default_type_folder(type_)).unwrap_or(Layer::Records)
}

/// The [`Layer`] a type-folder path lives in, read from its first component
/// (`sources/` → `Sources`, `records/` → `Records`). Used to
/// bound [`Store::find_by_type`]'s whole-layer sidecar read to a single layer
/// subtree. Returns `None` for a path with no recognized layer prefix; every
/// value [`default_type_folder`] produces has one, so in practice this is
/// always `Some` on the call path — `None` degrades to a store-wide read.
fn layer_of_folder(folder: &Path) -> Option<Layer> {
    let first = folder.components().next()?.as_os_str().to_str()?;
    Layer::from_dir_name(first)
}

/// True if a store-relative path is a db.md **content** file: rooted in a real
/// layer (`sources/` or `records/` as its FIRST component), with a `.md`
/// extension, and not an `index.md` sidecar. This is the SPEC's "content files =
/// everything under `sources/` and `records/` only" predicate (SPEC § content
/// files), keyed on the *first* component so a non-layer top-level dir is never
/// content even if a deeper component happens to be named `records`/`sources`
/// (e.g. `EXPECTED/records/x.md`, `archive/sources/y.md`).
///
/// It mirrors the graph engine's content filter so the surfaces that READ the
/// store (`graph backlinks`) and the surface that MUTATES it (`rename`) agree on
/// exactly which files are content. `rename` uses it to restrict its
/// link-rewrite set: a store-root file, a non-layer dir (`scratch/`,
/// `EXPECTED/`, `archive/`), or an `index.md` is NEVER rewritten — `rename` does
/// not own those bytes. The broad store scan ([`Store::find_links_to_any`],
/// shared with the read-only working-set validate) is left untouched; the filter
/// is applied at the point of mutation.
pub fn is_content_path(rel: &Path) -> bool {
    if layer_of_folder(rel).is_none() {
        return false;
    }
    if rel.extension().and_then(|e| e.to_str()) != Some("md") {
        return false;
    }
    rel.file_name().and_then(|n| n.to_str()) != Some("index.md")
}

/// Infer a content file's canonical `type` from its store-relative path — the
/// inverse of [`default_type_folder`] and the single source of truth for
/// path→type inference (the CLI's `fm init` calls this, never re-derives it).
///
/// Requires the canonical `<layer>/<type-folder>/<file>` 3-component shape; a
/// shorter path (a file directly under a layer) or an unknown leading layer
/// yields `None`.
///
/// Recognized `(layer, folder)` pairs map back to their canonical type. For an
/// unrecognized folder the fallback is the **bare folder name verbatim** (no
/// pluralization/singularization) so it round-trips with `default_type_folder`,
/// whose unrecognized fallback is the bare type name (`task` ⇄ `records/task`).
/// Singularizing here would break that round-trip (`records/tasks` → `task`
/// while `default_type_folder("task")` → `records/task`). A conclusion record's
/// folder (e.g. `records/profiles/`) infers its bare folder name (`profiles`),
/// the same custom-type fallback as any other unrecognized folder.
pub fn infer_type_from_path(rel: &Path) -> Option<String> {
    let mut comps = rel.components().filter_map(|c| c.as_os_str().to_str());
    let layer = comps.next()?;
    if !matches!(layer, "sources" | "records") {
        return None;
    }
    let folder = comps.next()?;
    // The file itself must be a third component (a real type-folder, not the
    // file sitting directly under the layer).
    comps.next()?;

    let mapped = match (layer, folder) {
        ("sources", "emails") => "email",
        ("sources", "transcripts") => "transcript",
        ("sources", "docs") => "pdf-source",
        ("sources", "notes") => "note",
        ("records", "contacts") => "contact",
        ("records", "companies") => "company",
        ("records", "expenses") => "expense",
        ("records", "meetings") => "meeting",
        ("records", "decisions") => "decision",
        ("records", "invoices") => "invoice",
        // Unrecognized folder: the bare name, verbatim. This is the inverse of
        // `default_type_folder`'s unrecognized fallback (`other → records/other`)
        // and the round-trip would break if we pluralized/singularized here.
        (_, other) => other,
    };
    Some(mapped.to_string())
}

/// The primary date field name for a sharding type (the field whose value
/// drives `<YYYY>/<MM>`). `None` means "use the `created` fallback only".
fn primary_date_field(type_: &str) -> Option<&'static str> {
    match type_ {
        "email" => Some("date"),
        "transcript" => Some("recorded_at"),
        "pdf-source" => Some("received_at"),
        "note" => Some("told_at"),
        "expense" | "invoice" | "meeting" => Some("date"),
        // recognized custom event types have no canonical date field name; they
        // fall back to `created`.
        _ => None,
    }
}

/// Parse a YAML value into an RFC3339 [`DateTime`], accepting both an explicit
/// string and a YAML-native scalar rendered to string.
fn value_to_datetime(value: &serde_norway::Value) -> Option<DateTime<FixedOffset>> {
    let s = yaml_scalar_string(value)?;
    DateTime::parse_from_rfc3339(s.trim()).ok()
}

/// Extract `(YYYY, MM)` from a YAML date/timestamp value. Lenient: matches a
/// leading `YYYY-MM` so a bare `2026-05-22` date and a full
/// `2026-05-22T10:00:00-07:00` timestamp both work.
fn value_to_year_month(value: &serde_norway::Value) -> Option<(String, String)> {
    let s = yaml_scalar_string(value)?;
    year_month_from_str(s.trim())
}

/// `(YYYY, MM)` from the leading `YYYY-M` or `YYYY-MM` of a date string, with
/// the month returned zero-padded to two digits.
///
/// The month may be single- OR double-digit so that `2026-1-15` and its
/// zero-padded twin `2026-01-15` shard to the *same* `2026/01` folder. This
/// matches the lenient `date`-shape validator (`is_iso8601_date_or_datetime`,
/// chrono `%Y-%m-%d`), which accepts an unpadded month — without this, a value
/// the validator treats as a valid date is silently mis-filed under the
/// `created`-fallback month. Genuinely non-date input still returns `None`.
fn year_month_from_str(s: &str) -> Option<(String, String)> {
    // Hand-roll the leading-`YYYY-M[M]` parse to avoid a regex compile on the
    // write path. Split on '-': require a 4-digit year, then a 1-or-2-digit
    // numeric month in 1..=12. Anything after the month (a `-DD` day, a `T...`
    // time) is ignored — the day field never separates the leading date.
    let mut parts = s.splitn(3, '-');
    let year = parts.next()?;
    let month_part = parts.next()?;

    // Year: exactly 4 ASCII digits.
    if year.len() != 4 || !year.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    // Month: 1 or 2 ASCII digits, value 1..=12. Padded to two digits on output.
    if month_part.is_empty()
        || month_part.len() > 2
        || !month_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let month: u8 = month_part.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }

    Some((year.to_string(), format!("{month:02}")))
}

/// Render a YAML scalar as a string: a real `String` verbatim, otherwise the
/// value's compact YAML serialization (covers timestamps that the YAML engine
/// may surface as a non-string scalar).
fn yaml_scalar_string(value: &serde_norway::Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    match value {
        serde_norway::Value::Null => None,
        serde_norway::Value::Mapping(_) | serde_norway::Value::Sequence(_) => None,
        other => serde_norway::to_string(other)
            .ok()
            .map(|s| s.trim().to_string()),
    }
}

/// The YAML frontmatter block of a file: the text between a leading `---` fence
/// and the next `---` fence, exclusive. `None` if the file does not open with a
/// `---` fence on its first line.
fn frontmatter_block(text: &str) -> Option<&str> {
    // Tolerate a UTF-8 BOM and CRLF, but the fence must be the very first line.
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rest = body;
    // First line must be exactly `---`, tolerating trailing whitespace (CR, but
    // also spaces/tabs) — matching the canonical parser (`parser.rs` /
    // `index.rs`'s `extract_frontmatter_block`). A strict `\r`-only trim missed a
    // `--- ` fence, so `read_updated` returned None and date-sharding silently
    // fell back, disagreeing with the sidecar the rest of the toolkit builds.
    let (first, after_first) = split_first_line(rest);
    if first.trim_end() != "---" {
        return None;
    }
    rest = after_first;
    let block_start = rest;
    let mut scanned = 0usize;
    loop {
        let (line, after) = split_first_line(rest);
        if line.trim_end() == "---" {
            return Some(&block_start[..scanned]);
        }
        if after.is_empty() && line.is_empty() {
            // Reached end of input without a closing fence.
            return None;
        }
        scanned += line.len() + 1; // +1 for the consumed '\n'
        if after.is_empty() {
            return None;
        }
        rest = after;
    }
}

/// Split a file's text into `(frontmatter, body)` at the leading `---`…`---`
/// fence — raw (no YAML parse), so a file with malformed frontmatter is still
/// split and fully scanned. `frontmatter` is the text between the fences
/// (exclusive); `body` is everything after the closing fence's line. Returns
/// `None` when the text does not open with a `---` fence or has no closing
/// fence — the caller then treats the whole text as body. Mirrors
/// [`frontmatter_block`]'s boundary detection (BOM- and CRLF-tolerant).
fn split_frontmatter_raw(text: &str) -> Option<(&str, &str)> {
    let stripped = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (first, after_first) = split_first_line(stripped);
    if first.trim_end() != "---" {
        return None;
    }
    let block_start = after_first;
    let mut scanned = 0usize;
    let mut rest = after_first;
    loop {
        let (line, after) = split_first_line(rest);
        if line.trim_end() == "---" {
            // `after` is the body: everything past the closing fence line.
            return Some((&block_start[..scanned], after));
        }
        if after.is_empty() && line.is_empty() {
            return None; // reached EOF with no closing fence
        }
        scanned += line.len() + 1; // +1 for the consumed '\n'
        if after.is_empty() {
            return None; // closing fence never found
        }
        rest = after;
    }
}

/// Split a string into (first line without its trailing `\n`, remainder after
/// the `\n`). If there is no newline, the whole string is the line and the
/// remainder is empty.
fn split_first_line(s: &str) -> (&str, &str) {
    match s.find('\n') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

/// True if an [`IndexRecord`] has a field `key` equal to `value`, checking the
/// typed columns first and then the flattened `fields` map.
fn record_matches_field(record: &IndexRecord, key: &str, value: &str) -> bool {
    match key {
        "type" => record.type_ == value,
        "summary" => record.summary == value,
        "path" => record.path.to_string_lossy() == value,
        "created" => timestamp_matches(record.created, value),
        "updated" => timestamp_matches(record.updated, value),
        "tags" => record.tags.iter().any(|t| t == value),
        "links" => record.links.iter().any(|l| l == value),
        other => record
            .fields
            .get(other)
            .map(|v| json_value_matches(v, value))
            .unwrap_or(false),
    }
}

/// Compare a record's `created`/`updated` instant against a query `value`.
///
/// db.md files write timestamps in several equivalent RFC3339 spellings — most
/// commonly the `Z` UTC designator (`2026-05-01T00:00:00Z`) but also an explicit
/// offset (`...+00:00`, `...-07:00`). A naive `record.created.to_rfc3339() ==
/// value` reformats only one side: chrono renders a UTC instant as `+00:00`, so
/// the `Z` form an agent reads straight out of the file would never match. We
/// instead parse `value` as RFC3339 and compare instants, where `Z` and `+00:00`
/// (and any same-instant offset) are equal. A `value` that is not valid RFC3339
/// can never equal a real timestamp, so it falls through to `false`.
fn timestamp_matches(stored: Option<DateTime<FixedOffset>>, value: &str) -> bool {
    match (stored, DateTime::parse_from_rfc3339(value)) {
        (Some(stored), Ok(queried)) => stored == queried,
        _ => false,
    }
}

/// Match a JSON number against a query string.
///
/// A FLOAT-valued field is compared NUMERICALLY, not textually: the sidecar
/// stores a YAML float through serde_json's canonical f64 rendering, which
/// discards the file's source spelling (`1234.00` -> `1234.0`, `12.50` ->
/// `12.5`, `1e3` -> `1000.0`). A raw `to_string()` compare therefore made the
/// spelling a human reads in the file fail to match (and disagreed with
/// free-text `search`), while requiring a canonical form often absent from the
/// file. We parse the query as f64 and compare values. Restricted to the float
/// case so a large INTEGER field never loses exactness to f64 rounding (integers
/// render canonically and round-trip exactly through the textual compare).
/// Mirrors the parse-then-compare pattern [`timestamp_matches`] already uses.
fn number_matches(n: &serde_json::Number, value: &str) -> bool {
    if n.to_string() == value {
        return true;
    }
    if n.is_f64() {
        if let (Some(stored), Ok(q)) = (n.as_f64(), value.parse::<f64>()) {
            return stored == q;
        }
    }
    false
}

/// Compare a JSON field value against a query string. A string matches
/// verbatim; scalars match their textual form; an array matches if any element
/// matches (so a list-valued frontmatter field is membership-queried).
fn json_value_matches(v: &serde_json::Value, value: &str) -> bool {
    match v {
        serde_json::Value::String(s) => s == value,
        serde_json::Value::Bool(b) => b.to_string() == value,
        serde_json::Value::Number(n) => number_matches(n, value),
        serde_json::Value::Array(items) => items.iter().any(|i| json_value_matches(i, value)),
        // A present-but-null field never matches — consistent with the in-memory
        // post-filter (`query::json_value_matches`, which the first `where`
        // clause is NOT re-checked against, so the two must agree here or a
        // `--where field=` query would return different rows than `--type X
        // --where field=`).
        serde_json::Value::Null => false,
        serde_json::Value::Object(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::{tempdir, TempDir};

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// Write `contents` to `<root>/<rel>`, creating parent dirs. Returns the
    /// store-relative path for convenient assertions.
    fn write(root: &Path, rel: &str, contents: &str) -> PathBuf {
        let abs = root.join(rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(&abs, contents).unwrap();
        PathBuf::from(rel)
    }

    /// A minimal content file with the given `updated` timestamp in frontmatter.
    fn content_md(updated: &str) -> String {
        format!(
            "---\ntype: note\ncreated: {updated}\nupdated: {updated}\nsummary: a note\n---\n\nbody\n"
        )
    }

    /// A bare directory with a `DB.md` marker (valid `db-md` frontmatter so the
    /// real parser is exercised).
    fn empty_store() -> TempDir {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("DB.md"),
            "---\ntype: db-md\nscope: company\nowner: Test\n---\n\n# Store\n",
        )
        .unwrap();
        dir
    }

    /// Open a store rooted at a TempDir; panics if `open` rejects it.
    fn open(dir: &TempDir) -> Store {
        Store::open(dir.path()).expect("fixture should be a valid store")
    }

    fn rels(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    // ── Layer ───────────────────────────────────────────────────────────────

    #[test]
    fn layer_dir_name_and_parse_are_inverse() {
        for layer in Layer::all() {
            assert_eq!(Layer::from_dir_name(layer.dir_name()), Some(layer));
        }
        assert_eq!(Layer::Sources.dir_name(), "sources");
        assert_eq!(Layer::Records.dir_name(), "records");
        // `wiki` is no longer a layer (the wiki/ layer was removed); it parses to None.
        assert_eq!(Layer::from_dir_name("wiki"), None);
        assert_eq!(Layer::from_dir_name("log"), None);
        assert_eq!(Layer::from_dir_name("Sources"), None); // case-sensitive
    }

    #[test]
    fn layer_order_is_canonical() {
        // stats keys a BTreeMap on Layer; the sort order must be sources<records.
        let mut v = [Layer::Records, Layer::Sources];
        v.sort();
        assert_eq!(v, [Layer::Sources, Layer::Records]);
    }

    #[test]
    fn is_content_path_is_layer_rooted_and_excludes_non_layer_files() {
        // Real content: a `.md` file rooted in a layer's FIRST component.
        assert!(is_content_path(Path::new("records/contacts/alice.md")));
        assert!(is_content_path(Path::new("sources/emails/2026/05/x.md")));
        // Store-root meta files and a bare top-level note are NOT content.
        assert!(!is_content_path(Path::new("DB.md")));
        assert!(!is_content_path(Path::new("log.md")));
        assert!(!is_content_path(Path::new("NOTES.md")));
        // Non-layer top-level dirs are NEVER content — even if a DEEPER
        // component is named `records`/`sources` (the rename data-loss case).
        assert!(!is_content_path(Path::new("scratch/draft.md")));
        assert!(!is_content_path(Path::new("EXPECTED/snapshot.md")));
        assert!(!is_content_path(Path::new("archive/old.md")));
        assert!(!is_content_path(Path::new(
            "EXPECTED/records/contacts/x.md"
        )));
        assert!(!is_content_path(Path::new("archive/sources/emails/y.md")));
        // An `index.md` sidecar inside a layer is a catalog, not content.
        assert!(!is_content_path(Path::new("records/contacts/index.md")));
        // A non-`.md` file inside a layer (e.g. the jsonl sidecar) is not content.
        assert!(!is_content_path(Path::new("records/contacts/index.jsonl")));
    }

    // ── is_db_md_store / open ────────────────────────────────────────────────

    #[test]
    fn is_store_true_only_with_uppercase_marker() {
        let dir = tempdir().unwrap();
        assert!(
            !Store::is_db_md_store(dir.path()),
            "no marker → not a store"
        );

        fs::write(dir.path().join("DB.md"), "---\ntype: db-md\n---\n").unwrap();
        assert!(Store::is_db_md_store(dir.path()), "uppercase DB.md → store");
    }

    #[test]
    fn is_store_false_for_lowercase_db_md() {
        // The case-sensitivity contract: a lowercase db.md is the spec name, not
        // a marker — even on a case-insensitive filesystem where Path::exists
        // would lie. This test must pass on macOS (case-insensitive) too.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("db.md"), "---\ntype: db-md\n---\n").unwrap();
        assert!(
            !Store::is_db_md_store(dir.path()),
            "lowercase db.md must NOT be treated as a store marker"
        );
        assert!(Store::open(dir.path()).is_err());
    }

    #[test]
    fn is_store_false_when_db_md_is_a_directory() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("DB.md")).unwrap();
        assert!(
            !Store::is_db_md_store(dir.path()),
            "a directory named DB.md is not the file marker"
        );
    }

    #[test]
    fn open_rejects_non_store_with_path() {
        let dir = tempdir().unwrap();
        let err = Store::open(dir.path()).unwrap_err();
        assert_eq!(err.path, dir.path());
    }

    #[test]
    fn open_succeeds_and_parses_config() {
        let dir = tempdir().unwrap();
        // A DB.md whose ## Policies declares a frozen page — proves open()
        // actually parsed the config rather than substituting a default.
        fs::write(
            dir.path().join("DB.md"),
            "---\ntype: db-md\nscope: company\nowner: Test\n---\n\n# Store\n\n\
             ## Policies\n\n### Frozen pages\n- records/decisions/q1.md\n",
        )
        .unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.root, dir.path());
        assert!(
            store
                .config
                .frozen_pages
                .iter()
                .any(|p| p == Path::new("records/decisions/q1.md")),
            "open() must surface DB.md ## Policies, got {:?}",
            store.config.frozen_pages
        );
    }

    // ── walk / walk_layer / walk_type_folder ─────────────────────────────────

    #[test]
    fn walk_collects_content_across_layers_skipping_meta_and_log() {
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "sources/emails/2026/05/a.md",
            &content_md("2026-05-01T00:00:00Z"),
        );
        write(
            root,
            "records/contacts/sarah.md",
            &content_md("2026-05-02T00:00:00Z"),
        );
        write(
            root,
            "records/profiles/sarah.md",
            &content_md("2026-05-03T00:00:00Z"),
        );
        // Things walk() must SKIP:
        write(root, "sources/emails/index.md", "---\ntype: index\n---\n"); // catalog
        write(root, "index.md", "---\ntype: index\n---\n"); // root catalog
        write(root, "log.md", "---\ntype: log\n---\n"); // log
        write(root, "log/2026-04.md", "---\ntype: log\n---\n"); // rotated log archive
        write(
            root,
            "sources/.hidden/secret.md",
            &content_md("2026-05-09T00:00:00Z"),
        ); // hidden dir
        write(root, "records/contacts/notes.txt", "not markdown"); // non-md

        let store = open(&dir);
        let got = rels(&store.walk().unwrap());
        assert_eq!(
            got,
            vec![
                "records/contacts/sarah.md".to_string(),
                "records/profiles/sarah.md".to_string(),
                "sources/emails/2026/05/a.md".to_string(),
            ]
        );
    }

    #[test]
    fn walk_includes_content_named_log_md_or_db_md_inside_a_layer() {
        let dir = empty_store();
        let root = dir.path();
        // A content file that merely happens to be named log.md / DB.md INSIDE a
        // layer is real content — those names are reserved only at the store root.
        write(
            root,
            "records/configs/log.md",
            &content_md("2026-05-01T00:00:00Z"),
        );
        write(
            root,
            "sources/docs/DB.md",
            &content_md("2026-05-02T00:00:00Z"),
        );
        // The derived catalog twin is still skipped at any depth.
        write(root, "records/configs/index.md", "---\ntype: index\n---\n");
        let store = open(&dir);
        let got = rels(&store.walk().unwrap());
        assert!(
            got.contains(&"records/configs/log.md".to_string()),
            "layer-internal log.md is content: {got:?}"
        );
        assert!(
            got.contains(&"sources/docs/DB.md".to_string()),
            "layer-internal DB.md is content: {got:?}"
        );
        assert!(
            !got.iter().any(|p| p.ends_with("index.md")),
            "index.md is still skipped: {got:?}"
        );
    }

    #[test]
    fn walk_layer_is_scoped() {
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "sources/emails/2026/05/a.md",
            &content_md("2026-05-01T00:00:00Z"),
        );
        write(
            root,
            "records/contacts/sarah.md",
            &content_md("2026-05-02T00:00:00Z"),
        );
        let store = open(&dir);

        assert_eq!(
            rels(&store.walk_layer(Layer::Sources).unwrap()),
            vec!["sources/emails/2026/05/a.md".to_string()]
        );
        assert_eq!(
            rels(&store.walk_layer(Layer::Records).unwrap()),
            vec!["records/contacts/sarah.md".to_string()]
        );
        // A layer with no directory is empty, not an error: a store with only a
        // sources/ tree has no records/ dir, so walking Records is empty.
        let only_sources = empty_store();
        write(
            only_sources.path(),
            "sources/emails/2026/05/a.md",
            &content_md("2026-05-01T00:00:00Z"),
        );
        let s2 = open(&only_sources);
        assert!(s2.walk_layer(Layer::Records).unwrap().is_empty());
    }

    #[test]
    fn walk_type_folder_recurses_shards_and_accepts_abs_or_rel() {
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "sources/emails/2026/05/a.md",
            &content_md("2026-05-01T00:00:00Z"),
        );
        write(
            root,
            "sources/emails/2026/06/b.md",
            &content_md("2026-06-01T00:00:00Z"),
        );
        write(root, "sources/emails/index.md", "---\ntype: index\n---\n"); // skipped
                                                                           // A different type folder must not leak in.
        write(
            root,
            "sources/docs/2026/05/c.md",
            &content_md("2026-05-04T00:00:00Z"),
        );
        let store = open(&dir);

        let expected = vec![
            "sources/emails/2026/05/a.md".to_string(),
            "sources/emails/2026/06/b.md".to_string(),
        ];
        // Relative folder arg.
        assert_eq!(
            rels(&store.walk_type_folder(Path::new("sources/emails")).unwrap()),
            expected
        );
        // Absolute folder arg under the store resolves identically.
        assert_eq!(
            rels(
                &store
                    .walk_type_folder(&root.join("sources/emails"))
                    .unwrap()
            ),
            expected
        );
    }

    // ── recent_in_type_folder ────────────────────────────────────────────────

    #[test]
    fn recent_orders_by_updated_desc_then_path_and_caps() {
        let dir = empty_store();
        let root = dir.path();
        // newest
        write(
            root,
            "records/meetings/2026/05/c.md",
            &content_md("2026-05-03T00:00:00Z"),
        );
        // tie on updated — path asc decides (a before b)
        write(
            root,
            "records/meetings/2026/05/a.md",
            &content_md("2026-05-02T00:00:00Z"),
        );
        write(
            root,
            "records/meetings/2026/05/b.md",
            &content_md("2026-05-02T00:00:00Z"),
        );
        // oldest
        write(
            root,
            "records/meetings/2026/04/z.md",
            &content_md("2026-04-01T00:00:00Z"),
        );
        let store = open(&dir);

        let all = rels(
            &store
                .recent_in_type_folder(Path::new("records/meetings"), 10)
                .unwrap(),
        );
        assert_eq!(
            all,
            vec![
                "records/meetings/2026/05/c.md".to_string(), // newest
                "records/meetings/2026/05/a.md".to_string(), // tie, path asc
                "records/meetings/2026/05/b.md".to_string(),
                "records/meetings/2026/04/z.md".to_string(), // oldest
            ]
        );

        // Cap takes the n most-recent.
        let top2 = rels(
            &store
                .recent_in_type_folder(Path::new("records/meetings"), 2)
                .unwrap(),
        );
        assert_eq!(
            top2,
            vec![
                "records/meetings/2026/05/c.md".to_string(),
                "records/meetings/2026/05/a.md".to_string(),
            ]
        );
    }

    #[test]
    fn recent_sorts_undated_files_last() {
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "records/contacts/dated.md",
            &content_md("2026-05-01T00:00:00Z"),
        );
        // No `updated` field at all.
        write(
            root,
            "records/contacts/undated.md",
            "---\ntype: contact\nsummary: x\n---\nbody\n",
        );
        let store = open(&dir);
        let got = rels(
            &store
                .recent_in_type_folder(Path::new("records/contacts"), 10)
                .unwrap(),
        );
        assert_eq!(
            got,
            vec![
                "records/contacts/dated.md".to_string(),
                "records/contacts/undated.md".to_string(),
            ],
            "a file with a real `updated` must outrank one with none"
        );
    }

    // ── type_shards ──────────────────────────────────────────────────────────

    #[test]
    fn type_shards_classification() {
        let dir = empty_store();
        let store = open(&dir);
        for t in [
            "email",
            "transcript",
            "pdf-source",
            "expense",
            "invoice",
            "meeting",
            "order",
            "ticket",
            "transaction",
        ] {
            assert!(store.type_shards(t), "{t} should shard");
        }
        for t in [
            "contact", "company", "decision", "profile", "index", "log", "db-md", "proposal",
        ] {
            assert!(!store.type_shards(t), "{t} should stay flat");
        }
    }

    #[test]
    fn type_shards_respects_schema_directive_both_directions() {
        use crate::parser::{Config, Schema};
        let dir = empty_store();
        let mut store = open(&dir);
        let mut config = Config::default();
        // A CUSTOM type (not in the built-in list) opts into date-sharding —
        // without the schema override `type_shards` would return false for it.
        config.schemas.insert(
            "shipment".to_string(),
            Schema {
                shard: Some(true),
                ..Schema::default()
            },
        );
        // A BUILT-IN event type opts OUT (flat) — the override wins over the
        // built-in default.
        config.schemas.insert(
            "expense".to_string(),
            Schema {
                shard: Some(false),
                ..Schema::default()
            },
        );
        // A schema with no `shard:` directive leaves the built-in default intact.
        config
            .schemas
            .insert("meeting".to_string(), Schema::default());
        store.config = config;

        assert!(
            store.type_shards("shipment"),
            "custom type with `shard: by-date` must shard"
        );
        assert!(
            !store.type_shards("expense"),
            "built-in event type with `shard: flat` must go flat"
        );
        assert!(
            store.type_shards("meeting"),
            "schema without a `shard:` directive keeps the built-in default"
        );
        assert!(
            !store.type_shards("contact"),
            "unconfigured entity type stays flat"
        );
    }

    // ── year_month_from_str ──────────────────────────────────────────────────

    #[test]
    fn year_month_from_str_accepts_unpadded_month() {
        // A single-digit month shards to the same zero-padded folder as its twin,
        // matching the lenient `date`-shape validator (chrono `%Y-%m-%d`).
        let ym = year_month_from_str;
        assert_eq!(
            ym("2026-1-15"),
            Some(("2026".to_string(), "01".to_string())),
        );
        assert_eq!(
            ym("2026-01-15"),
            Some(("2026".to_string(), "01".to_string())),
        );
        assert_eq!(
            ym("2026-12-5"),
            Some(("2026".to_string(), "12".to_string())),
        );
        assert_eq!(ym("2026-1"), Some(("2026".to_string(), "01".to_string())));
        // Full timestamps still parse off the leading date.
        assert_eq!(
            ym("2026-3-22T10:00:00-07:00"),
            Some(("2026".to_string(), "03".to_string())),
        );
    }

    #[test]
    fn year_month_from_str_rejects_non_dates() {
        // Genuinely non-date input still returns None (behavior unchanged).
        assert_eq!(year_month_from_str(""), None);
        assert_eq!(year_month_from_str("not-a-date"), None);
        assert_eq!(year_month_from_str("2026"), None); // no month part
        assert_eq!(year_month_from_str("26-1-15"), None); // year not 4 digits
        assert_eq!(year_month_from_str("2026-13-01"), None); // month out of range
        assert_eq!(year_month_from_str("2026-0-01"), None); // month zero
        assert_eq!(year_month_from_str("2026-001-01"), None); // month over 2 digits
        assert_eq!(year_month_from_str("2026-x-01"), None); // non-numeric month
        assert_eq!(year_month_from_str("20a6-1-15"), None); // non-numeric year
    }

    #[test]
    fn shard_path_accepts_unpadded_month_same_as_padded() {
        // End-to-end: an unpadded `date` shards to its real month, identically to
        // its zero-padded twin — not to the `created`-fallback month.
        let dir = empty_store();
        let store = open(&dir);

        let padded = store
            .shard_path_for("expense", &fm_with_extra("date", "2026-01-15"), "padded")
            .unwrap();
        assert_eq!(padded, PathBuf::from("records/expenses/2026/01/padded.md"));

        let single = store
            .shard_path_for("expense", &fm_with_extra("date", "2026-1-15"), "single")
            .unwrap();
        assert_eq!(single, PathBuf::from("records/expenses/2026/01/single.md"));
    }

    // ── shard_path_for ───────────────────────────────────────────────────────

    fn fm_with_extra(key: &str, value: &str) -> Frontmatter {
        let mut fm = Frontmatter::default();
        fm.extra.insert(
            key.to_string(),
            serde_norway::Value::String(value.to_string()),
        );
        fm
    }

    fn fm_with_created(rfc3339: &str) -> Frontmatter {
        Frontmatter {
            created: Some(DateTime::parse_from_rfc3339(rfc3339).unwrap()),
            ..Default::default()
        }
    }

    #[test]
    fn shard_path_uses_primary_date_field_per_type() {
        let dir = empty_store();
        let store = open(&dir);

        // expense.date → records/expenses/<YYYY>/<MM>/
        let p = store
            .shard_path_for("expense", &fm_with_extra("date", "2026-05-22"), "lunch")
            .unwrap();
        assert_eq!(p, PathBuf::from("records/expenses/2026/05/lunch.md"));

        // email.date → sources/emails/<YYYY>/<MM>/
        let p = store
            .shard_path_for(
                "email",
                &fm_with_extra("date", "2026-11-02T09:00:00-07:00"),
                "e1",
            )
            .unwrap();
        assert_eq!(p, PathBuf::from("sources/emails/2026/11/e1.md"));

        // transcript.recorded_at → sources/transcripts/<YYYY>/<MM>/
        let p = store
            .shard_path_for(
                "transcript",
                &fm_with_extra("recorded_at", "2025-01-15T12:00:00Z"),
                "t1",
            )
            .unwrap();
        assert_eq!(p, PathBuf::from("sources/transcripts/2025/01/t1.md"));
    }

    #[test]
    fn shard_path_falls_back_to_created() {
        let dir = empty_store();
        let store = open(&dir);
        // meeting with no `date` field but a `created` timestamp.
        let p = store
            .shard_path_for(
                "meeting",
                &fm_with_created("2024-07-09T08:30:00-04:00"),
                "sync",
            )
            .unwrap();
        assert_eq!(p, PathBuf::from("records/meetings/2024/07/sync.md"));
    }

    #[test]
    fn shard_path_primary_field_wins_over_created() {
        let dir = empty_store();
        let store = open(&dir);
        let mut fm = fm_with_created("2020-01-01T00:00:00Z");
        fm.extra.insert(
            "date".into(),
            serde_norway::Value::String("2026-05-22".into()),
        );
        let p = store.shard_path_for("expense", &fm, "x").unwrap();
        // The primary `date` (2026/05), not `created` (2020/01), drives the shard.
        assert_eq!(p, PathBuf::from("records/expenses/2026/05/x.md"));
    }

    #[test]
    fn shard_path_flat_types_have_no_shard_segment() {
        let dir = empty_store();
        let store = open(&dir);
        // A contact has a `created` date, but contacts stay flat.
        let p = store
            .shard_path_for(
                "contact",
                &fm_with_created("2026-05-22T00:00:00Z"),
                "sarah-chen",
            )
            .unwrap();
        assert_eq!(p, PathBuf::from("records/contacts/sarah-chen.md"));

        // A conclusion `profile` is a custom (non-built-in) type: it is flat (no
        // date shard) and lands under the records-layer fallback folder
        // `records/<type>` — `records/profile/<name>.md`, a conforming 3-component
        // `<layer>/<type-folder>/<file>` path. A 2-component path would be
        // invisible to the index/validate type-folder model.
        let p = store
            .shard_path_for("profile", &Frontmatter::default(), "renewal-theme")
            .unwrap();
        assert_eq!(p, PathBuf::from("records/profile/renewal-theme.md"));
    }

    /// Regression: a type written through the toolkit's own path computation
    /// must land at a path the index + validate type-folder model accepts. A
    /// 2-component `<layer>/<file>` path is one `type_folder_of` (in both `index`
    /// and `validate`) treats as "no type-folder" — it would either crash
    /// `Index::on_write` (it tried to create `index.md` inside a file) or be
    /// silently dropped from every catalog by `Index::rebuild_all`. A custom
    /// (non-built-in) type like a conclusion `profile` falls back to
    /// `records/<type>` — still a conforming 3-component
    /// `<layer>/<type-folder>/<file>` path.
    #[test]
    fn shard_path_custom_type_is_indexable_three_component_path() {
        let dir = empty_store();
        let store = open(&dir);
        let p = store
            .shard_path_for("profile", &Frontmatter::default(), "renewal-theme")
            .unwrap();
        // First two components are a layer + a non-empty type-folder segment;
        // the file is the third. This is exactly the shape `type_folder_of`
        // (`comps.len() >= 3`, `comps[0]` a known layer) requires.
        let comps: Vec<&str> = p.iter().filter_map(|c| c.to_str()).collect();
        assert_eq!(
            comps.len(),
            3,
            "custom-type path must be <layer>/<type-folder>/<file>, got {p:?}"
        );
        assert_eq!(
            comps[0], "records",
            "first component must be the records layer (a custom type is \
             filed under the records fallback)"
        );
        assert!(
            !comps[1].is_empty() && comps[1] != "renewal-theme.md",
            "second component must be a real type-folder, not the file: {p:?}"
        );
        assert!(
            comps[2].ends_with(".md"),
            "third component must be the .md file: {p:?}"
        );
    }

    #[test]
    fn shard_path_preserves_and_adds_md_extension() {
        let dir = empty_store();
        let store = open(&dir);
        let with = store
            .shard_path_for("contact", &Frontmatter::default(), "sarah.md")
            .unwrap();
        let without = store
            .shard_path_for("contact", &Frontmatter::default(), "sarah")
            .unwrap();
        assert_eq!(with, PathBuf::from("records/contacts/sarah.md"));
        assert_eq!(without, PathBuf::from("records/contacts/sarah.md"));
    }

    #[test]
    fn shard_path_errors_when_sharding_type_has_no_date() {
        let dir = empty_store();
        let store = open(&dir);
        // expense shards, but no `date` and no `created` → NoShardDate.
        let err = store
            .shard_path_for("expense", &Frontmatter::default(), "mystery")
            .unwrap_err();
        match err {
            StoreError::NoShardDate { file } => {
                assert_eq!(file, PathBuf::from("records/expenses/mystery.md"));
            }
            other => panic!("expected NoShardDate, got {other:?}"),
        }
    }

    // ── find_links_to ────────────────────────────────────────────────────────

    #[test]
    fn find_links_to_matches_all_accepted_spellings() {
        let dir = empty_store();
        let root = dir.path();
        let target = "records/contacts/sarah-chen";

        // Plain link.
        write(
            root,
            "records/profiles/sarah.md",
            &format!(
                "---\ntype: profile\nmeta-type: conclusion\nsummary: s\n---\nSee [[{target}]].\n"
            ),
        );
        // Link with display text.
        write(
            root,
            "records/meetings/2026/05/m.md",
            &format!("---\ntype: meeting\nsummary: s\n---\nWith [[{target}|Sarah]].\n"),
        );
        // Link with .md extension (accepted, warned by validate).
        write(
            root,
            "records/concepts/t.md",
            &format!(
                "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\n[[{target}.md]]\n"
            ),
        );
        // A catalog/index file also contains the link literally — included.
        write(
            root,
            "records/contacts/index.md",
            &format!("---\ntype: index\n---\n- [[{target}]] — Sarah\n"),
        );
        // No link to the target.
        write(
            root,
            "records/profiles/elena.md",
            "---\ntype: profile\nmeta-type: conclusion\nsummary: s\n---\nNo links here.\n",
        );
        // Short-form link must NOT match the full-path target.
        write(
            root,
            "records/profiles/bob.md",
            "---\ntype: profile\nmeta-type: conclusion\nsummary: s\n---\n[[sarah-chen]]\n",
        );
        // A longer path that merely starts with the target must NOT match
        // (boundary correctness): target `sarah-chen` vs `sarah-chen-jr`.
        write(
            root,
            "records/profiles/jr.md",
            &format!(
                "---\ntype: profile\nmeta-type: conclusion\nsummary: s\n---\n[[{target}-jr]]\n"
            ),
        );

        let store = open(&dir);
        let got = rels(&store.find_links_to(Path::new(target)).unwrap());
        assert_eq!(
            got,
            vec![
                "records/concepts/t.md".to_string(),
                "records/contacts/index.md".to_string(),
                "records/meetings/2026/05/m.md".to_string(),
                "records/profiles/sarah.md".to_string(),
            ]
        );
    }

    #[test]
    fn find_links_to_distinguishes_sibling_paths() {
        // Two contacts whose paths share a prefix; a link to one must not be
        // reported as a link to the other.
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "records/concepts/a.md",
            "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\n[[records/contacts/sarah]]\n",
        );
        write(
            root,
            "records/concepts/b.md",
            "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\n[[records/contacts/sarah-chen]]\n",
        );
        let store = open(&dir);

        assert_eq!(
            rels(
                &store
                    .find_links_to(Path::new("records/contacts/sarah"))
                    .unwrap()
            ),
            vec!["records/concepts/a.md".to_string()]
        );
        assert_eq!(
            rels(
                &store
                    .find_links_to(Path::new("records/contacts/sarah-chen"))
                    .unwrap()
            ),
            vec!["records/concepts/b.md".to_string()]
        );
    }

    #[test]
    fn regression_find_links_to_tolerates_invalid_utf8_on_a_matched_line() {
        // Regression: a `.md` file can carry a stray non-UTF-8 byte on the SAME
        // line as a `[[target]]` link (a verbatim-ingested `sources/` artifact,
        // e.g. a mis-decoded Latin-1 import). The scan must still report the
        // link — `find_links_to` / `find_links_to_any` (and `graph backlinks` +
        // the working-set validate incoming-linker pass) must not error out and
        // drop the legitimate UTF-8 linkers. The content scan reads the file
        // with `String::from_utf8_lossy`, so the invalid byte becomes a
        // replacement char and the ASCII `[[target]]` link is still extracted.
        let dir = empty_store();
        let root = dir.path();
        let target = "records/contacts/sarah-chen";

        // A clean, fully-UTF-8 linker that MUST be returned regardless.
        write(
            root,
            "records/profiles/clean.md",
            &format!(
                "---\ntype: profile\nmeta-type: conclusion\nsummary: s\n---\nSee [[{target}]].\n"
            ),
        );

        // A linker whose link line ALSO carries a stray 0xFF byte (a mis-decoded
        // Latin-1 import). Write raw bytes so the invalid byte survives — a
        // `&str` fixture could not express it. The byte-level regex still
        // matches `[[target]]` on this line; pre-fix the UTF8 sink aborted here.
        let mut bytes: Vec<u8> =
            b"---\ntype: email\nsummary: s\n---\nSee [[records/contacts/sarah-chen]] \xFF here\n"
                .to_vec();
        let dirty_abs = root.join("sources/emails/2026/05/raw.md");
        fs::create_dir_all(dirty_abs.parent().unwrap()).unwrap();
        fs::write(&dirty_abs, &bytes).unwrap();
        // Defensive: confirm the fixture really is invalid UTF-8 (so the test
        // exercises the bug, not a coincidentally-valid file).
        assert!(
            std::str::from_utf8(&bytes).is_err(),
            "fixture must contain invalid UTF-8 to exercise the regression"
        );
        bytes.clear();

        let store = open(&dir);
        let got = rels(
            &store
                .find_links_to(Path::new(target))
                .expect("a stray non-UTF-8 byte must not abort the backlink scan"),
        );
        assert_eq!(
            got,
            vec![
                "records/profiles/clean.md".to_string(),
                "sources/emails/2026/05/raw.md".to_string(),
            ],
            "both the clean linker and the one with an invalid byte on the link \
             line are reported; the scan degrades, it does not fail"
        );
    }

    // ── find_links_to_any (batch — the O(changed × store) fix) ─────────────────

    /// The working-set validate's incoming-linker discovery runs through
    /// `find_links_to_any` over the WHOLE changed set in one pass. This pins the
    /// batch contract that makes that single-pass behavior correct: the result is
    /// the union of incoming linkers across every target, with per-target
    /// boundary correctness preserved (no alternation arm bleeds into a
    /// prefix-sharing sibling). If a regression reverts the batch finder to a
    /// per-object loop, the union below would still hold — but the boundary +
    /// union-equivalence assertions are what guard the *correctness* of folding N
    /// scans into one regex.
    #[test]
    fn find_links_to_any_returns_the_union_with_boundary_correctness() {
        let dir = empty_store();
        let root = dir.path();

        // Two distinct targets, each with its own linker.
        write(
            root,
            "records/concepts/links-sarah.md",
            "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\n[[records/contacts/sarah-chen]]\n",
        );
        write(
            root,
            "records/concepts/links-acme.md",
            "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\nDeal with [[records/companies/acme|Acme]].\n",
        );
        // One file links to BOTH targets — must appear exactly once (deduped),
        // proving the per-file early-exit folds multiple-target hits into a
        // single result row rather than one row per matched target.
        write(
            root,
            "records/meetings/2026/05/m.md",
            "---\ntype: meeting\nsummary: s\n---\n[[records/contacts/sarah-chen]] re \
             [[records/companies/acme]]\n",
        );
        // A prefix-sharing sibling of a target: a link to `sarah-chen-jr` must NOT
        // be reported as a link to `sarah-chen` even though the alternation now
        // carries `sarah-chen` as one arm.
        write(
            root,
            "records/concepts/links-jr.md",
            "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\n[[records/contacts/sarah-chen-jr]]\n",
        );
        // A file that links to neither requested target.
        write(
            root,
            "records/concepts/unrelated.md",
            "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\n[[records/concepts/spend]]\n",
        );

        let store = open(&dir);
        let targets = vec![
            PathBuf::from("records/contacts/sarah-chen"),
            PathBuf::from("records/companies/acme"),
        ];

        let got = rels(&store.find_links_to_any(&targets).unwrap());
        assert_eq!(
            got,
            vec![
                "records/concepts/links-acme.md".to_string(),
                "records/concepts/links-sarah.md".to_string(),
                "records/meetings/2026/05/m.md".to_string(),
            ],
            "batch finder must return the deduped union of linkers across all \
             targets, excluding the prefix-sibling and the unrelated file"
        );

        // Equivalence: the batch result must equal the union of the per-target
        // single finder. This is the property the working-set path relies on
        // when it folds one-scan-per-object into one scan for the whole set.
        let mut union: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
        for t in &targets {
            for linker in store.find_links_to(t).unwrap() {
                union.insert(linker);
            }
        }
        assert_eq!(
            rels(&union.into_iter().collect::<Vec<_>>()),
            got,
            "find_links_to_any must equal the union of per-target find_links_to"
        );
    }

    /// An empty target set must scan nothing and find nothing — and crucially
    /// must NOT compile to a match-everything empty regex (which would report
    /// every `.md` as a linker). This is the empty-working-set fast path the
    /// `validate` loop hits when nothing changed.
    #[test]
    fn find_links_to_any_empty_targets_matches_nothing() {
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "records/concepts/a.md",
            "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\n[[records/contacts/sarah-chen]]\n",
        );
        let store = open(&dir);

        assert!(
            store.find_links_to_any(&[]).unwrap().is_empty(),
            "no targets ⇒ no linkers (an empty pattern must not match every file)"
        );
        // A set of only empty/non-link targets is likewise a no-op, not a
        // match-everything.
        assert!(
            store
                .find_links_to_any(&[PathBuf::from(""), PathBuf::from("./")])
                .unwrap()
                .is_empty(),
            "targets that render to empty link text contribute no alternation arm"
        );
    }

    // ── read_type_index ──────────────────────────────────────────────────────

    #[test]
    fn read_type_index_parses_records_and_flattens_fields() {
        let dir = empty_store();
        let root = dir.path();
        let jsonl = "\
{\"path\":\"records/expenses/2026/05/a.md\",\"type\":\"expense\",\"summary\":\"lunch\",\"tags\":[\"meals\"],\"links\":[\"records/companies/acme\"],\"created\":\"2026-05-01T00:00:00Z\",\"updated\":\"2026-05-01T00:00:00Z\",\"vendor\":\"acme\",\"amount\":42}
{\"path\":\"records/expenses/2026/05/b.md\",\"type\":\"expense\",\"summary\":\"taxi\",\"created\":null,\"updated\":null,\"vendor\":\"yellow\"}
";
        let p = write(root, "records/expenses/index.jsonl", jsonl);
        let store = open(&dir);
        let recs = store.read_type_index(&store.abs_path(&p)).unwrap();

        assert_eq!(recs.len(), 2);
        // Sorted by path asc.
        assert_eq!(recs[0].path, PathBuf::from("records/expenses/2026/05/a.md"));
        assert_eq!(recs[0].type_, "expense");
        assert_eq!(recs[0].summary, "lunch");
        assert_eq!(recs[0].tags, vec!["meals".to_string()]);
        assert_eq!(recs[0].links, vec!["records/companies/acme".to_string()]);
        assert!(recs[0].created.is_some());
        // Extra (non-typed) frontmatter flattens into `fields`.
        assert_eq!(
            recs[0].fields.get("vendor"),
            Some(&serde_json::json!("acme"))
        );
        assert_eq!(recs[0].fields.get("amount"), Some(&serde_json::json!(42)));
        // Defaults: missing tags/links → empty.
        assert!(recs[1].tags.is_empty());
        assert!(recs[1].links.is_empty());
    }

    #[test]
    fn read_type_index_last_write_wins_and_skips_blanks() {
        let dir = empty_store();
        let root = dir.path();
        // Same path twice; the second line supersedes the first. A blank line
        // in between must be ignored, not error.
        let jsonl = "\
{\"path\":\"records/contacts/sarah.md\",\"type\":\"contact\",\"summary\":\"old\",\"created\":null,\"updated\":null}

{\"path\":\"records/contacts/sarah.md\",\"type\":\"contact\",\"summary\":\"new\",\"created\":null,\"updated\":null}
";
        let p = write(root, "records/contacts/index.jsonl", jsonl);
        let store = open(&dir);
        let recs = store.read_type_index(&store.abs_path(&p)).unwrap();
        assert_eq!(recs.len(), 1, "duplicate path collapses to one record");
        assert_eq!(recs[0].summary, "new", "later line must win");
    }

    #[test]
    fn read_type_index_errors_on_malformed_line() {
        let dir = empty_store();
        let root = dir.path();
        let p = write(root, "records/contacts/index.jsonl", "{not valid json}\n");
        let store = open(&dir);
        let err = store.read_type_index(&store.abs_path(&p)).unwrap_err();
        assert!(matches!(err, StoreError::BadTypeIndex { .. }));
    }

    // ── find_by_type / find_by_where ─────────────────────────────────────────

    fn jsonl_line(path: &str, type_: &str, summary: &str, extra: &str) -> String {
        format!(
            "{{\"path\":\"{path}\",\"type\":\"{type_}\",\"summary\":\"{summary}\",\"created\":null,\"updated\":null{extra}}}\n"
        )
    }

    #[test]
    fn find_by_type_reads_canonical_folder_sidecar() {
        let dir = empty_store();
        let root = dir.path();
        // Canonical folder for `contact` is records/contacts.
        write(
            root,
            "records/contacts/index.jsonl",
            &(jsonl_line("records/contacts/sarah.md", "contact", "Sarah", "")
                + &jsonl_line("records/contacts/elena.md", "contact", "Elena", "")),
        );
        // A different type's sidecar must not leak into a contact query.
        write(
            root,
            "records/companies/index.jsonl",
            &jsonl_line("records/companies/acme.md", "company", "Acme", ""),
        );
        let store = open(&dir);
        let recs = store.find_by_type("contact").unwrap();
        let names: Vec<_> = recs.iter().map(|r| r.summary.clone()).collect();
        assert_eq!(names, vec!["Elena".to_string(), "Sarah".to_string()]); // path-sorted
        assert!(recs.iter().all(|r| r.type_ == "contact"));
    }

    #[test]
    fn regression_find_by_type_includes_non_canonical_folder_when_canonical_exists() {
        // Regression for the silent-incompleteness bug: once the canonical
        // type-folder sidecar exists, `find_by_type` used to read ONLY that
        // sidecar and drop same-type records filed in a non-canonical folder in
        // the SAME layer — so the result flipped to incomplete the moment a
        // canonical record was added. The write path actively enables such a
        // layout (`records/clients/` for a `contact`, any `records/<folder>/`
        // for a conclusion `profile`), so this is a reachable, dedup-breaking
        // omission.
        let dir = empty_store();
        let root = dir.path();

        // CANONICAL folder sidecar exists (`records/contacts/` for `contact`),
        // which is exactly the condition that triggered the bug.
        write(
            root,
            "records/contacts/index.jsonl",
            &jsonl_line("records/contacts/sarah.md", "contact", "Sarah", ""),
        );
        // A `contact` filed in a NON-canonical folder within the same (Records)
        // layer. Pre-fix this was silently dropped because the canonical
        // sidecar existed; it must now come back.
        write(
            root,
            "records/clients/index.jsonl",
            &jsonl_line("records/clients/elena.md", "contact", "Elena", ""),
        );
        // A different type in the same layer must NOT leak in (proves the read
        // is type-filtered, not just a blind whole-layer dump).
        write(
            root,
            "records/companies/index.jsonl",
            &jsonl_line("records/companies/acme.md", "company", "Acme", ""),
        );

        let store = open(&dir);
        let got: std::collections::BTreeSet<String> = store
            .find_by_type("contact")
            .unwrap()
            .into_iter()
            .map(|r| r.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            got,
            ["records/clients/elena.md", "records/contacts/sarah.md"]
                .into_iter()
                .map(String::from)
                .collect::<std::collections::BTreeSet<_>>(),
            "both the canonical-folder and the non-canonical-folder contact must \
             be returned; the company record must be excluded"
        );
    }

    #[test]
    fn regression_find_by_type_profile_spans_multiple_topic_folders() {
        // Regression for the scoped-backlinks variant of the same bug
        // (`graph backlinks --type <conclusion-type>`): a conclusion type like
        // `profile` has the canonical fallback folder `records/profile`, but the
        // agent may file profiles under ANY records topic folder
        // (`records/people/`, `records/clients/`, …). With a
        // `records/profile/index.jsonl` present, the old code read only that
        // folder and dropped profiles in the other topic folders —
        // under-reporting dependents in a blast-radius check. The
        // whole-`records/`-layer read must surface all of them.
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "records/profile/index.jsonl",
            &jsonl_line("records/profile/billing.md", "profile", "Billing", ""),
        );
        write(
            root,
            "records/people/index.jsonl",
            &jsonl_line("records/people/sarah-chen.md", "profile", "Sarah Chen", ""),
        );
        write(
            root,
            "records/clients/index.jsonl",
            &jsonl_line("records/clients/atlas.md", "profile", "Atlas", ""),
        );

        let store = open(&dir);
        let got: std::collections::BTreeSet<String> = store
            .find_by_type("profile")
            .unwrap()
            .into_iter()
            .map(|r| r.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            got,
            [
                "records/clients/atlas.md",
                "records/people/sarah-chen.md",
                "records/profile/billing.md",
            ]
            .into_iter()
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>(),
            "a profile query must return records from every topic folder, not \
             just the canonical records/profile/"
        );
    }

    #[test]
    fn find_by_type_canonical_absent_falls_back_within_the_layer_only() {
        let dir = empty_store();
        let root = dir.path();
        // A custom `proposal` record filed under a non-canonical folder NAME
        // (the natural plural `records/proposals/`) inside the records layer.
        // `default_type_folder("proposal")` = `records/proposal` (bare type, no
        // pluralization guess), so the canonical sidecar does not exist and
        // `find_by_type` falls back. The fallback is bounded to the type's
        // layer (records), so this record — same layer, non-canonical folder —
        // is still found: completeness within the layer holds.
        write(
            root,
            "records/proposals/index.jsonl",
            &jsonl_line("records/proposals/p1.md", "proposal", "Q3 proposal", ""),
        );
        // A DECOY of the SAME type sitting in a DIFFERENT layer (sources/). The
        // old whole-store fallback read every sidecar in the store and would
        // have leaked this into the result; the layer-bounded fallback must not.
        // It also pins that the fallback is O(entities-in-layer), never O(store).
        write(
            root,
            "sources/proposals/index.jsonl",
            &jsonl_line(
                "sources/proposals/leak.md",
                "proposal",
                "cross-layer decoy",
                "",
            ),
        );
        let store = open(&dir);
        let recs = store.find_by_type("proposal").unwrap();
        assert_eq!(
            recs.len(),
            1,
            "only the records-layer proposal, not the sources decoy"
        );
        assert_eq!(recs[0].summary, "Q3 proposal");
        assert_eq!(recs[0].path, PathBuf::from("records/proposals/p1.md"));
    }

    #[test]
    fn find_by_type_canonical_absent_does_not_read_other_layers() {
        let dir = empty_store();
        let root = dir.path();
        // `email`'s canonical folder is `sources/emails` (layer Sources). No
        // sidecar there yet, so `find_by_type("email")` falls back — but only
        // within the Sources layer. A populated sidecar in the Records layer
        // must never be touched: the fallback is layer-bounded, not store-wide.
        // Under the old `read_all_type_indexes_in(None)` fallback this records
        // sidecar would have been read and filtered (wasted O(store) I/O); now
        // it is outside the walk root entirely.
        write(
            root,
            "records/contacts/index.jsonl",
            &jsonl_line("records/contacts/sarah.md", "contact", "Sarah", ""),
        );
        let store = open(&dir);
        // No email anywhere ⇒ empty, and the records layer was not in scope.
        assert!(store.find_by_type("email").unwrap().is_empty());
    }

    #[test]
    fn find_by_where_matches_typed_columns_and_flat_fields() {
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "records/expenses/index.jsonl",
            &(jsonl_line(
                "records/expenses/a.md",
                "expense",
                "lunch",
                ",\"vendor\":\"acme\",\"tags\":[\"meals\"]",
            ) + &jsonl_line(
                "records/expenses/b.md",
                "expense",
                "taxi",
                ",\"vendor\":\"yellow\"",
            )),
        );
        write(
            root,
            "records/contacts/index.jsonl",
            &jsonl_line(
                "records/contacts/sarah.md",
                "contact",
                "Sarah",
                ",\"tags\":[\"customer\"]",
            ),
        );
        let store = open(&dir);

        // Flat field in `fields`.
        let by_vendor = store.find_by_where("vendor", "acme").unwrap();
        assert_eq!(by_vendor.len(), 1);
        assert_eq!(by_vendor[0].path, PathBuf::from("records/expenses/a.md"));

        // Typed column: type (spans both expense records).
        assert_eq!(store.find_by_where("type", "expense").unwrap().len(), 2);

        // Typed list column: tags membership.
        let customers = store.find_by_where("tags", "customer").unwrap();
        assert_eq!(customers.len(), 1);
        assert_eq!(
            customers[0].path,
            PathBuf::from("records/contacts/sarah.md")
        );

        // No match → empty.
        assert!(store.find_by_where("vendor", "nobody").unwrap().is_empty());
    }

    #[test]
    fn find_by_where_matches_timestamps_across_rfc3339_spellings() {
        let dir = empty_store();
        let root = dir.path();
        // db.md files most commonly carry the `Z` UTC spelling. The index.jsonl
        // serialized from such a file preserves it verbatim.
        write(
            root,
            "records/meetings/index.jsonl",
            "{\"path\":\"records/meetings/kickoff.md\",\"type\":\"meeting\",\
\"summary\":\"kickoff\",\"created\":\"2026-05-01T00:00:00Z\",\
\"updated\":\"2026-05-02T09:30:00-07:00\"}\n",
        );
        let store = open(&dir);

        // The exact value an agent reads out of the file (`Z` form) must match.
        let by_z = store
            .find_by_where("created", "2026-05-01T00:00:00Z")
            .unwrap();
        assert_eq!(by_z.len(), 1);
        assert_eq!(by_z[0].path, PathBuf::from("records/meetings/kickoff.md"));

        // The equivalent explicit-offset spelling of the same instant matches too.
        assert_eq!(
            store
                .find_by_where("created", "2026-05-01T00:00:00+00:00")
                .unwrap()
                .len(),
            1
        );

        // A non-UTC stored value matches both its own offset spelling and the
        // same instant expressed as `Z` (instant comparison, not string compare).
        assert_eq!(
            store
                .find_by_where("updated", "2026-05-02T09:30:00-07:00")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .find_by_where("updated", "2026-05-02T16:30:00Z")
                .unwrap()
                .len(),
            1
        );

        // A different instant does not match.
        assert!(store
            .find_by_where("created", "2026-05-01T00:00:01Z")
            .unwrap()
            .is_empty());
        // A non-RFC3339 query value never matches a real timestamp.
        assert!(store
            .find_by_where("created", "2026-05-01")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn find_by_where_matches_floats_across_serialized_spellings() {
        // Adversarial review #5: a float field is stored in index.jsonl via
        // serde_json's canonical f64 render, which DISCARDS the file's source
        // spelling (`1234.00` -> `1234.0`, `1e3` -> `1000.0`). A textual compare
        // made the spelling a human reads in the file miss (and disagree with
        // free-text `search`); numeric compare fixes it. `fm query`/`index query`
        // is the SPEC pre-write dedup primitive, so a miss here silently writes a
        // duplicate record.
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "records/invoices/index.jsonl",
            "{\"path\":\"records/invoices/inv.md\",\"type\":\"invoice\",\
\"summary\":\"inv\",\"amount\":1234.0,\"score\":1000.0,\"count\":42}\n",
        );
        let store = open(&dir);

        // Every spelling of the same numeric value matches the canonical-f64 store.
        for spelling in ["1234.00", "1234.0", "1234"] {
            assert_eq!(
                store.find_by_where("amount", spelling).unwrap().len(),
                1,
                "amount spelling `{spelling}` must match the stored 1234.0"
            );
        }
        for spelling in ["1e3", "1000", "1000.0"] {
            assert_eq!(
                store.find_by_where("score", spelling).unwrap().len(),
                1,
                "score spelling `{spelling}` must match the stored 1000.0"
            );
        }
        // A genuinely different value does not match.
        assert!(store.find_by_where("amount", "1234.5").unwrap().is_empty());
        // Integer fields keep exact textual matching (unaffected by the fix).
        assert_eq!(store.find_by_where("count", "42").unwrap().len(), 1);
    }

    #[test]
    fn number_matches_is_numeric_for_floats_but_exact_for_integers() {
        use serde_json::Number;
        // Float-valued field: any equal spelling matches (the bug fix).
        let f: Number = serde_json::from_str("1234.0").unwrap();
        assert!(number_matches(&f, "1234.00"));
        assert!(number_matches(&f, "1234"));
        assert!(number_matches(&f, "1234.0"));
        assert!(!number_matches(&f, "1234.5"));
        // Integer-valued field: EXACT textual compare, never f64-rounded — two
        // adjacent large integers that round to the same f64 must NOT collide
        // (the safety property that motivates restricting numeric compare to
        // floats).
        let big: Number = serde_json::from_str("18446744073709551615").unwrap(); // u64::MAX
        assert!(number_matches(&big, "18446744073709551615"));
        assert!(!number_matches(&big, "18446744073709551614"));
    }

    #[test]
    fn find_by_where_in_layer_reads_only_that_layers_sidecars() {
        // The O(entities-in-layer) contract: a layer-scoped where read must walk
        // ONLY the named layer's subtree. Proven structurally — a *malformed*
        // sidecar in another layer would make `read_type_index` error if it were
        // read, so a scoped read that succeeds (and excludes that record) is
        // proof the other layer's I/O never happened.
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "records/companies/index.jsonl",
            &jsonl_line(
                "records/companies/acme.md",
                "company",
                "Acme",
                ",\"domain\":\"acme.com\"",
            ),
        );
        // Same field/value in the sources layer — but the sidecar is corrupt.
        write(
            root,
            "sources/emails/index.jsonl",
            "{ this is not valid json and would error if read }\n",
        );
        let store = open(&dir);

        // Scoped to records: the corrupt sources sidecar is out of scope, so the
        // read succeeds and returns only the records-layer match.
        let in_records = store
            .find_by_where_in("domain", "acme.com", Some(Layer::Records))
            .expect("a records-scoped read must not touch the sources sidecar");
        assert_eq!(
            rels(
                &in_records
                    .iter()
                    .map(|r| r.path.clone())
                    .collect::<Vec<_>>()
            ),
            vec!["records/companies/acme.md".to_string()]
        );

        // The store-wide read DOES reach the corrupt sidecar and surfaces it as
        // a parse error — confirming the corrupt file is genuinely in the tree
        // and that only the layer scope spares it.
        let store_wide = store.find_by_where("domain", "acme.com");
        assert!(
            matches!(store_wide, Err(StoreError::BadTypeIndex { .. })),
            "unscoped read walks every layer and hits the corrupt sidecar"
        );

        // Scoping to the layer that holds only the corrupt sidecar still errors
        // (the scope includes it), proving the scope is a real subtree bound and
        // not a silent "skip anything that fails".
        let in_sources = store.find_by_where_in("domain", "acme.com", Some(Layer::Sources));
        assert!(matches!(in_sources, Err(StoreError::BadTypeIndex { .. })));
    }

    #[test]
    fn find_by_where_in_missing_layer_is_empty_not_an_error() {
        // A layer-scoped read over a layer folder that does not exist yet must
        // return empty (mirrors `walk_layer`'s missing-dir guard), never a walk
        // error from `ignore` over a nonexistent path.
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "records/contacts/index.jsonl",
            &jsonl_line(
                "records/contacts/sarah.md",
                "contact",
                "Sarah",
                ",\"city\":\"denver\"",
            ),
        );
        let store = open(&dir);

        // `sources/` was never created.
        let in_sources = store
            .find_by_where_in("city", "denver", Some(Layer::Sources))
            .expect("missing layer subtree is empty, not an error");
        assert!(in_sources.is_empty());

        // Same query scoped to the layer that has the record still finds it.
        let in_records = store
            .find_by_where_in("city", "denver", Some(Layer::Records))
            .unwrap();
        assert_eq!(in_records.len(), 1);
    }

    // ── abs_path / rel_path ──────────────────────────────────────────────────

    #[test]
    fn abs_and_rel_path_roundtrip() {
        let dir = empty_store();
        let store = open(&dir);
        let rel = Path::new("records/contacts/sarah.md");
        let abs = store.abs_path(rel);
        assert_eq!(abs, dir.path().join(rel));
        assert_eq!(store.rel_path(&abs).as_deref(), Some(rel));

        // An absolute path is passed through unchanged by abs_path.
        assert_eq!(store.abs_path(&abs), abs);

        // A path outside the store has no store-relative form.
        assert_eq!(store.rel_path(Path::new("/somewhere/else.md")), None);
    }

    // ── infer_type_from_path (inverse of default_type_folder) ────────────────

    #[test]
    fn infer_type_maps_every_recognized_folder_back_to_its_type() {
        let cases = [
            ("sources/emails/x.md", "email"),
            ("sources/transcripts/x.md", "transcript"),
            ("sources/docs/x.md", "pdf-source"),
            ("sources/notes/x.md", "note"),
            ("records/contacts/x.md", "contact"),
            ("records/companies/x.md", "company"),
            ("records/expenses/x.md", "expense"),
            ("records/meetings/x.md", "meeting"),
            ("records/decisions/x.md", "decision"),
            ("records/invoices/x.md", "invoice"),
        ];
        for (path, expected) in cases {
            assert_eq!(
                infer_type_from_path(Path::new(path)).as_deref(),
                Some(expected),
                "path {path} should infer type {expected}"
            );
        }
    }

    #[test]
    fn infer_type_round_trips_with_default_type_folder() {
        // The canonical invariant: inference is the inverse of the forward map.
        // Every recognized type, routed through `default_type_folder` and then
        // back through `infer_type_from_path`, must return the original type.
        let recognized = [
            "email",
            "transcript",
            "pdf-source",
            "contact",
            "company",
            "expense",
            "meeting",
            "decision",
            "invoice",
        ];
        for type_ in recognized {
            let folder = default_type_folder(type_);
            let file = folder.join("x.md");
            assert_eq!(
                infer_type_from_path(&file).as_deref(),
                Some(type_),
                "recognized type {type_} (folder {folder:?}) must round-trip"
            );
        }
    }

    #[test]
    fn infer_type_round_trips_custom_types_verbatim_no_singularization() {
        // Regression guard for the CLI/core divergence: `default_type_folder`'s
        // unrecognized fallback is the BARE type name (`task → records/task`,
        // `tasks → records/tasks`). Inference must NOT singularize, or a custom
        // type would not round-trip (e.g. `records/tasks` → `task` would clash
        // with `default_type_folder("task") → records/task`).
        for custom in ["task", "tasks", "playbook", "process", "okrs", "ticket"] {
            let folder = default_type_folder(custom);
            assert_eq!(folder, PathBuf::from("records").join(custom));
            let file = folder.join("x.md");
            assert_eq!(
                infer_type_from_path(&file).as_deref(),
                Some(custom),
                "custom type {custom} must round-trip verbatim (no singularization)"
            );
        }

        // The specific case named in the finding: a plural custom folder keeps
        // its trailing `s`; it is NOT singularized to `task`.
        assert_eq!(
            infer_type_from_path(Path::new("records/tasks/x.md")).as_deref(),
            Some("tasks"),
            "records/tasks must infer `tasks`, not `task`"
        );
    }

    #[test]
    fn infer_type_requires_three_component_layer_folder_file_shape() {
        // Fewer than 3 components: a file directly under a layer has no
        // type-folder, so inference yields None (matches the old CLI contract).
        assert_eq!(infer_type_from_path(Path::new("records/x.md")), None);
        assert_eq!(infer_type_from_path(Path::new("sources/x.md")), None);
        assert_eq!(infer_type_from_path(Path::new("x.md")), None);
        // Unknown leading layer is never inferred.
        assert_eq!(infer_type_from_path(Path::new("foo/bar/x.md")), None);
        // Deeper paths still infer from the first type-folder segment (e.g. a
        // sharded record under records/expenses/2026/05/x.md).
        assert_eq!(
            infer_type_from_path(Path::new("records/expenses/2026/05/x.md")).as_deref(),
            Some("expense"),
        );
    }

    // ── ensure_path_within_store (containment) ───────────────────────────────

    #[test]
    fn ensure_path_within_store_accepts_in_store_and_rejects_escape() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("records/contacts")).unwrap();
        fs::write(root.join("records/contacts/sarah.md"), "x").unwrap();

        // An existing in-store file resolves and is accepted.
        let inside = root.join("records/contacts/sarah.md");
        let got = ensure_path_within_store(root, &inside).expect("in-store path accepted");
        // Canonical, but still under the (canonical) root.
        assert!(got.starts_with(root.canonicalize().unwrap()));

        // A not-yet-existing in-store leaf is accepted (rename destination).
        let new_leaf = root.join("records/contacts/sarah-chen.md");
        assert!(
            ensure_path_within_store(root, &new_leaf).is_ok(),
            "a non-existent in-store leaf must be accepted"
        );

        // A `..`-escaping path is rejected even though its prefix exists.
        let escape = root.join("records/contacts/../../outside/secret.md");
        assert!(
            ensure_path_within_store(root, &escape).is_err(),
            "a `..`-escaping path must be rejected"
        );
    }

    #[test]
    fn ensure_path_within_store_rejects_symlink_escape() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("store");
        fs::create_dir_all(&root).unwrap();
        let outside_dir = dir.path().join("outside");
        fs::create_dir_all(&outside_dir).unwrap();
        let secret = outside_dir.join("secret.md");
        fs::write(&secret, "TOPSECRET").unwrap();

        // A symlink inside the store that points OUTSIDE it must be rejected:
        // resolving the symlink lands outside the canonical root.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = root.join("escape.md");
            symlink(&secret, &link).unwrap();
            assert!(
                ensure_path_within_store(&root, &link).is_err(),
                "a symlink resolving outside the store must be rejected"
            );
        }
    }

    // ── shared link-edge notion (fence / whitespace / case) ──────────────────

    #[test]
    fn extract_edge_targets_trims_inner_whitespace() {
        // Padded `[[ x ]]` is the same edge as `[[x]]`.
        assert_eq!(
            extract_edge_targets("See [[ records/contacts/sarah ]] today."),
            vec!["records/contacts/sarah".to_string()]
        );
    }

    #[test]
    fn extract_edge_targets_skips_fenced_code_blocks() {
        // A `[[...]]` inside a ``` fence is a doc example, NOT an edge — matching
        // validate's body extractor.
        let body = "\
Real [[records/contacts/sarah]] link.

```markdown
[[records/contacts/ghost-example]] is how you link.
```

After fence [[records/companies/acme]].
";
        let got = extract_edge_targets(body);
        assert_eq!(
            got,
            vec![
                "records/contacts/sarah".to_string(),
                "records/companies/acme".to_string(),
            ],
            "fenced example link must not be an edge"
        );
    }

    #[test]
    fn extract_edge_targets_frontmatter_fence_does_not_swallow_body_links() {
        // Regression: `search_by_link` / `forwardlinks` / `dbmd links` feed the
        // WHOLE file (frontmatter + body) here. A stray code-fence run inside a
        // frontmatter value must NOT open a markdown fence that swallows the
        // body's real wiki-links. Frontmatter links are still edges; a link
        // genuinely inside a BODY fence is still ignored.
        let file = "\
---
type: note
summary: \"a note\"
ref: \"[[records/contacts/sarah]]\"
snippet: \"```\"
---

Body mentions [[records/companies/acme]].

```
[[records/contacts/ghost-example]] inside a body fence.
```

After fence [[records/contacts/dave]].
";
        let got = extract_edge_targets(file);
        assert_eq!(
            got,
            vec![
                "records/contacts/sarah".to_string(), // frontmatter edge
                "records/companies/acme".to_string(), // body edge AFTER the frontmatter ```
                "records/contacts/dave".to_string(),  // body edge after a real body fence
            ],
            "a code fence inside frontmatter must not suppress body wiki-links, \
             and a real body-fenced link must still be ignored"
        );
    }

    #[test]
    fn extract_edge_targets_handles_nested_indented_and_long_run_fences() {
        // Regression for the naive `starts_with("```")/("~~~")` toggle: a fence
        // nested inside another, an over-indented (>3 space) marker, and a
        // long-run fence wrapping a shorter inner one must all leave the block's
        // links un-extracted (validate treats the whole block as opaque). The
        // (char, run-length) tracker keys on the OPENING fence and closes only on
        // a matching char with run ≥ the opener.

        // (a) A ```` ```` ````-run block (run 4) wrapping a ``` example (run 3).
        // The inner ``` does NOT close the outer run-4 fence, so both `[[...]]`
        // inside stay fenced.
        let nested = "\
Doc:

````
```
[[records/contacts/bob]]
```
still fenced [[records/contacts/bob]]
````

Real [[records/companies/acme]].
";
        assert_eq!(
            extract_edge_targets(nested),
            vec!["records/companies/acme".to_string()],
            "a nested ``` inside a ````-run fence must not leak the fenced links"
        );

        // (b) A `~~~` block containing a ``` line (the standard way to document a
        // backtick fence). The inner backtick line must not flip the state.
        let tilde_wraps_backtick = "\
~~~
```
[[records/contacts/ghost]]
```
~~~

After [[records/companies/acme]].
";
        assert_eq!(
            extract_edge_targets(tilde_wraps_backtick),
            vec!["records/companies/acme".to_string()],
            "a ``` line inside a ~~~ block must not invert the fence state"
        );

        // (c) An over-indented ```` ``` ```` (4 spaces) is NOT a fence; the link
        // on the next line is live.
        let over_indented = "    ```\nLive [[records/contacts/sarah]].\n";
        assert_eq!(
            extract_edge_targets(over_indented),
            vec!["records/contacts/sarah".to_string()],
            "a >3-space-indented ``` is not a fence opener"
        );
    }

    #[test]
    fn canonical_link_target_strips_md_dotslash_and_trims() {
        assert_eq!(canonical_link_target("  records/x.md  "), "records/x");
        assert_eq!(canonical_link_target("./records/y"), "records/y");
        assert_eq!(canonical_link_target("/records/z"), "records/z");
    }

    #[test]
    fn link_edge_key_folds_case_only_on_case_insensitive_fs() {
        let a = link_edge_key("records/contacts/Sarah-Chen");
        let b = link_edge_key("records/contacts/sarah-chen");
        if fs_is_case_insensitive() {
            assert_eq!(a, b, "case-insensitive FS must fold the key");
        } else {
            assert_ne!(a, b, "case-sensitive FS must keep the key case-exact");
        }
    }

    #[test]
    fn link_edge_key_unifies_nfc_and_nfd_normalization_forms() {
        // REGRESSION (Unicode encoding / silent graph break): on macOS/APFS a
        // file written in one Unicode normalization form and a link written in
        // the other name the SAME file (the FS folds NFC/NFD), but their raw
        // bytes differ. The edge comparison key must fold them to one key on
        // every platform, or the graph (backlinks/forwardlinks/orphans) keys the
        // two as different targets and silently misses the edge.
        let nfc = "records/contacts/jos\u{00e9}"; // é = U+00E9 (NFC)
        let nfd = "records/contacts/jose\u{0301}"; // e + U+0301 (NFD)
                                                   // The two inputs are genuinely byte-different (the test would be vacuous
                                                   // otherwise).
        assert_ne!(nfc, nfd, "test inputs must be byte-distinct NFC vs NFD");
        assert_eq!(
            link_edge_key(nfc),
            link_edge_key(nfd),
            "NFC and NFD spellings of the same name must produce one edge key"
        );
    }

    // ── walk follows symlinked content ───────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn walk_includes_symlinked_content_file_and_symlinked_folder() {
        use std::os::unix::fs::symlink;
        let dir = empty_store();
        let root = dir.path();
        // A regular file (control).
        write(
            root,
            "records/contacts/sarah.md",
            &content_md("2026-05-01T00:00:00Z"),
        );
        // A symlinked .md content file inside a real folder.
        let external_file = root.join("external-elena.md");
        fs::write(&external_file, content_md("2026-05-02T00:00:00Z")).unwrap();
        symlink(&external_file, root.join("records/contacts/elena.md")).unwrap();
        // A symlinked type folder.
        let external_dir = dir.path().join("external-companies");
        fs::create_dir_all(&external_dir).unwrap();
        fs::write(
            external_dir.join("acme.md"),
            content_md("2026-05-03T00:00:00Z"),
        )
        .unwrap();
        symlink(&external_dir, root.join("records/companies")).unwrap();

        let store = open(&dir);
        let got = rels(&store.walk().unwrap());
        assert!(
            got.contains(&"records/contacts/elena.md".to_string()),
            "a symlinked content file must be walked: {got:?}"
        );
        assert!(
            got.contains(&"records/companies/acme.md".to_string()),
            "a file inside a symlinked type folder must be walked: {got:?}"
        );
    }

    // ── find_links_to: padded / fenced / case ────────────────────────────────

    #[test]
    fn find_links_to_matches_whitespace_padded_link() {
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "records/profiles/a.md",
            "---\ntype: profile\nmeta-type: conclusion\nsummary: s\n---\nSee [[ records/contacts/sarah ]] today.\n",
        );
        let store = open(&dir);
        let got = rels(
            &store
                .find_links_to(Path::new("records/contacts/sarah"))
                .unwrap(),
        );
        assert_eq!(
            got,
            vec!["records/profiles/a.md".to_string()],
            "a padded `[[ x ]]` link must be found as a backward edge, matching forwardlinks"
        );
    }

    #[test]
    fn find_links_to_ignores_fenced_example_link() {
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "records/concepts/howto.md",
            "---\ntype: concept\nmeta-type: conclusion\nsummary: s\n---\n```markdown\n[[records/contacts/sarah]]\n```\n",
        );
        let store = open(&dir);
        let got = store
            .find_links_to(Path::new("records/contacts/sarah"))
            .unwrap();
        assert!(
            got.is_empty(),
            "a `[[...]]` only inside a fenced code block is not a backward edge: {got:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn find_links_to_matches_case_variant_on_case_insensitive_fs() {
        // Only meaningful on a case-insensitive filesystem; on a case-sensitive
        // one the case-variant link is genuinely a different target.
        if !fs_is_case_insensitive() {
            return;
        }
        let dir = empty_store();
        let root = dir.path();
        write(
            root,
            "records/profiles/bio.md",
            "---\ntype: profile\nmeta-type: conclusion\nsummary: s\n---\nSee [[records/contacts/Sarah-Chen]].\n",
        );
        let store = open(&dir);
        let got = rels(
            &store
                .find_links_to(Path::new("records/contacts/sarah-chen"))
                .unwrap(),
        );
        assert_eq!(
            got,
            vec!["records/profiles/bio.md".to_string()],
            "a case-variant link must be found on a case-insensitive filesystem"
        );
    }
}
