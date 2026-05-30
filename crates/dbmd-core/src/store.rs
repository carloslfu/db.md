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
//! [`Store::find_links_to`] / [`Store::find_links_to_any`] (embedded ripgrep,
//! presence-only) and the `index.jsonl` sidecar readers
//! ([`Store::find_by_type`] / [`Store::find_by_where`] /
//! [`Store::read_type_index`]) — never a whole-store parse. The batch
//! [`Store::find_links_to_any`] is what keeps the working-set validate's
//! incoming-linker discovery a single store scan rather than one scan per
//! changed object.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, FixedOffset};
use grep::regex::RegexMatcher;
use grep::searcher::sinks::UTF8;
use grep::searcher::Searcher;
use ignore::WalkBuilder;

use crate::index::IndexRecord;
use crate::parser::{parse_db_md, Config, Frontmatter};

/// Basenames that are never content files: the config marker and the two
/// curator-maintained catalogs. The store walks skip these so a SWEEP over the
/// content layers never mistakes a catalog for a record.
const NON_CONTENT_BASENAMES: [&str; 3] = ["DB.md", "index.md", "log.md"];

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
/// declaration order (`Sources` < `Records` < `Wiki`) is the sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    /// `sources/` — raw evidence; immutable; date-sharded at scale.
    Sources,
    /// `records/` — atomic typed data; entity types flat, event types sharded.
    Records,
    /// `wiki/` — curator-synthesized narrative; flat.
    Wiki,
}

impl Layer {
    /// The on-disk folder name for this layer (`"sources"` / `"records"` /
    /// `"wiki"`).
    pub fn dir_name(self) -> &'static str {
        match self {
            Layer::Sources => "sources",
            Layer::Records => "records",
            Layer::Wiki => "wiki",
        }
    }

    /// Parse a layer from its folder name; `None` for anything else.
    pub fn from_dir_name(name: &str) -> Option<Self> {
        match name {
            "sources" => Some(Layer::Sources),
            "records" => Some(Layer::Records),
            "wiki" => Some(Layer::Wiki),
            _ => None,
        }
    }

    /// Every layer, in canonical order.
    pub fn all() -> [Layer; 3] {
        [Layer::Sources, Layer::Records, Layer::Wiki]
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

    /// Open `path` as a db.md store: confirm the `DB.md` marker (else
    /// [`NotAStore`]) and parse the `DB.md` config. Every store-walking
    /// subcommand opens through here.
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
    /// `sources/`, `records/`, and `wiki/`, skipping hidden dirs and `log/`.
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
    /// dedup-bounded entity types (`contact`/`company`/`decision`) and `wiki/`.
    pub fn type_shards(&self, type_: &str) -> bool {
        // Built-in classification. Sharding is a property of the *type*:
        //  - source types carry a primary date field and shard;
        //  - event record types track business volume and shard;
        //  - dedup-bounded entity types and curation-bounded wiki stay flat.
        // NOTE: the SPEC's `DB.md ## Schemas` `shard: by-date` override has no
        // representation in the frozen `Schema`/`FieldSpec` types (no shard
        // flag), so it cannot be consulted here yet — see the store findings.
        matches!(
            type_,
            // source types
            "email" | "transcript" | "pdf-source"
            // event record types (canonical)
            | "expense" | "invoice" | "meeting"
            // event record types (recognized custom, per the plan)
            | "order" | "ticket" | "transaction"
        )
    }

    /// Compute the canonical write path for a new file. For a sharding type
    /// (per [`Store::type_shards`]) insert `<YYYY>/<MM>/` from the type's
    /// primary date field (`email.date`, `expense.date`, … fallback `created`)
    /// under the type folder; flat types and `wiki/` get no shard segment.
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
    /// write surface honour an agent-supplied conforming sub-folder — e.g.
    /// `wiki/projects/`, `wiki/people/`, `wiki/synthesis/` (the SPEC files a
    /// `wiki-page` under `wiki/<topic>/`, i.e. ANY topic sub-folder, not only the
    /// `wiki/topics` default) — while still applying date-sharding for sharding
    /// types. The folder must be a conforming `<layer>/<type-folder>` (2
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
            // Flat type (entity records, wiki, decisions): no shard segment.
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

    /// Find files with an incoming wiki-link to `target`, via **embedded
    /// ripgrep** for `[[target]]` across all layers. Loop-fast; no whole-graph
    /// build. Returns store-relative paths.
    pub fn find_links_to(&self, target: &Path) -> Result<Vec<PathBuf>, StoreError> {
        // A single target is just the degenerate batch case — one alternation
        // arm, one store scan. Routing through `find_links_to_any` keeps the
        // pattern construction and the scan loop in exactly one place. The
        // batch API takes `&[PathBuf]`, so the one-element slice is owned (a
        // single alloc on this single-target convenience path; the batch path
        // validate.rs rides is untouched).
        self.find_links_to_any(&[target.to_path_buf()])
    }

    /// Find every file with an incoming wiki-link to **any** of `targets`, in a
    /// **single embedded-ripgrep pass** over the store (one `.md` walk, one
    /// presence-only scan per file). This is the batch incoming-linker finder the
    /// working-set [`crate::validate::validate_working_set`] sits on: it must find
    /// the linkers for the *whole* changed set without paying a full store read
    /// per changed object. Cost is therefore one store scan (O(store)), NOT
    /// `targets.len() × store` — calling [`find_links_to`](Self::find_links_to)
    /// in a loop would reread every `.md` once per target and is the exact
    /// `O(changed × store)` blow-up this method exists to prevent. Returns
    /// store-relative paths (deduped, sorted).
    ///
    /// Why content scan and not the sidecar `links` field: the sidecar projects
    /// only the frontmatter `links:` array, so it misses edges written in the
    /// body or in typed fields (`company: [[…]]`). Finding an incoming link to an
    /// arbitrary path therefore requires reading file content — the same reason
    /// the single-target finder uses ripgrep.
    pub fn find_links_to_any(&self, targets: &[PathBuf]) -> Result<Vec<PathBuf>, StoreError> {
        // The wiki-link doctrine: a link is the full store-relative path, no
        // `.md` extension. A reference to a target therefore appears literally
        // as `[[<target>]]`, optionally with a `|display` suffix and (warned
        // but accepted) a trailing `.md`. Build ONE regex that matches all
        // accepted spellings of an incoming link to ANY target, escaping each
        // target so path separators / dots stay literal and the alternation
        // arms keep their boundaries (a link to `sarah` never matches
        // `sarah-chen`).
        let mut arms: Vec<String> = Vec::new();
        for target in targets {
            let target_str = path_to_link_str(target);
            if target_str.is_empty() {
                continue;
            }
            // [[ <target> (.md)? ( | display )? ]]
            arms.push(format!(
                r"\[\[{}(\.md)?(\|[^\]]*)?\]\]",
                regex::escape(&target_str)
            ));
        }
        // No usable targets → no possible incoming links, and an empty pattern
        // would compile to a match-everything regex. Short-circuit instead.
        if arms.is_empty() {
            return Ok(Vec::new());
        }
        let pattern = arms.join("|");

        let matcher = RegexMatcher::new(&pattern).map_err(|e| StoreError::Search {
            root: self.root.clone(),
            message: format!("invalid backlink pattern: {e}"),
        })?;

        let mut hits = std::collections::BTreeSet::new();
        // Scan every `.md` file in the store (skip hidden + `log/`), including
        // `index.md` catalogs — an incoming reference is wherever the literal
        // link text lives; the caller decides relevance. ONE walk for the whole
        // target set; per file we stop at the first hit (presence is all we
        // need), so a file that links to several targets is read once, not once
        // per target.
        for rel in self.walk_all_md()? {
            let abs = self.abs_path(&rel);
            let mut matched_here = false;
            let mut searcher = Searcher::new();
            let res = searcher.search_path(
                &matcher,
                &abs,
                UTF8(|_lnum, _line| {
                    matched_here = true;
                    // Stop at the first hit: presence is all we need.
                    Ok(false)
                }),
            );
            if let Err(e) = res {
                return Err(StoreError::Search {
                    root: self.root.clone(),
                    message: format!("search failed in {}: {e}", abs.display()),
                });
            }
            if matched_here {
                hits.insert(rel);
            }
        }
        Ok(hits.into_iter().collect())
    }

    /// Candidate set for a `type` query: read the relevant type-folder
    /// `index.jsonl` sidecar(s) and return their records. Complete and
    /// cold-cache-proof — NOT a walk-and-parse or a frontmatter ripgrep scan,
    /// and **never a store-wide read**. The common path is one sequential read
    /// of the canonical type-folder sidecar (O(entities)); when that sidecar is
    /// absent the read is bounded to the type's single layer subtree
    /// (O(entities-in-layer)), so a `--type proposal` query before that folder
    /// has been indexed still stays inside the interactive loop's O(entities)
    /// contract instead of fanning out across every sidecar in the store.
    pub fn find_by_type(&self, type_: &str) -> Result<Vec<IndexRecord>, StoreError> {
        // Read the type's canonical-folder sidecar when it exists (the common,
        // O(entities) path). Otherwise fall back to the sidecars of the *one
        // layer* the type belongs to and filter by `type` — complete for records
        // filed under a non-canonical folder name within that layer (e.g. a
        // custom `proposal` filed in `records/proposals/` when the canonical
        // guess is the bare `records/proposal/`), without the whole-store
        // sidecar fan-out that would break the interactive loop's O(entities)
        // contract. A type lives in exactly one layer, and `default_type_folder`
        // always encodes it (recognized → its SPEC layer; unrecognized →
        // `records/`), so the fallback walk is bounded to that layer's subtree —
        // O(entities-in-layer), never O(store). Either way: sequential, complete
        // sidecar reads, never a walk-and-parse of the tree.
        let canonical_folder = default_type_folder(type_);
        let canonical = self.root.join(&canonical_folder).join(TYPE_INDEX_FILE);
        let records = if canonical.is_file() {
            self.read_type_index(&canonical)?
        } else {
            self.read_all_type_indexes_in(layer_of_folder(&canonical_folder))?
        };
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
    /// whole store (skip hidden + `log/`), returning store-relative paths. The
    /// walk root is `<root>/<layer>/` for a scoped read and `self.root` for the
    /// store-wide read; a non-existent layer subtree yields no sidecars rather
    /// than walking a missing path.
    fn find_type_index_files_in(&self, layer: Option<Layer>) -> Result<Vec<PathBuf>, StoreError> {
        let walk_root = match layer {
            Some(l) => self.root.join(l.dir_name()),
            None => self.root.clone(),
        };
        // A scoped walk over a layer folder that does not exist yet must be an
        // empty result, mirroring `walk_layer`'s missing-dir guard — not a walk
        // error from `ignore` over a nonexistent path.
        if !walk_root.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut builder = WalkBuilder::new(&walk_root);
        builder.standard_filters(false).hidden(true);
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
    /// `.gitignore`), but hidden files/dirs are skipped.
    fn md_walker(&self, root: &Path) -> WalkBuilder {
        let mut builder = WalkBuilder::new(root);
        builder.standard_filters(false).hidden(true);
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
        let value: serde_yml::Value = serde_yml::from_str(yaml).ok()?;
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

// ── Free helpers (no `self`) ────────────────────────────────────────────────

/// True if a walk entry is a regular file (not a dir / symlink-to-dir).
fn is_file_entry(entry: &ignore::DirEntry) -> bool {
    entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
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

/// Render a store-relative path as a wiki-link target string with `/`
/// separators (never `\`), no leading `./`, no trailing `.md`.
fn path_to_link_str(target: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in target.components() {
        if let std::path::Component::Normal(os) = comp {
            if let Some(s) = os.to_str() {
                parts.push(s.to_string());
            }
        }
    }
    let mut joined = parts.join("/");
    if let Some(stripped) = joined.strip_suffix(".md") {
        joined = stripped.to_string();
    }
    joined
}

/// The canonical default folder for a recognized type, per the SPEC type table
/// (`email → sources/emails`, `expense → records/expenses`, …). Unrecognized
/// types fall back to `records/<type>` (the bare type name, no pluralization
/// guess) — see the store findings on the docstring's looser `<type>` phrasing.
fn default_type_folder(type_: &str) -> PathBuf {
    let path = match type_ {
        // sources
        "email" => "sources/emails",
        "transcript" => "sources/transcripts",
        "pdf-source" => "sources/docs",
        // records — entities
        "contact" => "records/contacts",
        "company" => "records/companies",
        // records — events
        "expense" => "records/expenses",
        "meeting" => "records/meetings",
        "decision" => "records/decisions",
        "invoice" => "records/invoices",
        // wiki — the SPEC type table files a wiki-page under `wiki/<topic>/`,
        // i.e. ALWAYS a sub-folder, never flat under `wiki/`. A 2-component
        // `wiki/<file>` path is non-conforming: `index::type_folder_of` /
        // `validate::type_folder_of` require `<layer>/<type-folder>/<file>` (3
        // components), so a flat wiki page either crashes write-through
        // (`on_write` tries to create `index.md` *inside* a file) or is silently
        // dropped from every catalog by `rebuild_all`. `topic` is the page's
        // canonical bucket; with only the bare type in hand here, `wiki/topics`
        // is the deterministic default folder (matches the dogfood store).
        "wiki-page" => "wiki/topics",
        // unrecognized: bare type name under records/
        other => return PathBuf::from("records").join(other),
    };
    PathBuf::from(path)
}

/// The canonical [`Layer`] a `type_` belongs to, derived from its default
/// type-folder (`email` → `Sources`, `contact` → `Records`, `wiki-page` →
/// `Wiki`, unrecognized → `Records`). The write path uses this to decide whether
/// an agent-supplied folder is in the *right* layer for the type before honouring
/// its sub-folder choice.
pub fn layer_for_type(type_: &str) -> Layer {
    layer_of_folder(&default_type_folder(type_)).unwrap_or(Layer::Records)
}

/// The [`Layer`] a type-folder path lives in, read from its first component
/// (`sources/` → `Sources`, `records/` → `Records`, `wiki/` → `Wiki`). Used to
/// bound [`Store::find_by_type`]'s canonical-folder-absent fallback to a single
/// layer subtree. Returns `None` for a path with no recognized layer prefix;
/// every value [`default_type_folder`] produces has one, so in practice this is
/// always `Some` on the call path — `None` degrades to a store-wide read.
fn layer_of_folder(folder: &Path) -> Option<Layer> {
    let first = folder.components().next()?.as_os_str().to_str()?;
    Layer::from_dir_name(first)
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
/// while `default_type_folder("task")` → `records/task`). `wiki/<topic>` always
/// infers `wiki-page`, since every wiki page is filed under a topic folder.
pub fn infer_type_from_path(rel: &Path) -> Option<String> {
    let mut comps = rel.components().filter_map(|c| c.as_os_str().to_str());
    let layer = comps.next()?;
    if !matches!(layer, "sources" | "records" | "wiki") {
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
        ("records", "contacts") => "contact",
        ("records", "companies") => "company",
        ("records", "expenses") => "expense",
        ("records", "meetings") => "meeting",
        ("records", "decisions") => "decision",
        ("records", "invoices") => "invoice",
        // Every wiki page is filed under `wiki/<topic>/`; the type is always
        // `wiki-page` regardless of the topic-folder name.
        ("wiki", _) => "wiki-page",
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
        "expense" | "invoice" | "meeting" => Some("date"),
        // recognized custom event types have no canonical date field name; they
        // fall back to `created`.
        _ => None,
    }
}

/// Parse a YAML value into an RFC3339 [`DateTime`], accepting both an explicit
/// string and a YAML-native scalar rendered to string.
fn value_to_datetime(value: &serde_yml::Value) -> Option<DateTime<FixedOffset>> {
    let s = yaml_scalar_string(value)?;
    DateTime::parse_from_rfc3339(s.trim()).ok()
}

/// Extract `(YYYY, MM)` from a YAML date/timestamp value. Lenient: matches a
/// leading `YYYY-MM` so a bare `2026-05-22` date and a full
/// `2026-05-22T10:00:00-07:00` timestamp both work.
fn value_to_year_month(value: &serde_yml::Value) -> Option<(String, String)> {
    let s = yaml_scalar_string(value)?;
    year_month_from_str(s.trim())
}

/// `(YYYY, MM)` from the leading `YYYY-MM` of a date string.
fn year_month_from_str(s: &str) -> Option<(String, String)> {
    // Hand-roll the leading-`YYYY-MM` parse to avoid a regex compile on the
    // write path. Require: 4 digits, '-', 2 digits.
    let bytes = s.as_bytes();
    if bytes.len() < 7 {
        return None;
    }
    let is_digit = |b: u8| b.is_ascii_digit();
    if !(is_digit(bytes[0])
        && is_digit(bytes[1])
        && is_digit(bytes[2])
        && is_digit(bytes[3])
        && bytes[4] == b'-'
        && is_digit(bytes[5])
        && is_digit(bytes[6]))
    {
        return None;
    }
    let month: u8 = (bytes[5] - b'0') * 10 + (bytes[6] - b'0');
    if !(1..=12).contains(&month) {
        return None;
    }
    Some((s[0..4].to_string(), s[5..7].to_string()))
}

/// Render a YAML scalar as a string: a real `String` verbatim, otherwise the
/// value's compact YAML serialization (covers timestamps that the YAML engine
/// may surface as a non-string scalar).
fn yaml_scalar_string(value: &serde_yml::Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    match value {
        serde_yml::Value::Null => None,
        serde_yml::Value::Mapping(_) | serde_yml::Value::Sequence(_) => None,
        other => serde_yml::to_string(other)
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
    // First line must be exactly `---` (allowing trailing CR).
    let (first, after_first) = split_first_line(rest);
    if first.trim_end_matches('\r') != "---" {
        return None;
    }
    rest = after_first;
    let block_start = rest;
    let mut scanned = 0usize;
    loop {
        let (line, after) = split_first_line(rest);
        if line.trim_end_matches('\r') == "---" {
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

/// Compare a JSON field value against a query string. A string matches
/// verbatim; scalars match their textual form; an array matches if any element
/// matches (so a list-valued frontmatter field is membership-queried).
fn json_value_matches(v: &serde_json::Value, value: &str) -> bool {
    match v {
        serde_json::Value::String(s) => s == value,
        serde_json::Value::Bool(b) => b.to_string() == value,
        serde_json::Value::Number(n) => n.to_string() == value,
        serde_json::Value::Array(items) => items.iter().any(|i| json_value_matches(i, value)),
        serde_json::Value::Null => value.is_empty(),
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
        assert_eq!(Layer::Wiki.dir_name(), "wiki");
        assert_eq!(Layer::from_dir_name("log"), None);
        assert_eq!(Layer::from_dir_name("Sources"), None); // case-sensitive
    }

    #[test]
    fn layer_order_is_canonical() {
        // stats keys a BTreeMap on Layer; the sort order must be sources<records<wiki.
        let mut v = [Layer::Wiki, Layer::Sources, Layer::Records];
        v.sort();
        assert_eq!(v, [Layer::Sources, Layer::Records, Layer::Wiki]);
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
            "wiki/people/sarah.md",
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
                "sources/emails/2026/05/a.md".to_string(),
                "wiki/people/sarah.md".to_string(),
            ]
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
        // A layer with no directory is empty, not an error.
        assert!(store.walk_layer(Layer::Wiki).unwrap().is_empty());
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
            "contact",
            "company",
            "decision",
            "wiki-page",
            "index",
            "log",
            "db-md",
            "proposal",
        ] {
            assert!(!store.type_shards(t), "{t} should stay flat");
        }
    }

    // ── shard_path_for ───────────────────────────────────────────────────────

    fn fm_with_extra(key: &str, value: &str) -> Frontmatter {
        let mut fm = Frontmatter::default();
        fm.extra
            .insert(key.to_string(), serde_yml::Value::String(value.to_string()));
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
        fm.extra
            .insert("date".into(), serde_yml::Value::String("2026-05-22".into()));
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

        // wiki-page is flat (no date shard) but still files under a type-folder:
        // `wiki/topics/<name>.md`, NEVER flat as `wiki/<name>.md`. A 2-component
        // path is invisible to the index/validate type-folder model.
        let p = store
            .shard_path_for("wiki-page", &Frontmatter::default(), "renewal-theme")
            .unwrap();
        assert_eq!(p, PathBuf::from("wiki/topics/renewal-theme.md"));
    }

    /// Regression: a wiki-page written through the toolkit's own path
    /// computation must land at a path the index + validate type-folder model
    /// accepts. `shard_path_for("wiki-page", …)` previously returned a
    /// 2-component `wiki/<file>` path, which `type_folder_of` (in both `index`
    /// and `validate`) treats as "no type-folder" — so the page either crashed
    /// `Index::on_write` (it tried to create `index.md` inside a file) or was
    /// silently dropped from every catalog by `Index::rebuild_all`. The
    /// computed path must have 3 components: `<layer>/<type-folder>/<file>`.
    #[test]
    fn shard_path_wiki_page_is_indexable_three_component_path() {
        let dir = empty_store();
        let store = open(&dir);
        let p = store
            .shard_path_for("wiki-page", &Frontmatter::default(), "renewal-theme")
            .unwrap();
        // First two components are a layer + a non-empty type-folder segment;
        // the file is the third. This is exactly the shape `type_folder_of`
        // (`comps.len() >= 3`, `comps[0]` a known layer) requires.
        let comps: Vec<&str> = p.iter().filter_map(|c| c.to_str()).collect();
        assert_eq!(
            comps.len(),
            3,
            "wiki-page path must be <layer>/<type-folder>/<file>, got {p:?}"
        );
        assert_eq!(comps[0], "wiki", "first component must be the wiki layer");
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
            "wiki/people/sarah.md",
            &format!("---\ntype: wiki-page\nsummary: s\n---\nSee [[{target}]].\n"),
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
            "wiki/themes/t.md",
            &format!("---\ntype: wiki-page\nsummary: s\n---\n[[{target}.md]]\n"),
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
            "wiki/people/elena.md",
            "---\ntype: wiki-page\nsummary: s\n---\nNo links here.\n",
        );
        // Short-form link must NOT match the full-path target.
        write(
            root,
            "wiki/people/bob.md",
            "---\ntype: wiki-page\nsummary: s\n---\n[[sarah-chen]]\n",
        );
        // A longer path that merely starts with the target must NOT match
        // (boundary correctness): target `sarah-chen` vs `sarah-chen-jr`.
        write(
            root,
            "wiki/people/jr.md",
            &format!("---\ntype: wiki-page\nsummary: s\n---\n[[{target}-jr]]\n"),
        );

        let store = open(&dir);
        let got = rels(&store.find_links_to(Path::new(target)).unwrap());
        assert_eq!(
            got,
            vec![
                "records/contacts/index.md".to_string(),
                "records/meetings/2026/05/m.md".to_string(),
                "wiki/people/sarah.md".to_string(),
                "wiki/themes/t.md".to_string(),
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
            "wiki/a.md",
            "---\ntype: wiki-page\nsummary: s\n---\n[[records/contacts/sarah]]\n",
        );
        write(
            root,
            "wiki/b.md",
            "---\ntype: wiki-page\nsummary: s\n---\n[[records/contacts/sarah-chen]]\n",
        );
        let store = open(&dir);

        assert_eq!(
            rels(
                &store
                    .find_links_to(Path::new("records/contacts/sarah"))
                    .unwrap()
            ),
            vec!["wiki/a.md".to_string()]
        );
        assert_eq!(
            rels(
                &store
                    .find_links_to(Path::new("records/contacts/sarah-chen"))
                    .unwrap()
            ),
            vec!["wiki/b.md".to_string()]
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
            "wiki/links-sarah.md",
            "---\ntype: wiki-page\nsummary: s\n---\n[[records/contacts/sarah-chen]]\n",
        );
        write(
            root,
            "wiki/links-acme.md",
            "---\ntype: wiki-page\nsummary: s\n---\nDeal with [[records/companies/acme|Acme]].\n",
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
            "wiki/links-jr.md",
            "---\ntype: wiki-page\nsummary: s\n---\n[[records/contacts/sarah-chen-jr]]\n",
        );
        // A file that links to neither requested target.
        write(
            root,
            "wiki/unrelated.md",
            "---\ntype: wiki-page\nsummary: s\n---\n[[wiki/themes/spend]]\n",
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
                "records/meetings/2026/05/m.md".to_string(),
                "wiki/links-acme.md".to_string(),
                "wiki/links-sarah.md".to_string(),
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
            "wiki/a.md",
            "---\ntype: wiki-page\nsummary: s\n---\n[[records/contacts/sarah-chen]]\n",
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

        // `wiki/` was never created.
        let in_wiki = store
            .find_by_where_in("city", "denver", Some(Layer::Wiki))
            .expect("missing layer subtree is empty, not an error");
        assert!(in_wiki.is_empty());

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
            ("records/contacts/x.md", "contact"),
            ("records/companies/x.md", "company"),
            ("records/expenses/x.md", "expense"),
            ("records/meetings/x.md", "meeting"),
            ("records/decisions/x.md", "decision"),
            ("records/invoices/x.md", "invoice"),
            // Any wiki sub-folder infers `wiki-page` regardless of the topic name.
            ("wiki/topics/x.md", "wiki-page"),
            ("wiki/pricing/x.md", "wiki-page"),
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
        // `wiki-page` is the one many-to-one case (every topic folder maps back
        // to `wiki-page`), so its forward folder still round-trips.
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
            "wiki-page",
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
        assert_eq!(infer_type_from_path(Path::new("wiki/x.md")), None);
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
}
