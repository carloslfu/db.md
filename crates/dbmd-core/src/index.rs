//! `index` — the hierarchical content catalog.
//!
//! A uniform three-level tree: root + per-layer + per-type-folder. **Two
//! artifacts per type-folder:** the human `index.md` (capped 500, recency
//! browse) and the machine `index.jsonl` (complete, structured — one JSON
//! object per file). Both read `summary` + key frontmatter fields + links
//! directly from each file — there is no extraction logic here.
//!
//! **Maintained write-through** by the write commands ([`Index::on_write`] /
//! [`Index::on_rename`] / [`Index::on_remove`] — the loop path, O(changed), no
//! store walk); [`Index::rebuild_all`] is the from-scratch SWEEP repair.
//!
//! **Key invariant:** write-through must produce a byte-identical `index.md`
//! and (post-compaction) `index.jsonl` to a full [`Index::rebuild_all`] over
//! the same end state — the loop path can never drift from the repair path.
//!
//! # Implementation notes (deviations the reader should know)
//!
//! - **Self-contained, by design.** This module does its own shard-aware folder
//!   walk, its own minimal frontmatter read, and its own atomic write, using
//!   only `store.root` (a public field) and the `serde_norway` / `serde_json` /
//!   `chrono` / `walkdir` crates rather than routing through the sibling
//!   `store`/`parser` helpers ([`Store::walk_type_folder`],
//!   [`Store::recent_in_type_folder`], [`parser::read_file`], …). The index has
//!   to stamp a *deterministic* `updated:` and emit a *canonical, compacted*
//!   `index.jsonl` (see the two notes below); keeping the read/walk/write local
//!   is what makes the byte-identity invariant a true byte comparison, free of
//!   any incidental formatting the shared readers might introduce. The public
//!   signatures in `lib.rs` are untouched.
//! - **Deterministic `updated:` on the index files themselves.** An index's own
//!   `updated` frontmatter is derived as the max `updated` over the files it
//!   catalogs (max over children for root/layer) — NOT wall-clock-now. This is
//!   what makes the byte-identity invariant a *true* byte comparison: a
//!   write-through write and a `rebuild_all` over the same end state stamp the
//!   same value. (The SPEC's rendered examples show a wall-clock-looking value;
//!   the conventions list only requires `updated: <RFC3339>`, and the
//!   property-tested invariant dominates.)
//! - **`index.jsonl` is always compacted.** Write-through rewrites the affected
//!   type-folder's jsonl in canonical form (one current line per path, recency
//!   order) rather than appending superseded/tombstone lines, so the jsonl is
//!   byte-identical to `rebuild_all` *immediately* (a strictly stronger
//!   guarantee than the SPEC's "post-compaction"). This keeps the loop cost at
//!   one sidecar read + one rewrite per touched type-folder — O(folder), the
//!   sanctioned loop primitive, never a whole-`Store::walk`.
//! - **Root/layer entry styling** follows plan §index (`(N)` numeric counts;
//!   layer headings in the root carry the layer's total count) which is more
//!   specific than the SPEC's illustrative `(42 files)` prose example. Type
//!   folders are listed alphabetically (a deterministic order a derived artifact
//!   needs); `scope: type-folder` follows the conventions list, not the one
//!   SPEC example that wrote `scope: folder`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, SecondsFormat};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::parser::FolderMeta;
use crate::store::{Layer, Store};

/// The browse-view cap for a type-folder `index.md`.
const MD_CAP: usize = 500;

/// Placeholder summary for a content file that has no `summary` frontmatter.
/// The index never invents a real summary — that is `dbmd fm init`'s job; this
/// marker is what `dbmd validate` keys off (`INDEX`-class issue).
const MISSING_SUMMARY: &str = "(no summary)";

/// The root `index.md` H1.
const ROOT_TITLE: &str = "Knowledge base index";

/// Which level of the catalog an [`Index`] represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexLevel {
    /// The store-wide root `index.md` (layers + per-type counts).
    Root,
    /// A layer `index.md` (every type-folder under one layer).
    Layer(Layer),
    /// A type-folder `index.md` + `index.jsonl` (every file in the folder).
    TypeFolder(PathBuf),
}

/// One record in a type-folder's `index.jsonl` — the complete, structured twin
/// of a single `index.md` browse entry.
///
/// `tags` are the document's flat labels; `links` are its concept/relationship
/// wiki-link targets. Both are copied verbatim from the file — never inferred.
/// `fields` holds the remaining type-specific frontmatter so the structured
/// query path can filter on any key without opening the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexRecord {
    /// Store-relative path of the file (the upsert key; last-write-wins).
    /// Serialized with forward slashes regardless of OS (see [`path_serde`]) so
    /// the `index.jsonl` catalog is byte-portable across platforms.
    #[serde(with = "path_serde")]
    pub path: PathBuf,
    /// The file's `type`.
    #[serde(rename = "type")]
    pub type_: String,
    /// The file's `summary`.
    pub summary: String,
    /// The file's flat `tags`.
    #[serde(default)]
    pub tags: Vec<String>,
    /// The file's concept/relationship wiki-link targets (store-relative).
    #[serde(default)]
    pub links: Vec<String>,
    /// `created` timestamp.
    pub created: Option<DateTime<FixedOffset>>,
    /// `updated` timestamp (the recency key for the `index.md` cap order).
    pub updated: Option<DateTime<FixedOffset>>,
    /// Remaining type-specific frontmatter fields, verbatim.
    #[serde(flatten)]
    pub fields: BTreeMap<String, Value>,
}

/// A built (or being-built) catalog for one [`IndexLevel`], with both rendered
/// artifacts available. Pure data until written via [`Index::write_level`].
#[derive(Debug, Clone, PartialEq)]
pub struct Index {
    /// Which level this catalog is for.
    pub level: IndexLevel,
    /// The complete record set for this level (type-folder level; empty for
    /// root/layer rollups, which carry only counts).
    pub records: Vec<IndexRecord>,
    /// Per-child counts for root/layer rollups (child path → file count).
    pub child_counts: BTreeMap<PathBuf, usize>,
}

impl Index {
    /// Build a type-folder catalog by aggregating across date-shards, producing
    /// both artifacts. `index.md` selection is recency (updated desc, ties by
    /// path asc; cap 500 with a `## More` footer over the cap); `index.jsonl`
    /// holds every file. A file missing `summary` gets a placeholder + a
    /// validate-detectable issue (the index never invents summaries).
    pub fn build_type_folder(store: &Store, type_folder: &Path) -> crate::Result<Index> {
        let rel = normalize_rel(type_folder);
        let abs = store.root.join(&rel);
        let mut records = Vec::new();
        for file_abs in walk_type_folder_files(&abs) {
            let rel_path =
                rel_to_store(&store.root, &file_abs).expect("walked file is under the store root");
            // Abort the build on a malformed file rather than skip it. A skipped
            // file would still be a content member the validator requires to be
            // catalogued (`validate::walk_content_files` enumerates by filename,
            // not by parseability), so silently dropping it would leave the store
            // in a permanently invalid state (`INDEX_MISSING_ENTRY` /
            // `INDEX_JSONL_DESYNC` that no rebuild can clear) and would desync the
            // rollups (`build_layer`/`build_root` count the raw `.md` files). The
            // loud `?` is the right outcome: `cleanup` now preserves the prior
            // canonical sidecars (`min_depth(2)`), so an aborted rebuild leaves
            // the existing catalogs intact and the operator a clear error naming
            // the file to fix — never a destroyed or silently-wrong index.
            records.push(record_from_file(&file_abs, rel_path)?);
        }
        sort_records(&mut records);
        Ok(Index {
            level: IndexLevel::TypeFolder(rel),
            records,
            child_counts: BTreeMap::new(),
        })
    }

    /// Build a layer catalog: every non-empty type-folder under the layer with
    /// `(N)` counts and a newest-file `summary` preview (≤ 80 chars), plus the
    /// **loose records** that live directly at the layer root (files with no
    /// type-folder between them and the layer). The type-folder rollup is the
    /// `index.md`; the loose records are the layer's own `index.jsonl` (so
    /// structured reads — `query`, dedup, `graph` — see a loose file the same
    /// way they see a canonical one). A layer with no loose files carries no
    /// `index.jsonl`, so existing stores are byte-unchanged.
    pub fn build_layer(store: &Store, layer: Layer) -> crate::Result<Index> {
        let mut child_counts = BTreeMap::new();
        for tf in type_folders_in_layer(store, layer) {
            let abs = store.root.join(&tf);
            let n = walk_type_folder_files(&abs).len();
            if n > 0 {
                child_counts.insert(tf, n);
            }
        }
        let mut records = Vec::new();
        for file_abs in loose_files_in_layer(store, layer) {
            let rel_path =
                rel_to_store(&store.root, &file_abs).expect("walked file is under the store root");
            // Abort on a malformed loose file rather than skip it, mirroring
            // `build_type_folder`: a skipped file is still a content member the
            // validator requires to be catalogued, so dropping it would leave a
            // permanently-invalid index. The loud `?` names the file to fix.
            records.push(record_from_file(&file_abs, rel_path)?);
        }
        sort_records(&mut records);
        Ok(Index {
            level: IndexLevel::Layer(layer),
            records,
            child_counts,
        })
    }

    /// Build the store-wide root catalog: one heading per non-empty layer with
    /// total count + bulleted per-type sub-entries with `(N)` counts.
    pub fn build_root(store: &Store) -> crate::Result<Index> {
        let mut child_counts = BTreeMap::new();
        for layer in Layer::all() {
            for tf in type_folders_in_layer(store, layer) {
                let abs = store.root.join(&tf);
                let n = walk_type_folder_files(&abs).len();
                if n > 0 {
                    child_counts.insert(tf, n);
                }
            }
        }
        Ok(Index {
            level: IndexLevel::Root,
            records: Vec::new(),
            child_counts,
        })
    }

    /// Render this catalog as a canonical `index.md`.
    pub fn to_markdown(&self) -> String {
        match &self.level {
            IndexLevel::TypeFolder(folder) => self.render_type_folder_md(folder),
            IndexLevel::Layer(layer) => self.render_layer_md(*layer),
            IndexLevel::Root => self.render_root_md(),
        }
    }

    /// Render this catalog's `records` as the complete `index.jsonl` (one JSON
    /// object per file, stable key order so diffs stay minimal). Used at the
    /// type-folder level for its files, and at the layer level for the loose
    /// files that live directly at the layer root. The root rollup carries no
    /// records, so it never produces a jsonl.
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for rec in &self.records {
            // The record type derives a deterministic, sorted key order
            // (declared fields first, then the flattened `fields` BTreeMap).
            let line = serde_json::to_string(rec).expect("IndexRecord serializes");
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    // ── rendering helpers ────────────────────────────────────────────────

    fn render_type_folder_md(&self, folder: &Path) -> String {
        let folder_disp = path_to_unix(folder);
        let updated = max_updated(self.records.iter().map(|r| r.updated.as_ref()));
        let mut s = String::new();
        s.push_str("---\n");
        s.push_str("type: index\n");
        s.push_str("scope: type-folder\n");
        s.push_str(&format!("folder: {folder_disp}\n"));
        if let Some(ts) = updated {
            s.push_str(&format!("updated: {}\n", fmt_ts(&ts)));
        }
        s.push_str("---\n\n");
        s.push_str(&format!("# {folder_disp}\n\n"));

        let shown = self.records.len().min(MD_CAP);
        for rec in self.records.iter().take(shown) {
            s.push_str(&format_md_entry(rec));
            s.push('\n');
        }

        if self.records.len() > MD_CAP {
            let type_ = self.records.first().map(|r| r.type_.as_str()).unwrap_or("");
            let layer = folder
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .unwrap_or("");
            s.push('\n');
            s.push_str(&more_footer(self.records.len(), type_, layer));
        }
        s
    }

    /// Store-less layer rollup: counts only, no preview / no derived `updated`
    /// (a layer index needs each child's on-disk jsonl for those — see
    /// [`render_layer_md_with_store`], the canonical path every disk write
    /// uses). This pure-data render is structurally identical sans preview.
    fn render_layer_md(&self, layer: Layer) -> String {
        let layer_dir = layer_dir_name(layer);
        let mut s = String::new();
        s.push_str("---\n");
        s.push_str("type: index\n");
        s.push_str("scope: layer\n");
        s.push_str(&format!("folder: {layer_dir}\n"));
        s.push_str("---\n\n");
        s.push_str(&format!("# {layer_dir}\n\n"));
        for (tf, n) in &self.child_counts {
            let tf_unix = path_to_unix(tf);
            let display = capitalize(folder_basename(tf));
            s.push_str(&format!("- [[{tf_unix}/index|{display}]] ({n})\n"));
        }
        s
    }

    /// Store-less root rollup: counts only (the canonical disk render adds a
    /// derived `updated` — see [`render_root_md_with_store`]).
    fn render_root_md(&self) -> String {
        let mut s = String::new();
        s.push_str("---\n");
        s.push_str("type: index\n");
        s.push_str("scope: root\n");
        s.push_str("---\n\n");
        s.push_str(&format!("# {ROOT_TITLE}\n"));
        for layer in Layer::all() {
            let layer_dir = layer_dir_name(layer);
            let prefix = format!("{layer_dir}/");
            let children: Vec<(&PathBuf, &usize)> = self
                .child_counts
                .iter()
                .filter(|(tf, _)| path_to_unix(tf).starts_with(&prefix))
                .collect();
            if children.is_empty() {
                continue;
            }
            let total: usize = children.iter().map(|(_, n)| **n).sum();
            s.push('\n');
            s.push_str(&format!("## {} ({total})\n", capitalize(layer_dir)));
            for (tf, n) in children {
                let tf_unix = path_to_unix(tf);
                let display = capitalize(folder_basename(tf));
                s.push_str(&format!("- [[{tf_unix}/index|{display}]] ({n})\n"));
            }
        }
        s
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Write-through + sweep (free functions on the impl block).
// ─────────────────────────────────────────────────────────────────────────

impl Index {
    /// **Write-through (loop, O(changed)).** Upsert a new/updated content file.
    /// Reads the affected type-folder's `index.jsonl` (the sanctioned per-folder
    /// sidecar read — never a whole-store walk), applies the change, and
    /// atomically rewrites that folder's `index.md` + `index.jsonl` plus the
    /// parent layer + root rollups so the artifacts equal a `rebuild_all` over
    /// the same end state.
    pub fn on_write(store: &Store, file: &Path) -> crate::Result<()> {
        let file_rel = normalize_rel(file);
        // The generated catalog files are not content — never upsert one into
        // itself. `build_type_folder`'s walk already excludes `index.md`
        // (`walk_type_folder_files`); the loop path must apply the same
        // exclusion or editing `index.md` via `fm set` inserts a phantom
        // self-row, inflating every `(N)` count and breaking the
        // write-through == rebuild byte-identity invariant.
        if is_index_artifact(&file_rel) {
            return Ok(());
        }
        // A loose file (directly at a layer root, no type-folder) is catalogued
        // in its layer's own `index.jsonl`; the layer `index.md` rollup is
        // unaffected (loose files do not change type-folder counts).
        if let Some(layer) = loose_layer_of(&file_rel) {
            return apply_loose_change(store, layer, &file_rel, false);
        }
        let file_abs = store.root.join(&file_rel);
        let folder = type_folder_of(&file_rel)
            .ok_or_else(|| bad_index(&file_rel, "file is not inside a layer/type-folder"))?;
        let record = record_from_file(&file_abs, file_rel.clone())?;

        // Serialize the sidecar read-modify-write so concurrent sanctioned
        // writes to this folder don't clobber each other's rows (lost update).
        let _lock = FolderLock::acquire(&store.root.join(&folder));
        let mut records = read_jsonl_records(&store.root.join(&folder).join("index.jsonl"))?;
        records.retain(|r| r.path != record.path);
        records.push(record);
        sort_records(&mut records);

        write_type_folder_artifacts(store, &folder, &records)?;
        update_parents(store, &folder)?;
        Ok(())
    }

    /// **Write-through (loop, O(changed)).** Move a file's entry between
    /// type-folder indexes (or within, if the same folder) in both `index.md`
    /// and `index.jsonl`, fixing counts on both sides.
    pub fn on_rename(store: &Store, old: &Path, new: &Path) -> crate::Result<()> {
        let old_rel = normalize_rel(old);
        let new_rel = normalize_rel(new);
        // Index artifacts are generated, not catalogued — a rename of/into one
        // is not a content move (same reasoning as `on_write`). Skip rather than
        // insert a phantom self-row.
        if is_index_artifact(&old_rel) || is_index_artifact(&new_rel) {
            return Ok(());
        }
        // If either side is a loose file (layer root, no type-folder), decompose
        // into remove-old + add-new: each entry point routes to the correct
        // catalog (the layer `index.jsonl` for a loose side, the type-folder for
        // the other), giving the same end state as the cross-folder path below
        // while reusing the tested single-file paths.
        if loose_layer_of(&old_rel).is_some() || loose_layer_of(&new_rel).is_some() {
            Self::on_remove(store, &old_rel)?;
            Self::on_write(store, &new_rel)?;
            return Ok(());
        }
        let old_folder = type_folder_of(&old_rel)
            .ok_or_else(|| bad_index(&old_rel, "source is not inside a layer/type-folder"))?;
        let new_folder = type_folder_of(&new_rel)
            .ok_or_else(|| bad_index(&new_rel, "target is not inside a layer/type-folder"))?;

        // Serialize the sidecar read-modify-write(s). For a cross-folder rename,
        // lock BOTH folders, always in sorted order, so two renames touching the
        // same pair can't deadlock. Held for the whole operation via RAII.
        let _locks = lock_folders(store, &old_folder, &new_folder);

        // Drop from the old folder.
        let mut old_records =
            read_jsonl_records(&store.root.join(&old_folder).join("index.jsonl"))?;
        old_records.retain(|r| r.path != old_rel);

        if old_folder == new_folder {
            // Same folder: re-read the (now-renamed) file and upsert.
            let record = record_from_file(&store.root.join(&new_rel), new_rel.clone())?;
            old_records.retain(|r| r.path != record.path);
            old_records.push(record);
            sort_records(&mut old_records);
            write_type_folder_artifacts(store, &old_folder, &old_records)?;
            update_parents(store, &old_folder)?;
            return Ok(());
        }

        // Cross-folder: write the trimmed old folder (or drop its indexes if
        // now empty), then upsert into the new folder.
        sort_records(&mut old_records);
        write_type_folder_artifacts(store, &old_folder, &old_records)?;

        let record = record_from_file(&store.root.join(&new_rel), new_rel.clone())?;
        let mut new_records =
            read_jsonl_records(&store.root.join(&new_folder).join("index.jsonl"))?;
        new_records.retain(|r| r.path != record.path);
        new_records.push(record);
        sort_records(&mut new_records);
        write_type_folder_artifacts(store, &new_folder, &new_records)?;

        update_parents(store, &old_folder)?;
        update_parents(store, &new_folder)?;
        Ok(())
    }

    /// **Write-through (loop, O(changed)).** Drop a file's entry from both
    /// `index.md` and `index.jsonl`; decrement counts; if the browse view drops
    /// below the cap, the next-most-recent is already present in the complete
    /// jsonl record set and re-renders into the md automatically.
    pub fn on_remove(store: &Store, file: &Path) -> crate::Result<()> {
        let file_rel = normalize_rel(file);
        // Removing a generated catalog artifact is not a content removal; it has
        // no row to drop (it was never catalogued). Skip, mirroring `on_write`.
        if is_index_artifact(&file_rel) {
            return Ok(());
        }
        // Loose file → drop its row from the layer `index.jsonl`.
        if let Some(layer) = loose_layer_of(&file_rel) {
            return apply_loose_change(store, layer, &file_rel, true);
        }
        let folder = type_folder_of(&file_rel)
            .ok_or_else(|| bad_index(&file_rel, "file is not inside a layer/type-folder"))?;
        // Serialize the sidecar read-modify-write (see `on_write`).
        let _lock = FolderLock::acquire(&store.root.join(&folder));
        let mut records = read_jsonl_records(&store.root.join(&folder).join("index.jsonl"))?;
        let before = records.len();
        records.retain(|r| r.path != file_rel);
        if records.len() == before {
            // Nothing to remove; still normalize the folder + parents so the
            // artifacts stay canonical.
        }
        sort_records(&mut records);
        write_type_folder_artifacts(store, &folder, &records)?;
        update_parents(store, &folder)?;
        Ok(())
    }

    /// **SWEEP repair.** Walk the store once and atomically (re)write root +
    /// every non-empty layer + every non-empty type-folder `index.md` and
    /// `index.jsonl` (compacting the jsonl). Also runs [`Index::cleanup`].
    pub fn rebuild_all(store: &Store) -> crate::Result<()> {
        Index::cleanup(store)?;
        for layer in Layer::all() {
            for tf in type_folders_in_layer(store, layer) {
                let idx = Index::build_type_folder(store, &tf)?;
                if idx.records.is_empty() {
                    continue;
                }
                write_type_folder_artifacts(store, &tf, &idx.records)?;
            }
            let layer_idx = Index::build_layer(store, layer)?;
            let layer_index_md = store.root.join(layer_dir_name(layer)).join("index.md");
            if layer_idx.child_counts.is_empty() {
                remove_if_exists(&layer_index_md)?;
            } else {
                write_atomic(
                    &layer_index_md,
                    render_layer_md_with_store(store, &layer_idx),
                )?;
            }
            // The layer's own `index.jsonl` — present iff the layer has loose
            // files directly at its root. Independent of the rollup above: a
            // layer can have loose files but no type-folders, or vice versa.
            write_layer_jsonl(store, layer, &layer_idx.records)?;
        }
        let root_idx = Index::build_root(store)?;
        let root_index_md = store.root.join("index.md");
        if root_idx.child_counts.is_empty() {
            remove_if_exists(&root_index_md)?;
        } else {
            write_atomic(&root_index_md, render_root_md_with_store(store, &root_idx))?;
        }
        Ok(())
    }

    /// Rebuild ONE type-folder's `index.md`/`index.jsonl` from a fresh walk, then
    /// cascade the new child count up to the layer and root rollups — so a
    /// scoped `dbmd index rebuild --folder` leaves the hierarchy consistent,
    /// exactly like `rebuild_all` and the loop-path `on_write` already do.
    /// (Writing only the folder, as the CLI used to, left stale layer/root
    /// counts that `validate` would then flag as an index desync.)
    pub fn rebuild_folder(store: &Store, folder: &Path) -> crate::Result<()> {
        Self::write_level(store, &IndexLevel::TypeFolder(folder.to_path_buf()))?;
        update_parents(store, folder)
    }

    /// Atomically write a single level's artifact(s) to disk.
    pub fn write_level(store: &Store, level: &IndexLevel) -> crate::Result<()> {
        match level {
            IndexLevel::TypeFolder(folder) => {
                let idx = Index::build_type_folder(store, folder)?;
                if idx.records.is_empty() {
                    remove_if_exists(&store.root.join(folder).join("index.md"))?;
                    remove_if_exists(&store.root.join(folder).join("index.jsonl"))?;
                } else {
                    write_type_folder_artifacts(store, folder, &idx.records)?;
                }
            }
            IndexLevel::Layer(layer) => {
                let idx = Index::build_layer(store, *layer)?;
                let p = store.root.join(layer_dir_name(*layer)).join("index.md");
                if idx.child_counts.is_empty() {
                    remove_if_exists(&p)?;
                } else {
                    write_atomic(&p, render_layer_md_with_store(store, &idx))?;
                }
                write_layer_jsonl(store, *layer, &idx.records)?;
            }
            IndexLevel::Root => {
                let idx = Index::build_root(store)?;
                let p = store.root.join("index.md");
                if idx.child_counts.is_empty() {
                    remove_if_exists(&p)?;
                } else {
                    write_atomic(&p, render_root_md_with_store(store, &idx))?;
                }
            }
        }
        Ok(())
    }

    /// Render the generated indexes to a string with `--- <path> ---`
    /// separators instead of writing them (`--dry-run`).
    pub fn render_dry_run(store: &Store, level: &IndexLevel) -> crate::Result<String> {
        let mut out = String::new();
        match level {
            IndexLevel::TypeFolder(folder) => {
                let idx = Index::build_type_folder(store, folder)?;
                let md_path = path_to_unix(&folder.join("index.md"));
                let jsonl_path = path_to_unix(&folder.join("index.jsonl"));
                out.push_str(&format!("--- {md_path} ---\n"));
                out.push_str(&idx.to_markdown());
                out.push_str(&format!("--- {jsonl_path} ---\n"));
                out.push_str(&idx.to_jsonl());
            }
            IndexLevel::Layer(layer) => {
                let idx = Index::build_layer(store, *layer)?;
                let md_path = format!("{}/index.md", layer_dir_name(*layer));
                out.push_str(&format!("--- {md_path} ---\n"));
                out.push_str(&render_layer_md_with_store(store, &idx));
            }
            IndexLevel::Root => {
                let idx = Index::build_root(store)?;
                out.push_str("--- index.md ---\n");
                out.push_str(&render_root_md_with_store(store, &idx));
            }
        }
        Ok(out)
    }

    /// Cleanup pass (part of [`Index::rebuild_all`]): delete `index.md` /
    /// `index.jsonl` in non-canonical folders (date-shards that should carry
    /// none). Symmetric with index creation.
    ///
    /// **Only deletes generated catalog artifacts, never user content.** Two
    /// guards keep this from eating data:
    /// - `min_depth(2)` so the walk starts *below* the type-folder root — the
    ///   canonical `<type-folder>/index.md` + `index.jsonl` are never targeted
    ///   here (they are rewritten by the per-folder builders, or removed only
    ///   when the folder is genuinely empty, in the dedicated branch below). The
    ///   old `min_depth(1)` deleted them up front, so a rebuild aborted by one
    ///   malformed file left every type-folder catalog destroyed.
    /// - [`is_deletable_catalog_artifact`] confirms a shard-level `index.md` is
    ///   an actual generated catalog (or stale/garbage leftover), NOT a content
    ///   file a user wrote at that name (e.g. `dbmd write …/index.md --type
    ///   email`, plausible when mirroring a website/doc export). Matching by
    ///   filename alone silently deleted such records on the next rebuild.
    pub fn cleanup(store: &Store) -> crate::Result<()> {
        for layer in Layer::all() {
            let layer_dir = store.root.join(layer_dir_name(layer));
            if !layer_dir.is_dir() {
                continue;
            }
            for tf in type_folders_in_layer(store, layer) {
                let tf_abs = store.root.join(&tf);
                // Any generated index inside a shard (below the type-folder
                // root) is non-canonical: delete it. Never touch a user content
                // file that merely happens to be named index.md.
                for entry in walkdir::WalkDir::new(&tf_abs)
                    .min_depth(2)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    let p = entry.path();
                    if is_index_artifact(p) && is_deletable_catalog_artifact(p) {
                        remove_if_exists(p)?;
                    }
                }
                // Empty type-folder → no index at its root either. Same content
                // guard: an `index.md` here that is actually a user record (the
                // only file in the folder) is preserved, not deleted.
                if walk_type_folder_files(&tf_abs).is_empty() {
                    let md = tf_abs.join("index.md");
                    if is_deletable_catalog_artifact(&md) {
                        remove_if_exists(&md)?;
                    }
                    remove_if_exists(&tf_abs.join("index.jsonl"))?;
                }
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Private free helpers — all self-contained, none call back into Store/parser.
// ─────────────────────────────────────────────────────────────────────────

/// Write both artifacts for a type-folder, or delete them if the folder is now
/// empty. The single funnel both write-through and rebuild go through, so their
/// output is byte-identical by construction.
fn write_type_folder_artifacts(
    store: &Store,
    folder: &Path,
    records: &[IndexRecord],
) -> crate::Result<()> {
    let folder_abs = store.root.join(folder);
    let md_path = folder_abs.join("index.md");
    let jsonl_path = folder_abs.join("index.jsonl");
    if records.is_empty() {
        remove_if_exists(&md_path)?;
        remove_if_exists(&jsonl_path)?;
        return Ok(());
    }
    let idx = Index {
        level: IndexLevel::TypeFolder(folder.to_path_buf()),
        records: records.to_vec(),
        child_counts: BTreeMap::new(),
    };
    write_atomic(&md_path, idx.to_markdown())?;
    write_atomic(&jsonl_path, idx.to_jsonl())?;
    Ok(())
}

/// Re-render the layer + root rollups that sit above `folder` — the
/// **loop path**, O(changed). Counts + previews come from the type-folders'
/// on-disk `index.jsonl` sidecars ([`collect_child_stats`]), NOT from a
/// content-tree walk: a single write reads one sidecar per type-folder (shared
/// across the layer and root rollups) — never the millions of files under the
/// shards. `build_layer` / `build_root` (which *do* walk the content tree) are
/// reserved for the from-scratch sweeps ([`Index::rebuild_all`],
/// [`Index::write_level`], [`Index::render_dry_run`]). The result is
/// byte-identical to those builders because in the loop — exactly as in
/// `rebuild_all` — every touched folder's jsonl is rewritten before its parents
/// are rolled up, so the per-folder stat (`count` / `newest`) equals what a
/// from-scratch walk would compute.
fn update_parents(store: &Store, folder: &Path) -> crate::Result<()> {
    // Read every type-folder's sidecar EXACTLY ONCE into a stat cache (`count` +
    // `newest` record), then render both rollups from the cache. This removed the
    // old 2–3×-per-write reparse (`child_counts_from_jsonl` for a count, plus
    // `render_layer_md_with_store` / `render_root_md_with_store` each doing a full
    // `read_jsonl_records` parse + sort just to take `.first()`); the output stays
    // byte-identical (`count` == `read_jsonl_records().len()`, `newest` == its
    // `.first()`).
    //
    // COST, stated honestly: this is `O(total catalogued records)` per write, NOT
    // `O(changed)`. `collect_child_stats` reads and line-parses EVERY type-folder
    // sidecar in the store to recompute the rollups, so a single high-volume
    // folder (months of ingested emails) makes an unrelated tiny write scan that
    // whole sidecar (a ~50× slowdown at ~200k records was measured). The crate's
    // literal `Store::walk` guard holds — this reads `index.jsonl` sidecars, not
    // the content tree — but the broader `O(changed)` complexity the loop path
    // advertises is NOT met here. Restoring true `O(changed)` needs a persisted
    // per-folder stat cache (or an in-place rollup patch for `on_write`); that is
    // a deliberate change to the catalog hot path, tracked as a follow-up, not
    // done inline. Until then, do not describe this op as `O(changed)`.
    let stats = collect_child_stats(store, &Layer::all())?;

    let layer = folder
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .and_then(layer_from_dir_name);
    if let Some(layer) = layer {
        let p = store.root.join(layer_dir_name(layer)).join("index.md");
        if layer_has_children(&stats, layer) {
            write_atomic(
                &p,
                render_layer_md_from_stats(layer, &stats, &store.config.folders),
            )?;
        } else {
            remove_if_exists(&p)?;
        }
    }
    let rp = store.root.join("index.md");
    if stats.values().any(|s| s.count > 0) {
        write_atomic(
            &rp,
            render_root_md_from_stats(&stats, &store.config.folders),
        )?;
    } else {
        remove_if_exists(&rp)?;
    }
    Ok(())
}

/// True if `layer` has at least one non-empty child type-folder in `stats`.
fn layer_has_children(stats: &BTreeMap<PathBuf, FolderStat>, layer: Layer) -> bool {
    let prefix = format!("{}/", layer_dir_name(layer));
    stats
        .iter()
        .any(|(tf, s)| s.count > 0 && path_to_unix(tf).starts_with(&prefix))
}

/// Render a layer `index.md` from the prebuilt per-folder stat cache — each
/// child's count + newest summary/updated come from its single cached sidecar
/// read, so the rollup matches the folder artifacts exactly (write-through and
/// rebuild alike) without re-reading any sidecar.
fn render_layer_md_from_stats(
    layer: Layer,
    stats: &BTreeMap<PathBuf, FolderStat>,
    folders: &BTreeMap<String, FolderMeta>,
) -> String {
    let layer_dir = layer_dir_name(layer);
    let prefix = format!("{layer_dir}/");
    let mut max_upd: Option<DateTime<FixedOffset>> = None;
    let mut entries = String::new();
    for (tf, stat) in stats {
        if stat.count == 0 || !path_to_unix(tf).starts_with(&prefix) {
            continue;
        }
        if let Some(u) = stat.newest.as_ref().and_then(|r| r.updated) {
            max_upd = Some(match max_upd {
                Some(cur) if cur >= u => cur,
                _ => u,
            });
        }
        let tf_unix = path_to_unix(tf);
        let (display, description) = folder_label(&tf_unix, folder_basename(tf), folders);
        entries.push_str(&folder_entry(&tf_unix, &display, stat.count, description));
    }
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str("type: index\n");
    s.push_str("scope: layer\n");
    s.push_str(&format!("folder: {layer_dir}\n"));
    if let Some(ts) = max_upd {
        s.push_str(&format!("updated: {}\n", fmt_ts(&ts)));
    }
    s.push_str("---\n\n");
    s.push_str(&format!("# {layer_dir}\n\n"));
    s.push_str(&entries);
    s
}

/// Render the root `index.md` from the prebuilt per-folder stat cache.
fn render_root_md_from_stats(
    stats: &BTreeMap<PathBuf, FolderStat>,
    folders: &BTreeMap<String, FolderMeta>,
) -> String {
    let mut max_upd: Option<DateTime<FixedOffset>> = None;
    for stat in stats.values() {
        if stat.count == 0 {
            continue;
        }
        if let Some(u) = stat.newest.as_ref().and_then(|r| r.updated) {
            max_upd = Some(match max_upd {
                Some(cur) if cur >= u => cur,
                _ => u,
            });
        }
    }
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str("type: index\n");
    s.push_str("scope: root\n");
    if let Some(ts) = max_upd {
        s.push_str(&format!("updated: {}\n", fmt_ts(&ts)));
    }
    s.push_str("---\n\n");
    s.push_str(&format!("# {ROOT_TITLE}\n"));
    for layer in Layer::all() {
        let layer_dir = layer_dir_name(layer);
        let prefix = format!("{layer_dir}/");
        let children: Vec<(&PathBuf, usize)> = stats
            .iter()
            .filter(|(tf, s)| s.count > 0 && path_to_unix(tf).starts_with(&prefix))
            .map(|(tf, s)| (tf, s.count))
            .collect();
        if children.is_empty() {
            continue;
        }
        let total: usize = children.iter().map(|(_, n)| *n).sum();
        s.push('\n');
        s.push_str(&format!("## {} ({total})\n", capitalize(layer_dir)));
        for (tf, n) in children {
            let tf_unix = path_to_unix(tf);
            let (display, description) = folder_label(&tf_unix, folder_basename(tf), folders);
            s.push_str(&folder_entry(&tf_unix, &display, n, description));
        }
    }
    s
}

/// Render a layer `index.md`, reading each child's newest summary + max-updated
/// straight from its on-disk `index.jsonl` (so the rollup matches the folder
/// artifacts exactly, write-through and rebuild alike). The **sweep-path**
/// renderer used by [`Index::rebuild_all`] / [`Index::write_level`] /
/// [`Index::render_dry_run`]; the loop path uses the cache-based
/// [`render_layer_md_from_stats`] to avoid re-reading sidecars.
fn render_layer_md_with_store(store: &Store, idx: &Index) -> String {
    let layer = match idx.level {
        IndexLevel::Layer(l) => l,
        _ => unreachable!("render_layer_md_with_store called on non-layer"),
    };
    let layer_dir = layer_dir_name(layer);
    let mut max_upd: Option<DateTime<FixedOffset>> = None;
    let mut entries = String::new();
    for (tf, n) in &idx.child_counts {
        let recs = read_jsonl_records(&store.root.join(tf).join("index.jsonl")).unwrap_or_default();
        let newest = recs.first();
        if let Some(u) = newest.and_then(|r| r.updated) {
            max_upd = Some(match max_upd {
                Some(cur) if cur >= u => cur,
                _ => u,
            });
        }
        let tf_unix = path_to_unix(tf);
        let (display, description) =
            folder_label(&tf_unix, folder_basename(tf), &store.config.folders);
        entries.push_str(&folder_entry(&tf_unix, &display, *n, description));
    }
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str("type: index\n");
    s.push_str("scope: layer\n");
    s.push_str(&format!("folder: {layer_dir}\n"));
    if let Some(ts) = max_upd {
        s.push_str(&format!("updated: {}\n", fmt_ts(&ts)));
    }
    s.push_str("---\n\n");
    s.push_str(&format!("# {layer_dir}\n\n"));
    s.push_str(&entries);
    s
}

/// Render the root `index.md`, taking each child's max-updated from its on-disk
/// `index.jsonl`. The **sweep-path** renderer (the loop path uses
/// [`render_root_md_from_stats`]).
fn render_root_md_with_store(store: &Store, idx: &Index) -> String {
    let mut max_upd: Option<DateTime<FixedOffset>> = None;
    for tf in idx.child_counts.keys() {
        let recs = read_jsonl_records(&store.root.join(tf).join("index.jsonl")).unwrap_or_default();
        if let Some(u) = recs.first().and_then(|r| r.updated) {
            max_upd = Some(match max_upd {
                Some(cur) if cur >= u => cur,
                _ => u,
            });
        }
    }
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str("type: index\n");
    s.push_str("scope: root\n");
    if let Some(ts) = max_upd {
        s.push_str(&format!("updated: {}\n", fmt_ts(&ts)));
    }
    s.push_str("---\n\n");
    s.push_str(&format!("# {ROOT_TITLE}\n"));
    for layer in Layer::all() {
        let layer_dir = layer_dir_name(layer);
        let prefix = format!("{layer_dir}/");
        let children: Vec<(&PathBuf, &usize)> = idx
            .child_counts
            .iter()
            .filter(|(tf, _)| path_to_unix(tf).starts_with(&prefix))
            .collect();
        if children.is_empty() {
            continue;
        }
        let total: usize = children.iter().map(|(_, n)| **n).sum();
        s.push('\n');
        s.push_str(&format!("## {} ({total})\n", capitalize(layer_dir)));
        for (tf, n) in children {
            let tf_unix = path_to_unix(tf);
            let (display, description) =
                folder_label(&tf_unix, folder_basename(tf), &store.config.folders);
            s.push_str(&folder_entry(&tf_unix, &display, *n, description));
        }
    }
    s
}

/// One `index.md` browse line: `- [[path]] — summary  ·  #tag #tag` (the
/// `  ·  #…` suffix omitted when the file has no tags). The wiki-link target is
/// the canonical **bare** store-relative path (no `.md` extension — the
/// doctrine the writers emit and `validate` enforces via
/// `WIKI_LINK_HAS_EXTENSION`); the jsonl `path` keeps the real on-disk name.
fn format_md_entry(rec: &IndexRecord) -> String {
    let path = wiki_target(&rec.path);
    // Collapse the summary to a single line before interpolating it into the
    // one-line browse entry. A hand-written file may legally carry a YAML block
    // scalar (`summary: |-`) whose value spans multiple lines; rendered verbatim
    // those embedded newlines break the line-oriented `index.md` format and can
    // forge a standalone catalog entry (`\n- [[…|Click me]] — injected`). The
    // CLI writers already collapse whitespace; do the same here so the spec's
    // primary write path (agents writing files directly) can't corrupt the
    // catalog.
    let summary = collapse_whitespace(&rec.summary);
    let mut line = format!("- [[{path}]] — {summary}");
    if !rec.tags.is_empty() {
        let tags = rec
            .tags
            .iter()
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join(" ");
        line.push_str(&format!("  ·  {tags}"));
    }
    line
}

/// The deterministic `## More` footer for an over-cap type-folder.
fn more_footer(total: usize, type_: &str, layer: &str) -> String {
    format!(
        "## More\n\nThis folder has {total} files. The {MD_CAP} most recent are listed above.\nUse `dbmd index query --type {type_} --in {layer}` for the complete catalog.\n"
    )
}

/// Canonical total order: `updated` descending (None sorts last), ties broken
/// by store-relative path ascending. A *total* order, so write-through and
/// rebuild never disagree on #500 vs #501.
fn sort_records(records: &mut [IndexRecord]) {
    records.sort_by(record_recency_cmp);
}

impl IndexRecord {
    /// Build the [`IndexRecord`] a freshly-rebuilt `index.jsonl` *should* hold
    /// for the file at `abs` (catalogued under store-relative `rel`).
    ///
    /// This is the single canonical projection from frontmatter → sidecar
    /// record: [`Index::build_type_folder`] uses the same path to write the
    /// jsonl, so the validator can rebuild the expected record here and compare
    /// it field-for-field against the committed line — covering **every**
    /// queryable/dedup field the query path reads (`summary`, `type`, `tags`,
    /// `links`, `created`, `updated`, and every type-specific `fields` entry
    /// like `email` / `domain` / `company` / `amount` / `vendor`) without the
    /// validator hand-rolling (and drifting from) the projection per field.
    pub(crate) fn expected_from_file(abs: &Path, rel: PathBuf) -> crate::Result<IndexRecord> {
        record_from_file(abs, rel)
    }
}

/// Build an [`IndexRecord`] from a file on disk. Missing `summary` →
/// [`MISSING_SUMMARY`] placeholder (the index never invents a summary).
fn record_from_file(abs: &Path, rel: PathBuf) -> crate::Result<IndexRecord> {
    let mut meta = read_frontmatter(abs)?;
    // Records carry an effective `meta-type` in the catalog: the declared value
    // (already spilled into `fields` by `read_frontmatter`), or the default
    // `fact` when absent — so `--where meta-type=fact` sees un-annotated records.
    // Sources are evidence and carry no meta-type.
    if rel.starts_with("records") {
        meta.fields
            .entry("meta-type".to_string())
            .or_insert_with(|| Value::String("fact".to_string()));
    }
    Ok(IndexRecord {
        path: rel,
        type_: meta.type_.unwrap_or_default(),
        summary: meta.summary.unwrap_or_else(|| MISSING_SUMMARY.to_string()),
        tags: meta.tags,
        links: meta.links,
        created: meta.created,
        updated: meta.updated,
        fields: meta.fields,
    })
}

/// The slice of a frontmatter this module needs.
struct FileMeta {
    type_: Option<String>,
    summary: Option<String>,
    tags: Vec<String>,
    links: Vec<String>,
    created: Option<DateTime<FixedOffset>>,
    updated: Option<DateTime<FixedOffset>>,
    fields: BTreeMap<String, Value>,
}

/// Minimal frontmatter read: split the leading `---`…`---` block and parse it
/// as YAML, extracting the typed fields and spilling the rest into `fields`.
/// Self-contained (does not route through the `parser` module).
///
/// **Body bytes are never required to be UTF-8.** `sources/` is "preserved
/// verbatim" per the SPEC and routinely carries non-UTF-8 imports (Latin-1
/// emails dropped in by `rsync`/`mbsync`/`cp`); the body can hold any byte. We
/// read the file as raw bytes and lossily decode *only* the leading frontmatter
/// region, so a stray non-UTF-8 byte in the body can never abort the projection
/// (the old `fs::read_to_string` failed on the first such byte anywhere in the
/// file, taking a whole `rebuild_all` / write-through down with it). The
/// frontmatter itself is expected to be UTF-8; if it isn't, `U+FFFD` markers
/// surface in the parsed values rather than a hard abort.
fn read_frontmatter(abs: &Path) -> crate::Result<FileMeta> {
    let bytes = fs::read(abs)?;
    let yaml = extract_frontmatter_block_lossy(&bytes).unwrap_or_default();
    let map: serde_norway::Mapping = if yaml.trim().is_empty() {
        serde_norway::Mapping::new()
    } else {
        serde_norway::from_str(&yaml).map_err(|e| {
            crate::Error::Store(crate::store::StoreError::BadTypeIndex {
                path: abs.to_path_buf(),
                message: format!("frontmatter YAML: {e}"),
            })
        })?
    };

    let mut type_ = None;
    let mut summary = None;
    let mut tags = Vec::new();
    let mut links = Vec::new();
    let mut created = None;
    let mut updated = None;
    let mut fields = BTreeMap::new();

    for (k, v) in map {
        let key = match k.as_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        match key.as_str() {
            // `type` and `summary` are coerced with the SAME scalar rule the
            // validator applies (`validate::scalar_string`: String/Number/Bool →
            // string). A bare `v.as_str()` returns `None` for an unquoted numeric
            // or boolean scalar (`summary: 2026`, `type: true`), so the index
            // would write the `(no summary)` / empty-type placeholder while
            // `dbmd validate` reads the file as HAVING that summary/type —
            // yielding a permanently-unfixable `INDEX_SUMMARY_MISMATCH` (every
            // rebuild reproduces the same mismatched placeholder). Coercing here
            // keeps the writer and the validator byte-for-byte in agreement.
            "type" => type_ = scalar_string(&v),
            "summary" => summary = scalar_string(&v),
            "tags" => tags = yaml_string_list(&v),
            "links" => links = yaml_string_list(&v),
            "created" => created = v.as_str().and_then(parse_ts),
            "updated" => updated = v.as_str().and_then(parse_ts),
            // `path`, `type`, `summary`, `tags`, `links`, `created`, `updated`
            // are the reserved IndexRecord keys; everything else (including
            // `id`, `status`, type-specific fields) goes to `fields`.
            "path" => {}
            _ => {
                fields.insert(key, yaml_to_json_value(&v));
            }
        }
    }

    Ok(FileMeta {
        type_,
        summary,
        tags,
        links,
        created,
        updated,
        fields,
    })
}

/// A YAML scalar (`String`/`Number`/`Bool`) rendered as a string; `None` for
/// sequences/mappings/null. **Must stay identical to `validate::scalar_string`**
/// so the index writer and the validator coerce `type`/`summary` the same way
/// (see [`read_frontmatter`]); an unquoted `summary: 2026` becomes `"2026"` in
/// both, not a placeholder here and a real value there.
fn scalar_string(v: &serde_norway::Value) -> Option<String> {
    match v {
        serde_norway::Value::String(s) => Some(s.clone()),
        serde_norway::Value::Number(n) => Some(n.to_string()),
        serde_norway::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Lossily decode the leading frontmatter region of a file given its raw bytes,
/// then pull the YAML between the opening `---` and the next `---`. Only the
/// frontmatter region needs to be valid UTF-8 in practice; the body may carry
/// arbitrary bytes (a verbatim `sources/` import). Returns `None` when the file
/// has no frontmatter fence at its very start.
fn extract_frontmatter_block_lossy(bytes: &[u8]) -> Option<String> {
    // Decode lossily so a non-UTF-8 body byte never aborts the read. The
    // frontmatter is at the very start of the file, so a lossy whole-file decode
    // is correct for extracting it (and cheap relative to the YAML parse). A
    // leading UTF-8 BOM is stripped by `extract_frontmatter_block`.
    let text = String::from_utf8_lossy(bytes);
    extract_frontmatter_block(&text)
}

/// Pull the YAML between a leading `---` line and the next `---` line. Returns
/// `None` when the file has no frontmatter fence at its very start.
fn extract_frontmatter_block(text: &str) -> Option<String> {
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = trimmed.lines();
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut block = String::new();
    for line in lines {
        if line.trim_end() == "---" {
            return Some(block);
        }
        block.push_str(line);
        block.push('\n');
    }
    None // no closing fence
}

/// Read a string scalar or a sequence-of-string-scalars into a `Vec<String>`.
/// Wiki-link items keep their `[[…]]` form verbatim.
fn yaml_string_list(v: &serde_norway::Value) -> Vec<String> {
    match v {
        serde_norway::Value::String(s) => vec![s.clone()],
        serde_norway::Value::Sequence(seq) => seq
            .iter()
            .filter_map(yaml_string_or_wiki_link_literal)
            .collect(),
        _ => Vec::new(),
    }
}

fn yaml_string_or_wiki_link_literal(v: &serde_norway::Value) -> Option<String> {
    v.as_str()
        .map(str::to_string)
        .or_else(|| unquoted_wiki_link_literal(v))
}

fn yaml_to_json_value(v: &serde_norway::Value) -> Value {
    if let Some(link) = unquoted_wiki_link_literal(v) {
        return Value::String(link);
    }
    match v {
        serde_norway::Value::String(s) => Value::String(s.clone()),
        serde_norway::Value::Bool(b) => Value::Bool(*b),
        serde_norway::Value::Number(n) => {
            serde_json::to_value(n).unwrap_or_else(|_| Value::String(n.to_string()))
        }
        serde_norway::Value::Sequence(seq) => {
            Value::Array(seq.iter().map(yaml_to_json_value).collect())
        }
        serde_norway::Value::Mapping(_) | serde_norway::Value::Tagged(_) => {
            serde_json::to_value(v).unwrap_or(Value::Null)
        }
        serde_norway::Value::Null => Value::Null,
    }
}

fn unquoted_wiki_link_literal(v: &serde_norway::Value) -> Option<String> {
    let serde_norway::Value::Sequence(outer) = v else {
        return None;
    };
    if outer.len() != 1 {
        return None;
    }
    let serde_norway::Value::Sequence(inner) = &outer[0] else {
        return None;
    };
    let [serde_norway::Value::String(target)] = inner.as_slice() else {
        return None;
    };
    Some(format!("[[{target}]]"))
}

/// Parse an RFC3339 timestamp scalar.
fn parse_ts(s: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(s.trim()).ok()
}

/// Render a timestamp the same way `serde_json` renders an `IndexRecord`
/// timestamp (RFC3339, `Z` for UTC, sub-seconds preserved) so the md
/// frontmatter and the jsonl agree byte-for-byte.
fn fmt_ts(ts: &DateTime<FixedOffset>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// Max `updated` over an iterator of optional timestamps.
fn max_updated<'a>(
    it: impl Iterator<Item = Option<&'a DateTime<FixedOffset>>>,
) -> Option<DateTime<FixedOffset>> {
    let mut best: Option<DateTime<FixedOffset>> = None;
    for ts in it.flatten() {
        best = Some(match best {
            Some(cur) if cur >= *ts => cur,
            _ => *ts,
        });
    }
    best
}

/// Read a type-folder's `index.jsonl` into records, applying last-write-wins by
/// `path` over any un-compacted lines (so a half-compacted jsonl still reads
/// cleanly). Missing file → empty set. Returns records in canonical order.
fn read_jsonl_records(jsonl: &Path) -> crate::Result<Vec<IndexRecord>> {
    let text = match fs::read_to_string(jsonl) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    // Last-write-wins by path; preserve only the final occurrence.
    let mut by_path: BTreeMap<PathBuf, IndexRecord> = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: IndexRecord = serde_json::from_str(line).map_err(|e| {
            crate::Error::Store(crate::store::StoreError::BadTypeIndex {
                path: jsonl.to_path_buf(),
                message: format!("line {}: {e}", i + 1),
            })
        })?;
        by_path.insert(rec.path.clone(), rec);
    }
    let mut records: Vec<IndexRecord> = by_path.into_values().collect();
    sort_records(&mut records);
    Ok(records)
}

/// The minimal rollup stat a parent index needs from one type-folder's
/// `index.jsonl`: how many distinct files it catalogs (`count`) and the single
/// newest record (`newest`, the recency-sorted `.first()` — its `updated` feeds
/// the parent's derived `updated`, its `summary` the layer preview). Holding the
/// newest record alone, rather than the whole sidecar, is what keeps a rollup
/// recompute cheap regardless of how large the sidecar grows.
#[derive(Debug, Clone, Default, PartialEq)]
struct FolderStat {
    count: usize,
    newest: Option<IndexRecord>,
}

/// Read a type-folder's `index.jsonl` ONCE and reduce it to a [`FolderStat`]:
/// distinct-`path` count (last-write-wins) plus the recency-newest record. A
/// missing sidecar is the default (`count: 0`, `newest: None`). This is the
/// **loop-path** rollup primitive — one streaming pass per sidecar, never the
/// content tree and never the 2–3× full reparse the old
/// `jsonl_record_count` + `read_jsonl_records` pair did. `count` is
/// byte-identical to [`read_jsonl_records`]`.len()` and `newest` to its
/// `.first()`, so a rollup built from these stats matches the from-scratch
/// builders byte-for-byte.
fn read_folder_stat(jsonl: &Path) -> crate::Result<FolderStat> {
    let text = match fs::read_to_string(jsonl) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FolderStat::default()),
        Err(e) => return Err(e.into()),
    };
    // Last-write-wins by path, exactly like `read_jsonl_records`, so count and
    // newest are computed over the same compacted record set.
    let mut by_path: BTreeMap<PathBuf, IndexRecord> = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: IndexRecord = serde_json::from_str(line).map_err(|e| {
            crate::Error::Store(crate::store::StoreError::BadTypeIndex {
                path: jsonl.to_path_buf(),
                message: format!("line {}: {e}", i + 1),
            })
        })?;
        by_path.insert(rec.path.clone(), rec);
    }
    let count = by_path.len();
    // The newest record is the minimum under `sort_records`' order (updated
    // desc, None last, ties by path asc) — i.e. what `.first()` returns. Find it
    // with a single min-scan instead of sorting the whole set.
    let newest = by_path.into_values().min_by(record_recency_cmp);
    Ok(FolderStat { count, newest })
}

/// The total order [`sort_records`] imposes, as a comparator over two records:
/// `updated` descending (None last), ties broken by store-relative path
/// ascending. Kept in one place so `read_folder_stat`'s min-scan agrees with the
/// sort byte-for-byte on which record is "newest".
fn record_recency_cmp(a: &IndexRecord, b: &IndexRecord) -> std::cmp::Ordering {
    match (b.updated, a.updated) {
        (Some(bu), Some(au)) => bu.cmp(&au),
        (Some(_), None) => std::cmp::Ordering::Greater, // a is None → after b
        (None, Some(_)) => std::cmp::Ordering::Less,    // b is None → after a
        (None, None) => std::cmp::Ordering::Equal,
    }
    .then_with(|| a.path.cmp(&b.path))
}

/// Per-child rollup stats for `layers`, read from each type-folder's on-disk
/// `index.jsonl` (one [`read_folder_stat`] pass each) rather than walked from the
/// content tree. The **loop-path** counterpart to the from-scratch counting in
/// [`Index::build_layer`] / [`Index::build_root`], reusing one read per sidecar
/// across BOTH the layer and root rollups. Empty folders (`count == 0`) are kept
/// out of the map.
///
/// NOTE on cost: this performs one read per type-folder, but each read line-parses
/// that folder's entire `index.jsonl`, so the total is `O(total catalogued
/// records)`, not `O(type-folders)` — it reads the whole catalog every call. It
/// avoids the content-tree walk ([`Store::walk`]), but it is NOT `O(changed)`. See
/// [`update_parents`] for the honest bound and the follow-up to fix it.
fn collect_child_stats(
    store: &Store,
    layers: &[Layer],
) -> crate::Result<BTreeMap<PathBuf, FolderStat>> {
    let mut stats = BTreeMap::new();
    for &layer in layers {
        for tf in type_folders_in_layer(store, layer) {
            let stat = read_folder_stat(&store.root.join(&tf).join("index.jsonl"))?;
            if stat.count > 0 {
                stats.insert(tf, stat);
            }
        }
    }
    Ok(stats)
}

/// Walk a type-folder's `.md` content files, recursing through date-shards,
/// excluding the `index.md` artifact itself and any hidden entries.
fn walk_type_folder_files(folder_abs: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !folder_abs.is_dir() {
        return out;
    }
    for entry in walkdir::WalkDir::new(folder_abs)
        .into_iter()
        .filter_entry(|e| !is_hidden(e.file_name()))
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if p.file_name().and_then(|n| n.to_str()) == Some("index.md") {
            continue;
        }
        out.push(p.to_path_buf());
    }
    out
}

/// The immediate type-folders under a layer (one directory level below the
/// layer dir), as store-relative paths. Hidden dirs and `log/` are skipped.
fn type_folders_in_layer(store: &Store, layer: Layer) -> Vec<PathBuf> {
    let layer_dir = store.root.join(layer_dir_name(layer));
    let mut out = Vec::new();
    let rd = match fs::read_dir(&layer_dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for entry in rd.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        if is_hidden(entry.file_name().as_os_str()) || name == "log" {
            continue;
        }
        out.push(PathBuf::from(layer_dir_name(layer)).join(name));
    }
    out.sort();
    out
}

/// The layer a *loose* content file sits directly in: `<layer>/<file>.md` with
/// no type-folder between them — exactly two path components, the first a known
/// layer. `None` for a file inside a type-folder (`<layer>/<type>/…`, the common
/// case) or one outside any layer. A loose file is catalogued in the layer's own
/// `index.jsonl`, not a type-folder's.
fn loose_layer_of(file_rel: &Path) -> Option<Layer> {
    let mut comps = file_rel.components();
    let layer = layer_from_dir_name(comps.next()?.as_os_str().to_str()?)?;
    comps.next()?; // the file segment must exist…
    if comps.next().is_some() {
        return None; // …and be the last one (else it's inside a type-folder)
    }
    Some(layer)
}

/// The `.md` content files that live directly at a layer root (loose files),
/// excluding `index.md` and any subdirectory (type-folders are walked
/// separately). Non-recursive: only the layer's immediate children.
fn loose_files_in_layer(store: &Store, layer: Layer) -> Vec<PathBuf> {
    let layer_dir = store.root.join(layer_dir_name(layer));
    let mut out = Vec::new();
    let rd = match fs::read_dir(&layer_dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if is_index_artifact(&p) || is_hidden(entry.file_name().as_os_str()) {
            continue;
        }
        out.push(p);
    }
    out
}

/// Write (or remove, when empty) a layer's own `index.jsonl` — the complete twin
/// for the loose files that live directly at the layer root. The single funnel
/// both write-through (`on_write`/`on_remove`/`on_rename`) and the sweeps
/// (`rebuild_all`/`write_level`) go through, so their output is byte-identical.
fn write_layer_jsonl(store: &Store, layer: Layer, records: &[IndexRecord]) -> crate::Result<()> {
    let path = store.root.join(layer_dir_name(layer)).join("index.jsonl");
    if records.is_empty() {
        remove_if_exists(&path)?;
        return Ok(());
    }
    let idx = Index {
        level: IndexLevel::Layer(layer),
        records: records.to_vec(),
        child_counts: BTreeMap::new(),
    };
    write_atomic(&path, idx.to_jsonl())
}

/// Upsert (`removing` = false) or remove (`removing` = true) a loose file's row
/// in its layer `index.jsonl`, serialising the read-modify-write under a folder
/// lock (same discipline as the type-folder write-through). The layer `index.md`
/// rollup is untouched — loose files do not change type-folder counts.
fn apply_loose_change(
    store: &Store,
    layer: Layer,
    file_rel: &Path,
    removing: bool,
) -> crate::Result<()> {
    let layer_dir = store.root.join(layer_dir_name(layer));
    let _lock = FolderLock::acquire(&layer_dir);
    let jsonl = layer_dir.join("index.jsonl");
    let mut records = read_jsonl_records(&jsonl)?;
    records.retain(|r| r.path != file_rel);
    if !removing {
        records.push(record_from_file(
            &store.root.join(file_rel),
            file_rel.to_path_buf(),
        )?);
    }
    sort_records(&mut records);
    write_layer_jsonl(store, layer, &records)
}

/// The type-folder a content file belongs to: `<layer>/<type>` (the first two
/// path components), or `None` if the path is not under a known layer with at
/// least a type segment.
fn type_folder_of(file_rel: &Path) -> Option<PathBuf> {
    let mut comps = file_rel.components();
    let layer = comps.next()?.as_os_str().to_str()?;
    layer_from_dir_name(layer)?;
    let type_seg = comps.next()?.as_os_str().to_str()?;
    Some(PathBuf::from(layer).join(type_seg))
}

/// Convert an absolute path under `root` to a store-relative path.
fn rel_to_store(root: &Path, abs: &Path) -> Option<PathBuf> {
    abs.strip_prefix(root).ok().map(|p| p.to_path_buf())
}

/// Normalize a possibly-absolute or `./`-prefixed path to a clean
/// store-relative form (drops a leading `./`; leaves already-relative paths).
fn normalize_rel(p: &Path) -> PathBuf {
    let s = path_to_unix(p);
    let s = s.strip_prefix("./").unwrap_or(&s);
    PathBuf::from(s)
}

fn is_index_artifact(p: &Path) -> bool {
    matches!(
        p.file_name().and_then(|n| n.to_str()),
        Some("index.md") | Some("index.jsonl")
    )
}

/// True when a file named `index.md` / `index.jsonl` is safe for [`Index::cleanup`]
/// to delete — i.e. it is a generated catalog artifact (or a stale/garbage
/// leftover from a previous build), NOT a user content file that merely happens
/// to be named `index.md`.
///
/// - `index.jsonl` is always a machine artifact (content files are `.md`), so it
///   is always deletable.
/// - `index.md` is deletable UNLESS it parses as a content file — frontmatter
///   whose `type` is some real record type (anything other than `index`). A
///   generated catalog carries `type: index`; a user record carries its own type
///   (`email`, `note`, …) and must be preserved (deleting it is silent,
///   unrecoverable data loss). A leftover with no/garbage frontmatter (e.g. a
///   bare `stale\n`) is treated as a deletable stale artifact.
fn is_deletable_catalog_artifact(p: &Path) -> bool {
    match p.file_name().and_then(|n| n.to_str()) {
        Some("index.jsonl") => true,
        Some("index.md") => match read_frontmatter(p) {
            // Real content file (non-`index` type) → preserve, never delete.
            Ok(meta) => meta.type_.as_deref().is_none_or(|t| t == "index"),
            // Unreadable / no frontmatter → a stale or garbage artifact, deletable.
            Err(_) => true,
        },
        _ => false,
    }
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_str().map(|s| s.starts_with('.')).unwrap_or(false)
}

fn layer_dir_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Sources => "sources",
        Layer::Records => "records",
    }
}

/// Local layer-name parse. Mirrors the contract of [`Layer::from_dir_name`];
/// kept local to keep this module's walk self-contained (see the module header).
fn layer_from_dir_name(name: &str) -> Option<Layer> {
    match name {
        "sources" => Some(Layer::Sources),
        "records" => Some(Layer::Records),
        _ => None,
    }
}

/// The final path component as a `&str` (folder basename).
fn folder_basename(p: &Path) -> &str {
    p.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

/// The canonical wiki-link target for a content path: the store-relative path
/// with `/` separators and the trailing `.md` stripped (the bare form the
/// `index.md` browse view links to).
fn wiki_target(p: &Path) -> String {
    let unix = path_to_unix(p);
    unix.strip_suffix(".md").unwrap_or(&unix).to_string()
}

/// Render a path with `/` separators regardless of host OS, so artifacts are
/// identical on every platform.
///
/// A non-UTF-8 path component (reachable on Linux/ext4, db.md's primary
/// deployment target, where `sources/` files arrive verbatim from Latin-1
/// exports) is decoded **lossily** with `U+FFFD` markers rather than silently
/// dropped. The old `filter_map(|c| c.as_os_str().to_str())` dropped any bad
/// component entirely, so `sources/emails/caf\xe9.md` serialized as
/// `sources/emails` — a path pointing at the *directory*, not the file, that
/// also collapsed distinct files onto one `index.jsonl` key. Lossy decoding
/// keeps the leaf present and visibly marked.
fn path_to_unix(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Serde for [`IndexRecord::path`]: always forward-slash on the wire, so the
/// `index.jsonl` catalog is identical whether the store was written on POSIX or
/// Windows (a git clone across OSes yields the same paths, and the last-write-
/// wins upsert key never splits on separator style). On POSIX this matches the
/// default `PathBuf` serialization; on Windows it rewrites `\` to `/`.
mod path_serde {
    use super::path_to_unix;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::path::{Path, PathBuf};

    pub fn serialize<S: Serializer>(p: &Path, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&path_to_unix(p))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        Ok(PathBuf::from(String::deserialize(d)?))
    }
}

/// ASCII-capitalize the first character.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Collapse all runs of whitespace (including newlines) into single spaces and
/// trim the ends — the single-line normalization the `index.md` browse entry
/// ([`format_md_entry`]) applies so a multi-line block-scalar summary can never
/// inject a newline into a catalog line.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Derive a folder's display name from its basename: separators (`-`, `_`)
/// become spaces and the first character is upper-cased (`hubspot-exports` →
/// `Hubspot exports`). A deterministic floor — the curator overrides it via
/// `DB.md ## Folders` (`records/x|HubSpot exports`) for casing the tool cannot
/// guess. The tool tidies a folder's *name*; it never infers its *meaning*.
fn default_display(basename: &str) -> String {
    let spaced: String = basename
        .chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c })
        .collect();
    capitalize(&spaced)
}

/// The display name + optional description a root/layer rollup shows for a child
/// type-folder: the curator's `## Folders` metadata when present, else the
/// derived display name and **no description**. This is the whole anti-"tool
/// invents the curator's judgment" contract for the rollups — a description is
/// surfaced only when the agent authored one; it is never composed from the
/// folder's newest member or any other content.
fn folder_label<'a>(
    tf_unix: &str,
    basename: &str,
    folders: &'a BTreeMap<String, FolderMeta>,
) -> (String, Option<&'a str>) {
    let meta = folders.get(tf_unix);
    let display = meta
        .and_then(|m| m.display.as_deref())
        .map(str::to_string)
        .unwrap_or_else(|| default_display(basename));
    (display, meta.and_then(|m| m.description.as_deref()))
}

/// One root/layer rollup entry: `- [[<tf>/index|<Display>]] (<count>)` with an
/// ` — <description>` suffix only when the curator authored one.
fn folder_entry(tf_unix: &str, display: &str, count: usize, description: Option<&str>) -> String {
    match description {
        Some(d) => format!("- [[{tf_unix}/index|{display}]] ({count}) — {d}\n"),
        None => format!("- [[{tf_unix}/index|{display}]] ({count})\n"),
    }
}

/// Atomic (rename-based) write for the **derived** catalog (`index.md` /
/// `index.jsonl`). Deliberately NOT `fsync`-durable like [`crate::fsx`]: the
/// index is rebuildable (`dbmd index rebuild`) and this is the O(changed)
/// write-through path, so a per-write `fsync` would be cost without benefit — a
/// crash-lost catalog write is recovered by a rebuild, not data loss. (Primary
/// data — content records, `log.md` — uses the durable `crate::fsx` path.)
fn write_atomic(path: &Path, contents: String) -> crate::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile_in(dir)?;
    tmp.write_all(contents.as_bytes())?;
    tmp.flush()?;
    tmp.persist(path)?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> crate::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn bad_index(path: &Path, msg: &str) -> crate::Error {
    crate::Error::Store(crate::store::StoreError::BadTypeIndex {
        path: path.to_path_buf(),
        message: msg.to_string(),
    })
}

/// Per-type-folder advisory lock for the write-through sidecar read-modify-write.
///
/// The write-through update of a folder's `index.jsonl`/`index.md` is a
/// read-snapshot → modify → atomic-rename-over-whole-file sequence. The SPEC
/// sanctions many-writer concurrency for `records/` (`dbmd write` is
/// `create_new`-race-safe for the *content* file), but two concurrent writers to
/// the SAME type-folder would each read the same sidecar snapshot, add only their
/// own row, and rename their whole file over the other's — a classic lost update,
/// dropping most rows until a manual `dbmd index rebuild`. This lock serializes
/// the per-folder RMW (the content file is already serialized by `create_new`),
/// so concurrent sanctioned writes each see the other's row.
///
/// Implementation: a hidden `<type-folder>/.index.lock` acquired via `create_new`
/// (the same O_EXCL primitive `cmd/write.rs` uses), bounded-spin with a small
/// sleep, and stale-lock breaking by mtime age so a crashed writer can't wedge
/// the folder forever. The dotfile name keeps it out of the content walk
/// (`walk_type_folder_files` skips hidden) and out of `cleanup`
/// (`is_index_artifact` only matches `index.md`/`index.jsonl`). RAII: the lock is
/// released (file removed) on drop, including on the error paths.
struct FolderLock {
    path: PathBuf,
    held: bool,
}

impl FolderLock {
    /// Acquire the lock for `folder_abs`. Spins (with a short sleep) up to a
    /// bounded number of attempts, breaking a lock older than the staleness
    /// window so a crash can't deadlock the folder. Best-effort: if the lock
    /// genuinely can't be taken (extremely rare contention), it proceeds
    /// unlocked rather than failing the write — degrading to the prior behavior
    /// instead of erroring a sanctioned operation.
    fn acquire(folder_abs: &Path) -> Self {
        use std::time::{Duration, SystemTime};
        const MAX_ATTEMPTS: u32 = 600; // ~6s at 10ms/attempt
        const SPIN: Duration = Duration::from_millis(10);
        const STALE_AFTER: Duration = Duration::from_secs(30);

        let path = folder_abs.join(".index.lock");
        // Ensure the folder exists so the lockfile create can succeed.
        let _ = fs::create_dir_all(folder_abs);
        for _ in 0..MAX_ATTEMPTS {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => {
                    return FolderLock { path, held: true };
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Break a stale lock left by a crashed writer.
                    if let Ok(meta) = fs::metadata(&path) {
                        if let Ok(modified) = meta.modified() {
                            if SystemTime::now()
                                .duration_since(modified)
                                .map(|age| age > STALE_AFTER)
                                .unwrap_or(false)
                            {
                                let _ = fs::remove_file(&path);
                                continue;
                            }
                        }
                    }
                    std::thread::sleep(SPIN);
                }
                // Any other error (e.g. permissions): give up on locking and
                // proceed unlocked rather than failing the write.
                Err(_) => return FolderLock { path, held: false },
            }
        }
        // Contention budget exhausted: proceed unlocked (best-effort).
        FolderLock { path, held: false }
    }
}

impl Drop for FolderLock {
    fn drop(&mut self) {
        if self.held {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Acquire the write-through lock for one or two type-folders. When `a == b`
/// (same-folder rename) only one lock is taken. For two distinct folders the
/// locks are always acquired in sorted order so a pair of concurrent renames
/// touching the same two folders can't deadlock by grabbing them in opposite
/// orders. Returns the guard(s); drop releases them.
fn lock_folders(store: &Store, a: &Path, b: &Path) -> Vec<FolderLock> {
    if a == b {
        return vec![FolderLock::acquire(&store.root.join(a))];
    }
    let (first, second) = if a < b { (a, b) } else { (b, a) };
    vec![
        FolderLock::acquire(&store.root.join(first)),
        FolderLock::acquire(&store.root.join(second)),
    ]
}

// A tiny atomic-write helper. `tempfile` is a dev-dependency for tests; for
// the library path we hand-roll a temp-file-then-rename so writes are atomic
// without pulling `tempfile` into the non-dev dependency set. The file handle
// is held in an `Option` so `persist` can take it out without fighting the
// `Drop` impl (which only cleans up an un-persisted temp file).
struct AtomicTemp {
    file: Option<fs::File>,
    path: PathBuf,
    persisted: bool,
}

impl AtomicTemp {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.as_mut().expect("temp file open").write_all(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.as_mut().expect("temp file open").flush()
    }
    fn persist(mut self, dest: &Path) -> std::io::Result<()> {
        if let Some(f) = self.file.take() {
            f.sync_all().ok();
            // `f` dropped here, closing the handle before the rename.
        }
        fs::rename(&self.path, dest)?;
        self.persisted = true;
        Ok(())
    }
}

impl Drop for AtomicTemp {
    fn drop(&mut self) {
        // Best-effort cleanup if not persisted (an error path bailed out).
        if !self.persisted {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn tempfile_in(dir: &Path) -> std::io::Result<AtomicTemp> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    // Monotonic-ish unique suffix; the dir is the destination dir so rename is
    // same-filesystem and therefore atomic.
    let counter = next_temp_counter();
    let name = format!(".dbmd-index-{pid}-{nanos}-{counter}.tmp");
    let path = dir.join(name);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok(AtomicTemp {
        file: Some(file),
        path,
        persisted: false,
    })
}

fn next_temp_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;
    use tempfile::TempDir;

    // ── fixtures ─────────────────────────────────────────────────────────

    /// A temp store with a `DB.md` marker. `store.config` is the parser default
    /// (these tests never exercise the config parser).
    fn mk_store() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("DB.md"), "# test store\n").unwrap();
        let store = Store {
            root: dir.path().to_path_buf(),
            config: crate::parser::Config::default(),
        };
        (dir, store)
    }

    /// Write a content file at `rel` with the given frontmatter lines + body.
    /// `fm` is the raw YAML body between the fences (no `---`).
    fn write_raw(store: &Store, rel: &str, fm: &str, body: &str) {
        let abs = store.root.join(rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(&abs, format!("---\n{fm}\n---\n{body}")).unwrap();
    }

    /// Convenience: write a typed content file with summary/updated/extras.
    fn write_doc(
        store: &Store,
        rel: &str,
        type_: &str,
        summary: Option<&str>,
        updated: Option<&str>,
        extra_yaml: &str,
    ) {
        let mut fm = format!("type: {type_}\n");
        if let Some(s) = summary {
            fm.push_str(&format!("summary: {s}\n"));
        }
        if let Some(u) = updated {
            fm.push_str(&format!("updated: {u}\n"));
        }
        fm.push_str(extra_yaml);
        write_raw(store, rel, fm.trim_end(), "\nbody text\n");
    }

    fn read(store: &Store, rel: &str) -> String {
        fs::read_to_string(store.root.join(rel)).unwrap()
    }

    fn exists(store: &Store, rel: &str) -> bool {
        store.root.join(rel).exists()
    }

    /// Collect every `index.md` + `index.jsonl` under the store, mapped to its
    /// bytes — the surface the byte-identity invariant compares.
    fn snapshot_artifacts(store: &Store) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for entry in walkdir::WalkDir::new(&store.root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if is_index_artifact(p) {
                let rel = path_to_unix(&rel_to_store(&store.root, p).unwrap());
                out.insert(rel, fs::read_to_string(p).unwrap());
            }
        }
        out
    }

    // ── build_type_folder + to_markdown ──────────────────────────────────

    #[test]
    fn type_folder_aggregates_across_shards_in_recency_order() {
        let (_d, store) = mk_store();
        // Three emails across two month-shards, deliberately written
        // out-of-recency-order on disk.
        write_doc(
            &store,
            "sources/emails/2026/05/b-old.md",
            "email",
            Some("Older mail"),
            Some("2026-05-01T09:00:00Z"),
            "",
        );
        write_doc(
            &store,
            "sources/emails/2026/06/c-new.md",
            "email",
            Some("Newest mail"),
            Some("2026-06-15T12:00:00Z"),
            "",
        );
        write_doc(
            &store,
            "sources/emails/2026/05/a-mid.md",
            "email",
            Some("Middle mail"),
            Some("2026-05-20T08:00:00Z"),
            "",
        );

        let idx = Index::build_type_folder(&store, Path::new("sources/emails")).unwrap();
        let paths: Vec<String> = idx.records.iter().map(|r| path_to_unix(&r.path)).collect();
        assert_eq!(
            paths,
            vec![
                "sources/emails/2026/06/c-new.md",
                "sources/emails/2026/05/a-mid.md",
                "sources/emails/2026/05/b-old.md",
            ],
            "records must aggregate across shards, newest `updated` first"
        );
    }

    #[test]
    fn type_folder_md_format_entries_tags_and_derived_updated() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "records/contacts/sarah-chen.md",
            "contact",
            Some("Renewal champion at Acme"),
            Some("2026-05-27T10:00:00Z"),
            "tags:\n  - renewal\n  - acme\n",
        );
        write_doc(
            &store,
            "records/contacts/no-tags.md",
            "contact",
            Some("Plain contact"),
            Some("2026-05-26T10:00:00Z"),
            "",
        );

        let idx = Index::build_type_folder(&store, Path::new("records/contacts")).unwrap();
        let md = idx.to_markdown();

        // Frontmatter is exact and the index's own `updated` is the MAX member
        // updated (the determinism the byte-identity invariant rests on).
        assert!(md.starts_with(
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\nupdated: 2026-05-27T10:00:00Z\n---\n\n# records/contacts\n"
        ), "frontmatter/heading wrong:\n{md}");

        // Entry with tags: `— summary  ·  #tag #tag`.
        assert!(
            md.contains(
                "- [[records/contacts/sarah-chen]] — Renewal champion at Acme  ·  #renewal #acme\n"
            ),
            "tagged entry wrong:\n{md}"
        );
        // Entry without tags omits the `  ·  ` suffix entirely.
        assert!(
            md.contains("- [[records/contacts/no-tags]] — Plain contact\n"),
            "untagged entry wrong:\n{md}"
        );
        assert!(
            !md.contains("Plain contact  ·"),
            "untagged entry must not emit a tag separator"
        );
        // No `## More` below the cap.
        assert!(!md.contains("## More"), "no footer expected under the cap");
    }

    #[test]
    fn missing_summary_becomes_placeholder_not_invented() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "records/notes/x.md",
            "note",
            None,
            Some("2026-05-27T10:00:00Z"),
            "",
        );
        let idx = Index::build_type_folder(&store, Path::new("records/notes")).unwrap();
        assert_eq!(idx.records[0].summary, MISSING_SUMMARY);
        let md = idx.to_markdown();
        assert!(
            md.contains("- [[records/notes/x]] — (no summary)\n"),
            "missing summary must render the placeholder, not invent text:\n{md}"
        );
    }

    // ── to_jsonl ─────────────────────────────────────────────────────────

    #[test]
    fn jsonl_is_complete_structured_and_round_trips() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "records/expenses/2026/05/e1.md",
            "expense",
            Some("Lunch with vendor"),
            Some("2026-05-10T10:00:00Z"),
            "created: 2026-05-10T09:00:00Z\nstatus: paid\namount: 42\ncompany: [[records/companies/acme]]\nrelated:\n  - [[records/concepts/spend]]\ntags:\n  - food\nlinks:\n  - records/concepts/spend\n  - [[records/concepts/renewal]]\n",
        );
        write_doc(
            &store,
            "records/expenses/2026/06/e2.md",
            "expense",
            Some("Cloud bill"),
            Some("2026-06-01T10:00:00Z"),
            "amount: 100\n",
        );

        let idx = Index::build_type_folder(&store, Path::new("records/expenses")).unwrap();
        let jsonl = idx.to_jsonl();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 2, "one JSON object per file, uncapped");

        // Newest first (e2), and each line parses back to an equal record.
        let r0: IndexRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(path_to_unix(&r0.path), "records/expenses/2026/06/e2.md");
        assert_eq!(
            r0, idx.records[0],
            "jsonl line must round-trip to the record"
        );

        // The first (data) record carries every reserved field + the extras in
        // `fields` (status/amount), and links/tags verbatim.
        let r1: IndexRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(r1.type_, "expense");
        assert_eq!(r1.summary, "Lunch with vendor");
        assert_eq!(r1.tags, vec!["food".to_string()]);
        assert_eq!(
            r1.links,
            vec![
                "records/concepts/spend".to_string(),
                "[[records/concepts/renewal]]".to_string()
            ]
        );
        assert_eq!(
            r1.created,
            Some(DateTime::parse_from_rfc3339("2026-05-10T09:00:00Z").unwrap())
        );
        assert_eq!(r1.fields.get("status"), Some(&Value::from("paid")));
        assert_eq!(r1.fields.get("amount"), Some(&Value::from(42)));
        assert_eq!(
            r1.fields.get("company"),
            Some(&Value::from("[[records/companies/acme]]"))
        );
        assert_eq!(
            r1.fields.get("related"),
            Some(&serde_json::json!(["[[records/concepts/spend]]"]))
        );
        // Reserved keys never leak into `fields`.
        for reserved in [
            "path", "type", "summary", "tags", "links", "created", "updated",
        ] {
            assert!(
                !r1.fields.contains_key(reserved),
                "reserved key {reserved} must not appear in fields"
            );
        }

        // Stable key order: declared fields first, then sorted extras.
        assert!(
            lines[1].starts_with(
                r#"{"path":"records/expenses/2026/05/e1.md","type":"expense","summary":"Lunch with vendor","tags":["food"],"links":["records/concepts/spend","[[records/concepts/renewal]]"],"created":"2026-05-10T09:00:00Z","updated":"2026-05-10T10:00:00Z","#
            ),
            "jsonl key order not stable:\n{}",
            lines[1]
        );
        // The flattened extras come in BTreeMap (sorted) order. The catalog
        // injects `meta-type: fact` into every records-layer file that does not
        // declare one, so it appears among the sorted extras (between `company`
        // and `related`).
        assert!(
            lines[1].ends_with(r#""amount":42,"company":"[[records/companies/acme]]","meta-type":"fact","related":["[[records/concepts/spend]]"],"status":"paid"}"#),
            "extras must be sorted:\n{}",
            lines[1]
        );
    }

    // ── cap + footer ─────────────────────────────────────────────────────

    #[test]
    fn over_cap_md_shows_500_plus_footer_jsonl_holds_all() {
        let (_d, store) = mk_store();
        let total = MD_CAP + 7;
        for i in 0..total {
            // Distinct, monotonically increasing `updated` so order is total.
            let day = 1 + (i % 27);
            let rel = format!("sources/emails/2026/05/m-{i:04}.md");
            let updated = format!("2026-05-{day:02}T00:00:{:02}Z", i % 60);
            write_doc(
                &store,
                &rel,
                "email",
                Some(&format!("mail {i}")),
                Some(&updated),
                "",
            );
        }
        let idx = Index::build_type_folder(&store, Path::new("sources/emails")).unwrap();
        assert_eq!(idx.records.len(), total, "jsonl/records keep every file");

        let md = idx.to_markdown();
        let entry_lines = md.lines().filter(|l| l.starts_with("- [[")).count();
        assert_eq!(entry_lines, MD_CAP, "md browse view is capped at 500");

        assert!(
            md.contains("## More\n\n"),
            "over-cap md needs a More footer"
        );
        assert!(
            md.contains(&format!(
                "This folder has {total} files. The 500 most recent are listed above.\n"
            )),
            "footer count wrong:\n{md}"
        );
        assert!(
            md.contains(
                "Use `dbmd index query --type email --in sources` for the complete catalog.\n"
            ),
            "footer must infer type=email layer=sources:\n{md}"
        );

        let jsonl = idx.to_jsonl();
        assert_eq!(jsonl.lines().count(), total, "jsonl is uncapped");
    }

    // ── sort total order ─────────────────────────────────────────────────

    #[test]
    fn sort_breaks_ties_by_path_and_puts_undated_last() {
        let mut recs = vec![
            rec("z/a.md", Some("2026-05-01T00:00:00Z")),
            rec("a/b.md", Some("2026-05-01T00:00:00Z")), // same updated, path < z/a
            rec("m/c.md", None),                         // undated → last
            rec("b/d.md", Some("2026-06-01T00:00:00Z")), // newest
        ];
        sort_records(&mut recs);
        let order: Vec<String> = recs.iter().map(|r| path_to_unix(&r.path)).collect();
        assert_eq!(order, vec!["b/d.md", "a/b.md", "z/a.md", "m/c.md"]);
    }

    fn rec(path: &str, updated: Option<&str>) -> IndexRecord {
        IndexRecord {
            path: PathBuf::from(path),
            type_: "t".into(),
            summary: "s".into(),
            tags: vec![],
            links: vec![],
            created: None,
            updated: updated.map(|u| DateTime::parse_from_rfc3339(u).unwrap()),
            fields: BTreeMap::new(),
        }
    }

    // ── build_layer / build_root ─────────────────────────────────────────

    #[test]
    fn layer_index_lists_type_folders_with_counts() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "records/contacts/a.md",
            "contact",
            Some("Contact A older"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        write_doc(
            &store,
            "records/contacts/b.md",
            "contact",
            Some("Contact B newest"),
            Some("2026-05-09T00:00:00Z"),
            "",
        );
        write_doc(
            &store,
            "records/companies/x.md",
            "company",
            Some("Acme Inc"),
            Some("2026-05-05T00:00:00Z"),
            "",
        );
        // build the type-folder artifacts first (layer preview reads their jsonl)
        Index::write_level(&store, &IndexLevel::TypeFolder("records/contacts".into())).unwrap();
        Index::write_level(&store, &IndexLevel::TypeFolder("records/companies".into())).unwrap();

        Index::write_level(&store, &IndexLevel::Layer(Layer::Records)).unwrap();
        let md = read(&store, "records/index.md");

        assert!(
            md.starts_with("---\ntype: index\nscope: layer\nfolder: records\n"),
            "layer fm:\n{md}"
        );
        // Alphabetical type-folder order: companies before contacts.
        let companies_at = md.find("companies/index").unwrap();
        let contacts_at = md.find("contacts/index").unwrap();
        assert!(
            companies_at < contacts_at,
            "type folders must be alphabetical"
        );
        // Count + display only — with no `## Folders`, the rollup never invents
        // a per-folder description from a member summary.
        assert!(
            md.contains("- [[records/contacts/index|Contacts]] (2)\n"),
            "contacts entry:\n{md}"
        );
        assert!(
            md.contains("- [[records/companies/index|Companies]] (1)\n"),
            "companies entry:\n{md}"
        );
        // Crucially: no member summary leaked into the rollup as a description.
        assert!(
            !md.contains("Contact B newest") && !md.contains("Acme Inc"),
            "layer rollup must not quote a member summary:\n{md}"
        );
        // Layer `updated` is the max across children (contacts b = 05-09).
        assert!(
            md.contains("updated: 2026-05-09T00:00:00Z\n"),
            "layer updated must be max child:\n{md}"
        );
    }

    #[test]
    fn folders_section_supplies_authored_display_and_description() {
        // The aligned contract: rollups surface the curator's `## Folders`
        // display + description; the tool never invents one. A folder with no
        // entry shows counts only — no member summary leaks in as a description.
        let (_d, mut store) = mk_store();
        store.config.folders.insert(
            "records/contacts".into(),
            crate::parser::FolderMeta {
                display: None,
                description: Some("people across customer + prospect accounts".into()),
            },
        );
        store.config.folders.insert(
            "sources/hubspot-exports".into(),
            crate::parser::FolderMeta {
                display: Some("HubSpot exports".into()),
                description: Some("deal + pipeline exports".into()),
            },
        );
        write_doc(
            &store,
            "records/contacts/a.md",
            "contact",
            Some("Contact A"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        // companies has NO `## Folders` entry → counts only.
        write_doc(
            &store,
            "records/companies/x.md",
            "company",
            Some("Acme Inc"),
            Some("2026-05-05T00:00:00Z"),
            "",
        );
        write_doc(
            &store,
            "sources/hubspot-exports/d.md",
            "hubspot-export",
            Some("a single deal export"),
            Some("2026-05-03T00:00:00Z"),
            "",
        );

        Index::rebuild_all(&store).unwrap();

        // Authored description surfaced (contacts), with the derived display.
        let records_layer = read(&store, "records/index.md");
        assert!(
            records_layer.contains("- [[records/contacts/index|Contacts]] (1) — people across customer + prospect accounts\n"),
            "authored description must surface:\n{records_layer}"
        );
        // No `## Folders` entry ⇒ counts only; the member summary never leaks in.
        assert!(
            records_layer.contains("- [[records/companies/index|Companies]] (1)\n")
                && !records_layer.contains("Acme Inc"),
            "un-described folder is counts-only:\n{records_layer}"
        );

        // Display override beats the derived "Hubspot exports".
        let sources_layer = read(&store, "sources/index.md");
        assert!(
            sources_layer.contains("- [[sources/hubspot-exports/index|HubSpot exports]] (1) — deal + pipeline exports\n"),
            "display override + description must surface:\n{sources_layer}"
        );

        // Root rollup carries the same authored metadata (display + description).
        let root = read(&store, "index.md");
        assert!(
            root.contains("- [[records/contacts/index|Contacts]] (1) — people across customer + prospect accounts\n"),
            "root surfaces authored description:\n{root}"
        );
        assert!(
            root.contains("- [[sources/hubspot-exports/index|HubSpot exports]] (1) — deal + pipeline exports\n"),
            "root surfaces display override:\n{root}"
        );
    }

    #[test]
    fn default_display_turns_separators_to_spaces_and_caps() {
        assert_eq!(default_display("contacts"), "Contacts");
        assert_eq!(default_display("hubspot-exports"), "Hubspot exports");
        assert_eq!(default_display("usage_exports"), "Usage exports");
    }

    #[test]
    fn root_index_groups_layers_with_totals_and_per_type_counts() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "sources/emails/2026/05/a.md",
            "email",
            Some("Mail"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        write_doc(
            &store,
            "sources/docs/d.md",
            "doc",
            Some("Doc"),
            Some("2026-05-02T00:00:00Z"),
            "",
        );
        write_doc(
            &store,
            "records/contacts/c.md",
            "contact",
            Some("C"),
            Some("2026-05-03T00:00:00Z"),
            "",
        );
        // wiki empty → no Wiki section

        Index::rebuild_all(&store).unwrap();
        let md = read(&store, "index.md");

        assert!(
            md.starts_with("---\ntype: index\nscope: root\n"),
            "root fm:\n{md}"
        );
        assert!(md.contains("# Knowledge base index\n"), "root title:\n{md}");
        // Layer heading with total count; Sources before Records (canonical).
        let sources_h = md
            .find("## Sources (2)")
            .expect("sources heading w/ total 2");
        let records_h = md
            .find("## Records (1)")
            .expect("records heading w/ total 1");
        assert!(sources_h < records_h, "Sources must precede Records");
        assert!(!md.contains("## Wiki"), "empty layer gets no section");
        // Per-type sub-entries with (N), no preview at root.
        assert!(
            md.contains("- [[sources/docs/index|Docs]] (1)\n"),
            "root docs entry:\n{md}"
        );
        assert!(
            md.contains("- [[sources/emails/index|Emails]] (1)\n"),
            "root emails entry:\n{md}"
        );
        assert!(
            md.contains("- [[records/contacts/index|Contacts]] (1)\n"),
            "root contacts entry:\n{md}"
        );
        assert!(!md.contains("— "), "root entries carry no preview text");
    }

    // ── write-through == rebuild (THE invariant) ─────────────────────────

    #[test]
    fn on_write_matches_rebuild_byte_for_byte() {
        // Build a store incrementally via on_write, and a second identical store
        // via a single rebuild_all, then assert every index artifact is equal.
        let (_d1, wt) = mk_store();
        let (_d2, rb) = mk_store();

        let docs: &[(&str, &str, &str, &str, &str)] = &[
            (
                "sources/emails/2026/05/e1.md",
                "email",
                "First mail",
                "2026-05-01T10:00:00Z",
                "tags:\n  - inbox\n",
            ),
            (
                "sources/emails/2026/06/e2.md",
                "email",
                "Second mail",
                "2026-06-01T10:00:00Z",
                "",
            ),
            (
                "records/contacts/sarah.md",
                "contact",
                "Sarah",
                "2026-05-15T10:00:00Z",
                "links:\n  - records/profiles/sarah\n",
            ),
            (
                "records/contacts/elena.md",
                "contact",
                "Elena",
                "2026-05-20T10:00:00Z",
                "status: active\n",
            ),
            (
                "records/profiles/sarah.md",
                "profile",
                "Sarah bio",
                "2026-05-21T10:00:00Z",
                "",
            ),
        ];

        for (rel, t, sum, upd, extra) in docs {
            write_doc(&wt, rel, t, Some(sum), Some(upd), extra);
            write_doc(&rb, rel, t, Some(sum), Some(upd), extra);
            Index::on_write(&wt, Path::new(rel)).unwrap();
        }
        Index::rebuild_all(&rb).unwrap();

        let a = snapshot_artifacts(&wt);
        let b = snapshot_artifacts(&rb);
        assert_eq!(
            a.keys().collect::<Vec<_>>(),
            b.keys().collect::<Vec<_>>(),
            "same set of index artifacts must exist"
        );
        for (k, v) in &a {
            assert_eq!(v, &b[k], "artifact {k} differs between write-through and rebuild:\n--- write-through ---\n{v}\n--- rebuild ---\n{}", b[k]);
        }
        // Sanity: artifacts actually exist (not a vacuous comparison of empties).
        assert!(a.contains_key("index.md"));
        assert!(a.contains_key("sources/emails/index.jsonl"));
        assert!(a.contains_key("records/contacts/index.md"));
    }

    /// Regression (O(changed) bound, not just correctness): a loop op must
    /// recompute its parent rollups from the type-folder `index.jsonl` sidecars
    /// — never by walking the content tree of *sibling* folders it wasn't asked
    /// about. The byte-identity property test (which always indexes every folder
    /// before comparing) can't catch a violation, because a full-store walk
    /// produces the *correct* counts too; it just does so in `O(store files)`.
    ///
    /// The behavioral fingerprint of the old `update_parents → build_layer /
    /// build_root` (which called `walk_type_folder_files` on every type-folder in
    /// the store): a single `on_write` to `records/contacts/sarah.md` would
    /// surface, in the layer + root rollups, the file count of
    /// `records/companies` — a sibling that has content on disk but was NEVER
    /// passed to a write/index op, so it has no `index.jsonl`. An O(changed) loop
    /// op cannot "see" that un-indexed folder; a whole-store walk can. So this
    /// asserts the rollups reflect ONLY the sidecar-indexed folder, proving no
    /// content-tree walk happened.
    #[test]
    fn loop_op_does_not_walk_sibling_content_tree() {
        let (_d, store) = mk_store();

        // A sibling type-folder with real content on disk, but deliberately
        // never indexed (no on_write / write_level / rebuild over it) ⇒ no
        // `records/companies/index.jsonl` exists.
        write_doc(
            &store,
            "records/companies/acme.md",
            "company",
            Some("Acme Inc"),
            Some("2026-05-05T00:00:00Z"),
            "",
        );
        write_doc(
            &store,
            "records/companies/globex.md",
            "company",
            Some("Globex"),
            Some("2026-05-06T00:00:00Z"),
            "",
        );
        assert!(
            !exists(&store, "records/companies/index.jsonl"),
            "precondition: companies must be un-indexed"
        );

        // The ONLY loop op: a single write to a different type-folder.
        write_doc(
            &store,
            "records/contacts/sarah.md",
            "contact",
            Some("Sarah"),
            Some("2026-05-15T00:00:00Z"),
            "",
        );
        Index::on_write(&store, Path::new("records/contacts/sarah.md")).unwrap();

        // The written folder is reflected in both rollups...
        let layer_md = read(&store, "records/index.md");
        let root_md = read(&store, "index.md");
        // (both rollups show counts only — no `## Folders` here, so no preview)
        assert!(
            layer_md.contains("- [[records/contacts/index|Contacts]] (1)\n")
                && !layer_md.contains("Sarah"),
            "layer must reflect the written folder, counts only:\n{layer_md}"
        );
        assert!(
            root_md.contains("- [[records/contacts/index|Contacts]] (1)\n"),
            "root must reflect the written folder:\n{root_md}"
        );

        // ...but the un-indexed sibling must be INVISIBLE to a loop op. If the
        // rollups mention `records/companies` at all, `on_write` walked the whole
        // content tree — the O(store) regression.
        assert!(
            !layer_md.contains("companies"),
            "loop op walked the sibling content tree: layer rollup counts un-indexed records/companies\n{layer_md}"
        );
        assert!(
            !root_md.contains("companies"),
            "loop op walked the sibling content tree: root rollup counts un-indexed records/companies\n{root_md}"
        );
        // The layer's only child is contacts ⇒ its total is exactly 1, not 3.
        assert!(
            root_md.contains("## Records (1)"),
            "root layer total must count only the sidecar-indexed folder (1), not walked siblings (would be 3):\n{root_md}"
        );

        // And the sidecar-derived count IS what a full walk WOULD yield once the
        // sibling is indexed too — i.e. the fix changes cost, not the eventual
        // result. Index companies, then confirm the rollups now (and only now)
        // include it, byte-identical to a from-scratch rebuild.
        let (_d2, rb) = mk_store();
        for (rel, t, s, u) in [
            (
                "records/companies/acme.md",
                "company",
                "Acme Inc",
                "2026-05-05T00:00:00Z",
            ),
            (
                "records/companies/globex.md",
                "company",
                "Globex",
                "2026-05-06T00:00:00Z",
            ),
            (
                "records/contacts/sarah.md",
                "contact",
                "Sarah",
                "2026-05-15T00:00:00Z",
            ),
        ] {
            write_doc(&rb, rel, t, Some(s), Some(u), "");
        }
        Index::on_write(&store, Path::new("records/companies/acme.md")).unwrap();
        Index::on_write(&store, Path::new("records/companies/globex.md")).unwrap();
        Index::rebuild_all(&rb).unwrap();
        let a = snapshot_artifacts(&store);
        let b = snapshot_artifacts(&rb);
        assert_eq!(
            a.keys().collect::<BTreeSet<_>>(),
            b.keys().collect::<BTreeSet<_>>(),
            "same artifact set after indexing both folders"
        );
        for (k, v) in &a {
            assert_eq!(
                v, &b[k],
                "after indexing the sibling too, loop result must equal rebuild for {k}"
            );
        }
        assert!(
            read(&store, "index.md").contains("## Records (3)"),
            "now that both folders are indexed, the root total is 3"
        );
    }

    /// Regression: a type filed at the path the toolkit ITSELF computes
    /// (`Store::shard_path_for`) must be indexable end-to-end. The class of bug
    /// is a 2-component `<layer>/<file>` path, which `type_folder_of` treats as
    /// having no type-folder — making the producer (path computation) disagree
    /// with the consumer (index): the loop path crashes (`on_write` → `Err`, it
    /// tries to write `index.md` *inside* a file) while the sweep path silently
    /// drops the page from every catalog. A conclusion `profile` is a custom
    /// (non-built-in) type, so `shard_path_for` files it under the records-layer
    /// fallback `records/profile/<file>` — a conforming 3-component path. This test
    /// drives both paths through the real `shard_path_for` output and asserts
    /// (1) `on_write` succeeds, (2) the page appears in the rebuilt catalog, and
    /// (3) write-through == rebuild.
    #[test]
    fn custom_type_at_shard_path_for_is_indexable_end_to_end() {
        let (_d1, wt) = mk_store();
        let (_d2, rb) = mk_store();

        // The toolkit's own canonical write path for a custom-type record.
        let rel = wt
            .shard_path_for(
                "profile",
                &crate::parser::Frontmatter::default(),
                "renewal-theme",
            )
            .unwrap();
        let rel_str = path_to_unix(&rel);
        // Guard the precondition the consumer requires: 3+ components so
        // `type_folder_of` resolves a real `<layer>/<type-folder>`.
        assert!(
            type_folder_of(&rel).is_some(),
            "shard_path_for produced a path the index cannot file: {rel_str}"
        );

        write_doc(
            &wt,
            &rel_str,
            "profile",
            Some("Renewal theme"),
            Some("2026-05-21T10:00:00Z"),
            "",
        );
        write_doc(
            &rb,
            &rel_str,
            "profile",
            Some("Renewal theme"),
            Some("2026-05-21T10:00:00Z"),
            "",
        );

        // (1) Loop path must NOT error (a 2-component `<layer>/<file>` shape
        // returned Err(Io(NotADirectory))).
        Index::on_write(&wt, &rel)
            .expect("on_write must succeed for a toolkit-computed custom-type path");
        Index::rebuild_all(&rb).unwrap();

        // (2) The page is present in the rebuilt catalog (the old flat-path bug
        // silently omitted it from every artifact). The individual page link
        // lives in the *type-folder* index; the *layer* index rolls the
        // type-folder up — assert both, since the bug erased both. A custom
        // type's canonical folder is the records-layer fallback `records/profile`.
        let page_link = wiki_target(&rel); // records/profile/renewal-theme
        let tf_md = read(&rb, "records/profile/index.md");
        assert!(
            tf_md.contains(&format!("[[{page_link}]]")),
            "type-folder index must list the page link, got:\n{tf_md}"
        );
        assert!(
            exists(&rb, "records/profile/index.jsonl"),
            "type-folder jsonl must exist"
        );
        assert!(
            read(&rb, "records/profile/index.jsonl").contains(&rel_str),
            "type-folder jsonl must contain the page row"
        );
        // The layer index rolls the type-folder up (proves the page's folder is
        // visible to the layer catalog, not dropped).
        let layer_md = read(&rb, "records/index.md");
        assert!(
            layer_md.contains("records/profile/index"),
            "layer index must roll up the records/profile type-folder, got:\n{layer_md}"
        );

        // (3) Write-through equals rebuild byte-for-byte — loop and sweep agree.
        let a = snapshot_artifacts(&wt);
        let b = snapshot_artifacts(&rb);
        assert_eq!(
            a.keys().collect::<Vec<_>>(),
            b.keys().collect::<Vec<_>>(),
            "loop and sweep must produce the same artifact set"
        );
        for (k, v) in &a {
            assert_eq!(
                v, &b[k],
                "custom-type artifact {k} differs between on_write and rebuild"
            );
        }
    }

    #[test]
    fn on_remove_then_rebuild_match_and_pull_in_next_over_cap() {
        let (_d1, wt) = mk_store();
        let (_d2, rb) = mk_store();
        let total = MD_CAP + 3; // 503 files; removing one keeps md full at 500
        let mut all_rels = Vec::new();
        for i in 0..total {
            let rel = format!("sources/emails/2026/05/m-{i:04}.md");
            // `updated` strictly increasing across i by varying both minute and second
            let updated = format!("2026-05-10T00:{:02}:{:02}Z", i / 60, i % 60);
            write_doc(
                &wt,
                &rel,
                "email",
                Some(&format!("mail {i}")),
                Some(&updated),
                "",
            );
            write_doc(
                &rb,
                &rel,
                "email",
                Some(&format!("mail {i}")),
                Some(&updated),
                "",
            );
            all_rels.push(rel);
        }
        // Build write-through index, then remove the single newest file.
        Index::rebuild_all(&wt).unwrap();
        let newest = &all_rels[total - 1]; // highest i = newest updated
        fs::remove_file(wt.root.join(newest)).unwrap();
        Index::on_remove(&wt, Path::new(newest)).unwrap();

        // Rebuild side: same end state (file physically absent).
        fs::remove_file(rb.root.join(newest)).unwrap();
        Index::rebuild_all(&rb).unwrap();

        let a = snapshot_artifacts(&wt);
        let b = snapshot_artifacts(&rb);
        for (k, v) in &a {
            assert_eq!(v, &b[k], "after remove, artifact {k} drifted from rebuild");
        }

        // The md must still hold exactly 500 entries (the 501st got pulled in)
        // and the removed file must be gone from both artifacts.
        let md = read(&wt, "sources/emails/index.md");
        assert_eq!(md.lines().filter(|l| l.starts_with("- [[")).count(), MD_CAP);
        // Removed (newest) file is gone from the bare-path md and the .md jsonl.
        assert!(
            !md.contains(&format!("[[{}]]", wiki_target(Path::new(newest)))),
            "removed file must not be listed in md"
        );
        // The file previously at rank 501 (excluded under the cap) is `all_rels[2]`
        // — `updated` increases with index, so newest-first rank 500 = index 2.
        // After dropping the newest it shifts into the visible 500.
        let pulled_in = &all_rels[2];
        assert!(
            md.contains(&format!("[[{}]]", wiki_target(Path::new(pulled_in)))),
            "the 501st-most-recent must be pulled into the browse view after a removal"
        );
        assert!(
            md.contains(&format!("This folder has {} files.", total - 1)),
            "footer count must decrement:\n{}",
            md.lines().rev().take(4).collect::<Vec<_>>().join("\n")
        );
        let jsonl = read(&wt, "sources/emails/index.jsonl");
        assert_eq!(
            jsonl.lines().count(),
            total - 1,
            "jsonl loses exactly the removed file"
        );
        assert!(
            !jsonl.contains(&path_to_unix(Path::new(newest))),
            "removed file must be gone from the jsonl too"
        );
    }

    #[test]
    fn on_rename_cross_folder_matches_rebuild() {
        let (_d1, wt) = mk_store();
        let (_d2, rb) = mk_store();
        // Seed both stores identically.
        let seed: &[(&str, &str, &str, &str)] = &[
            (
                "records/contacts/a.md",
                "contact",
                "A",
                "2026-05-01T00:00:00Z",
            ),
            (
                "records/contacts/b.md",
                "contact",
                "B",
                "2026-05-02T00:00:00Z",
            ),
            (
                "records/companies/x.md",
                "company",
                "X",
                "2026-05-03T00:00:00Z",
            ),
        ];
        for (rel, t, s, u) in seed {
            write_doc(&wt, rel, t, Some(s), Some(u), "");
            write_doc(&rb, rel, t, Some(s), Some(u), "");
        }
        Index::rebuild_all(&wt).unwrap();

        // Rename contacts/b.md -> companies/b.md (cross type-folder). The file's
        // `type` changes to match its new folder, as a real `dbmd rename` would.
        let old = "records/contacts/b.md";
        let new = "records/companies/b.md";
        fs::create_dir_all(wt.root.join("records/companies")).unwrap();
        fs::rename(wt.root.join(old), wt.root.join(new)).unwrap();
        // (type stays "contact" here; index copies frontmatter verbatim — the
        // test only asserts placement + parity with rebuild.)
        Index::on_rename(&wt, Path::new(old), Path::new(new)).unwrap();

        // Rebuild side: same end state.
        fs::create_dir_all(rb.root.join("records/companies")).unwrap();
        fs::rename(rb.root.join(old), rb.root.join(new)).unwrap();
        Index::rebuild_all(&rb).unwrap();

        let a = snapshot_artifacts(&wt);
        let b = snapshot_artifacts(&rb);
        assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>());
        for (k, v) in &a {
            assert_eq!(v, &b[k], "rename: artifact {k} drifted from rebuild");
        }
        // Concretely: b is gone from contacts, present in companies.
        let contacts = read(&wt, "records/contacts/index.md");
        assert!(!contacts.contains("records/contacts/b]]"));
        let companies = read(&wt, "records/companies/index.md");
        assert!(companies.contains("[[records/companies/b]]"));
    }

    #[test]
    fn on_write_updates_existing_entry_in_place() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "records/contacts/a.md",
            "contact",
            Some("Original"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        Index::on_write(&store, Path::new("records/contacts/a.md")).unwrap();
        // Edit the same file: new summary + newer updated.
        write_doc(
            &store,
            "records/contacts/a.md",
            "contact",
            Some("Revised"),
            Some("2026-05-09T00:00:00Z"),
            "",
        );
        Index::on_write(&store, Path::new("records/contacts/a.md")).unwrap();

        let jsonl = read(&store, "records/contacts/index.jsonl");
        assert_eq!(
            jsonl.lines().count(),
            1,
            "upsert must not duplicate the line"
        );
        assert!(jsonl.contains("Revised"), "jsonl must reflect the update");
        assert!(
            !jsonl.contains("Original"),
            "stale line must be gone (compacted)"
        );
        let md = read(&store, "records/contacts/index.md");
        assert!(md.contains("- [[records/contacts/a]] — Revised\n"));
        assert!(
            md.contains("updated: 2026-05-09T00:00:00Z\n"),
            "index updated must track the newer member"
        );
    }

    // ── dry-run + cleanup ────────────────────────────────────────────────

    #[test]
    fn dry_run_emits_separators_and_writes_nothing() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "sources/emails/2026/05/a.md",
            "email",
            Some("Mail"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        let out = Index::render_dry_run(&store, &IndexLevel::TypeFolder("sources/emails".into()))
            .unwrap();
        assert!(
            out.contains("--- sources/emails/index.md ---\n"),
            "md separator:\n{out}"
        );
        assert!(
            out.contains("--- sources/emails/index.jsonl ---\n"),
            "jsonl separator:\n{out}"
        );
        assert!(
            out.contains("- [[sources/emails/2026/05/a]] — Mail"),
            "md body present"
        );
        // Nothing was written to disk.
        assert!(
            !exists(&store, "sources/emails/index.md"),
            "dry-run must not write"
        );
        assert!(
            !exists(&store, "sources/emails/index.jsonl"),
            "dry-run must not write"
        );
    }

    #[test]
    fn cleanup_removes_noncanonical_and_empty_indexes() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "sources/emails/2026/05/a.md",
            "email",
            Some("Mail"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        // A stray index inside a date-shard (non-canonical) ...
        fs::write(
            store.root.join("sources/emails/2026/05/index.md"),
            "stale\n",
        )
        .unwrap();
        fs::write(
            store.root.join("sources/emails/2026/05/index.jsonl"),
            "stale\n",
        )
        .unwrap();
        // ... and an index in an empty type-folder.
        fs::create_dir_all(store.root.join("records/empty")).unwrap();
        fs::write(store.root.join("records/empty/index.md"), "stale\n").unwrap();

        Index::cleanup(&store).unwrap();

        assert!(
            !exists(&store, "sources/emails/2026/05/index.md"),
            "shard index must be deleted"
        );
        assert!(
            !exists(&store, "sources/emails/2026/05/index.jsonl"),
            "shard jsonl must be deleted"
        );
        assert!(
            !exists(&store, "records/empty/index.md"),
            "empty-folder index must be deleted"
        );
        // The canonical type-folder file itself is untouched by cleanup.
        assert!(exists(&store, "sources/emails/2026/05/a.md"));
    }

    #[test]
    fn rebuild_deletes_stale_indexes_for_emptied_folders() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "records/contacts/a.md",
            "contact",
            Some("A"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        Index::rebuild_all(&store).unwrap();
        assert!(exists(&store, "records/contacts/index.md"));
        assert!(exists(&store, "records/index.md"));
        assert!(exists(&store, "index.md"));

        // Empty the folder entirely, then rebuild: all three levels vanish.
        fs::remove_file(store.root.join("records/contacts/a.md")).unwrap();
        Index::rebuild_all(&store).unwrap();
        assert!(
            !exists(&store, "records/contacts/index.md"),
            "emptied type-folder index gone"
        );
        assert!(
            !exists(&store, "records/index.md"),
            "now-empty layer index gone"
        );
        assert!(!exists(&store, "index.md"), "now-empty root index gone");
    }

    // ── randomized parity (property-style) ───────────────────────────────

    #[test]
    fn property_writethrough_equals_rebuild_under_mixed_ops() {
        // Deterministic pseudo-random op sequence (no rand crate): a small LCG.
        let (_d1, wt) = mk_store();
        let (_d2, rb) = mk_store();
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };

        let folders = ["sources/emails", "records/contacts", "records/profiles"];
        let types = ["email", "contact", "profile"];
        let mut live: Vec<String> = Vec::new(); // store-relative paths that exist

        for step in 0..120u32 {
            let r = next();
            let op = r % 10;
            if op < 6 || live.is_empty() {
                // CREATE/UPDATE
                let fi = (next() as usize) % folders.len();
                let folder = folders[fi];
                let id = next() % 40;
                let rel = if folder == "sources/emails" {
                    let month = 5 + (id % 2); // shard across two months
                    format!("{folder}/2026/{month:02}/f-{id:02}.md")
                } else {
                    format!("{folder}/f-{id:02}.md")
                };
                // recency varies with step so order is meaningful + total
                let updated = format!(
                    "2026-05-{:02}T{:02}:{:02}:00Z",
                    1 + (step % 27),
                    step % 24,
                    id % 60
                );
                let extra = if id % 3 == 0 {
                    "tags:\n  - x\n  - y\n"
                } else {
                    ""
                };
                write_doc(
                    &wt,
                    &rel,
                    types[fi],
                    Some(&format!("sum {step}")),
                    Some(&updated),
                    extra,
                );
                write_doc(
                    &rb,
                    &rel,
                    types[fi],
                    Some(&format!("sum {step}")),
                    Some(&updated),
                    extra,
                );
                Index::on_write(&wt, Path::new(&rel)).unwrap();
                if !live.contains(&rel) {
                    live.push(rel);
                }
            } else if op < 8 {
                // REMOVE a live file
                let idx = (next() as usize) % live.len();
                let rel = live.remove(idx);
                fs::remove_file(wt.root.join(&rel)).unwrap();
                fs::remove_file(rb.root.join(&rel)).ok();
                Index::on_remove(&wt, Path::new(&rel)).unwrap();
            } else {
                // RENAME a live file within the same layer (new id, maybe new type-folder)
                let idx = (next() as usize) % live.len();
                let old = live[idx].clone();
                // pick a destination folder in the same layer-ish set
                let fi = (next() as usize) % folders.len();
                let folder = folders[fi];
                let id = 50 + (next() % 40);
                let new = if folder == "sources/emails" {
                    format!("{folder}/2026/05/f-{id:02}.md")
                } else {
                    format!("{folder}/f-{id:02}.md")
                };
                if new == old || live.contains(&new) {
                    continue;
                }
                fs::create_dir_all(wt.root.join(&new).parent().unwrap()).unwrap();
                fs::create_dir_all(rb.root.join(&new).parent().unwrap()).unwrap();
                fs::rename(wt.root.join(&old), wt.root.join(&new)).unwrap();
                fs::rename(rb.root.join(&old), rb.root.join(&new)).unwrap();
                Index::on_rename(&wt, Path::new(&old), Path::new(&new)).unwrap();
                live[idx] = new;
            }
        }

        // Now rebuild the rb side from the shared end state and compare.
        Index::rebuild_all(&rb).unwrap();
        let a = snapshot_artifacts(&wt);
        let b = snapshot_artifacts(&rb);
        assert_eq!(
            a.keys().collect::<BTreeSet<_>>(),
            b.keys().collect::<BTreeSet<_>>(),
            "write-through and rebuild must produce the same set of artifacts"
        );
        for (k, v) in &a {
            assert_eq!(
                v, &b[k],
                "INVARIANT VIOLATED: artifact {k} differs after mixed ops\n--- write-through ---\n{v}\n--- rebuild ---\n{}",
                b[k]
            );
        }
        assert!(
            !a.is_empty(),
            "the run must have produced at least one artifact"
        );
    }

    // ── regressions: cleanup must not delete user content ─────────────────

    /// CRITICAL regression: a user content file named `index.md` inside a date
    /// shard (e.g. from a website/doc-export mirror) must SURVIVE `cleanup` /
    /// `rebuild_all`. The old filename-only match silently deleted it.
    #[test]
    fn cleanup_preserves_user_content_named_index_md_in_shard() {
        let (_d, store) = mk_store();
        // A real content record that merely happens to be named index.md.
        write_doc(
            &store,
            "sources/emails/2026/06/index.md",
            "email",
            Some("Important imported mail"),
            Some("2026-06-11T04:23:25Z"),
            "",
        );
        Index::cleanup(&store).unwrap();
        assert!(
            exists(&store, "sources/emails/2026/06/index.md"),
            "cleanup must not delete a user content file named index.md"
        );
        // A full rebuild (which runs cleanup first) must also preserve it.
        Index::rebuild_all(&store).unwrap();
        assert!(
            exists(&store, "sources/emails/2026/06/index.md"),
            "rebuild_all must not delete a user content file named index.md"
        );
        let kept = read(&store, "sources/emails/2026/06/index.md");
        assert!(
            kept.contains("Important imported mail"),
            "the user's record content must be intact"
        );
    }

    /// HIGH regression: `cleanup` uses `min_depth(2)`, so the canonical
    /// type-folder-root `index.md`/`index.jsonl` are NOT deleted up front. A
    /// genuine generated catalog at the type-folder root survives a cleanup pass
    /// (it is only ever rewritten, or removed when the folder is truly empty).
    #[test]
    fn cleanup_keeps_canonical_type_folder_root_sidecars() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "records/contacts/alice.md",
            "contact",
            Some("Alice"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        Index::write_level(&store, &IndexLevel::TypeFolder("records/contacts".into())).unwrap();
        assert!(exists(&store, "records/contacts/index.md"));
        assert!(exists(&store, "records/contacts/index.jsonl"));
        Index::cleanup(&store).unwrap();
        assert!(
            exists(&store, "records/contacts/index.md"),
            "cleanup must keep the canonical type-folder index.md (non-empty folder)"
        );
        assert!(
            exists(&store, "records/contacts/index.jsonl"),
            "cleanup must keep the canonical type-folder index.jsonl (non-empty folder)"
        );
    }

    // ── regression: write-through must not catalog index artifacts ────────

    /// HIGH regression: routing a generated `index.md` through `on_write` (as
    /// `dbmd fm set records/contacts/index.md …` would) must NOT insert a phantom
    /// self-row — counts and bytes stay equal to a rebuild.
    #[test]
    fn on_write_ignores_index_artifact_no_phantom_row() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "records/contacts/alice.md",
            "contact",
            Some("Alice"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        Index::on_write(&store, Path::new("records/contacts/alice.md")).unwrap();
        let jsonl_before = read(&store, "records/contacts/index.jsonl");
        assert_eq!(jsonl_before.lines().count(), 1);

        // Tamper: route the catalog file itself through on_write.
        Index::on_write(&store, Path::new("records/contacts/index.md")).unwrap();

        let jsonl_after = read(&store, "records/contacts/index.jsonl");
        assert_eq!(
            jsonl_after.lines().count(),
            1,
            "on_write on index.md must not add a phantom self-row"
        );
        assert!(
            !jsonl_after.contains("\"type\":\"index\""),
            "the catalog artifact must never appear as a catalogued row"
        );
        // Root rollup count stays 1 (not inflated to 2).
        let root = read(&store, "index.md");
        assert!(
            root.contains("[[records/contacts/index|Contacts]] (1)"),
            "count must not inflate:\n{root}"
        );
    }

    // ── regression: multi-line summary cannot inject a catalog line ───────

    /// HIGH regression: a block-scalar summary spanning multiple lines must be
    /// collapsed to one line in the browse entry, so it cannot forge a standalone
    /// `- [[…]]` catalog line.
    #[test]
    fn multiline_summary_is_single_lined_in_index_md() {
        let (_d, store) = mk_store();
        // A YAML block scalar whose value embeds a forged-looking entry line.
        write_raw(
            &store,
            "records/notes/evil.md",
            "type: note\nupdated: 2026-06-10T00:00:00Z\nsummary: |-\n  legit first line\n  - [[records/secrets/fake|Click me]] — injected entry",
            "\nbody\n",
        );
        let idx = Index::build_type_folder(&store, Path::new("records/notes")).unwrap();
        let md = idx.to_markdown();
        // Exactly one browse entry line, and no embedded newline forging a second.
        let entry_lines = md.lines().filter(|l| l.starts_with("- [[")).count();
        assert_eq!(
            entry_lines, 1,
            "a multi-line summary must not produce extra entry lines:\n{md}"
        );
        assert!(
            md.contains(
                "- [[records/notes/evil]] — legit first line - [[records/secrets/fake|Click me]] — injected entry\n"
            ),
            "summary newlines must collapse to spaces inline:\n{md}"
        );
    }

    // ── regression: writer/validator scalar coercion agreement ────────────

    /// HIGH regression: an unquoted non-string scalar `summary`/`type`
    /// (`summary: 2026`, `type: true`) must be coerced to a string by the index
    /// writer exactly as `validate::scalar_string` does — so the index entry holds
    /// the real value (`2026`), not the `(no summary)` placeholder that produced a
    /// permanently-unfixable INDEX_SUMMARY_MISMATCH.
    #[test]
    fn non_string_scalar_summary_and_type_are_coerced_like_validator() {
        let (_d, store) = mk_store();
        write_raw(
            &store,
            "records/contacts/a.md",
            "type: contact\nupdated: 2026-05-01T00:00:00Z\nsummary: 2026",
            "\nbody\n",
        );
        let rec = record_from_file(
            &store.root.join("records/contacts/a.md"),
            PathBuf::from("records/contacts/a.md"),
        )
        .unwrap();
        // `summary: 2026` (YAML number) coerces to the string "2026", matching
        // the validator's `scalar_string` (Number -> n.to_string()).
        assert_eq!(rec.summary, "2026");
        assert_eq!(rec.type_, "contact");

        // And the rendered index entry quotes the real value, not the placeholder.
        let idx = Index::build_type_folder(&store, Path::new("records/contacts")).unwrap();
        let md = idx.to_markdown();
        assert!(
            md.contains("- [[records/contacts/a]] — 2026\n"),
            "index entry must hold the coerced scalar, not the placeholder:\n{md}"
        );

        // A boolean scalar type coerces to "true" (mirrors scalar_string(Bool)).
        write_raw(
            &store,
            "records/contacts/b.md",
            "type: true\nupdated: 2026-05-02T00:00:00Z\nsummary: hi",
            "\nbody\n",
        );
        let rec_b = record_from_file(
            &store.root.join("records/contacts/b.md"),
            PathBuf::from("records/contacts/b.md"),
        )
        .unwrap();
        assert_eq!(rec_b.type_, "true");
    }

    // ── regression: non-UTF-8 body must not abort the projection ──────────

    /// HIGH regression: a content file with valid-UTF-8 frontmatter but a
    /// non-UTF-8 byte in the BODY (a verbatim Latin-1 `sources/` import) must
    /// still project to an IndexRecord — `record_from_file` reads frontmatter
    /// without requiring the whole file to be UTF-8, so a stray byte can't abort
    /// `rebuild_all` / write-through for the entire store.
    #[test]
    fn non_utf8_body_does_not_abort_record_projection() {
        let (_d, store) = mk_store();
        let rel = "sources/emails/2026/06/x.md";
        let abs = store.root.join(rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        // Valid-UTF-8 frontmatter; a raw 0xE9 (Latin-1 'é') in the body.
        let mut bytes: Vec<u8> =
            b"---\ntype: email\nupdated: 2026-06-11T00:00:00Z\nsummary: An imported email\n---\n\nCaf"
                .to_vec();
        bytes.push(0xE9);
        bytes.extend_from_slice(b" meeting notes\n");
        fs::write(&abs, bytes).unwrap();

        let rec = record_from_file(&abs, PathBuf::from(rel))
            .expect("non-UTF-8 body must not abort the frontmatter read");
        assert_eq!(rec.summary, "An imported email");
        assert_eq!(rec.type_, "email");

        // The full sweep indexes the folder rather than aborting the whole store.
        Index::rebuild_all(&store).unwrap();
        assert!(
            exists(&store, "sources/emails/index.jsonl"),
            "rebuild must produce the catalog despite a non-UTF-8 body byte"
        );
        assert!(
            read(&store, "sources/emails/index.jsonl").contains("An imported email"),
            "the record must be catalogued"
        );
    }

    /// HIGH regression: a single malformed-YAML file must abort the rebuild
    /// loudly (not be silently skipped) — skipping it would leave the store in a
    /// permanently invalid state (`INDEX_MISSING_ENTRY` / `INDEX_JSONL_DESYNC`
    /// that no rebuild clears, since the validator enumerates members by
    /// filename, not by parseability) and would desync the rollups. The abort is
    /// safe because `cleanup` preserves the prior canonical catalogs
    /// (`min_depth(2)`), so an aborted rebuild leaves the existing sidecars
    /// intact and surfaces a clear error naming the file to fix.
    #[test]
    fn rebuild_aborts_on_malformed_file_and_keeps_prior_catalogs() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "records/contacts/alice.md",
            "contact",
            Some("Alice"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        write_doc(
            &store,
            "records/companies/acme.md",
            "company",
            Some("Acme"),
            Some("2026-05-02T00:00:00Z"),
            "",
        );

        // A clean first rebuild establishes the canonical catalogs.
        Index::rebuild_all(&store).expect("clean rebuild succeeds");
        assert!(exists(&store, "records/contacts/index.jsonl"));
        assert!(exists(&store, "records/companies/index.jsonl"));

        // Routine malformed file: unterminated quoted scalar.
        let bad = store.root.join("records/contacts/broken.md");
        fs::write(
            &bad,
            "---\ntype: contact\nsummary: \"unterminated\n---\nbody\n",
        )
        .unwrap();

        // Must abort loudly — a silent skip leaves a file the validator requires
        // to be catalogued out of the index forever.
        Index::rebuild_all(&store)
            .expect_err("rebuild must abort, not silently skip, on a malformed file");

        // The prior canonical catalogs survive the aborted rebuild: `cleanup`'s
        // `min_depth(2)` never deletes a type-folder's root-level sidecars, so a
        // mid-sweep abort leaves the existing indexes intact rather than wiped.
        assert!(
            exists(&store, "records/companies/index.jsonl"),
            "an aborted rebuild must not destroy a clean sibling folder's catalog"
        );
        assert!(
            exists(&store, "records/contacts/index.jsonl"),
            "an aborted rebuild must not destroy the affected folder's prior catalog"
        );
        let contacts_jsonl = read(&store, "records/contacts/index.jsonl");
        assert!(contacts_jsonl.contains("records/contacts/alice.md"));
    }

    /// HIGH regression (problem B): `rebuild_all`'s rollup `(N)` counts must
    /// equal the catalogued `index.jsonl` record counts — never a raw `.md` walk
    /// that disagrees with the sidecar. The over-corrected skip-with-diagnostic
    /// build excluded a malformed file from `index.jsonl` while `build_layer` /
    /// `build_root` kept counting it via `walk_type_folder_files`, so a folder
    /// would show `Contacts (2)` in the root/layer rollups while its `index.jsonl`
    /// held only 1 record — and a single subsequent write-through (which derives
    /// `(N)` from the jsonl) rewrote it to `Contacts (1)`, making `rebuild_all`
    /// and write-through emit different bytes for the same state. With the loud
    /// abort, the only successful-rebuild states are fully consistent: every
    /// rollup `(N)` equals the catalogued record count AND equals what a
    /// write-through over the same files produces.
    #[test]
    fn rebuild_rollup_counts_equal_jsonl_records_and_write_through() {
        let (_d, store) = mk_store();
        // Two well-formed contacts: the rollups must read (2), matching the two
        // jsonl records — this is the count the skip-version inflated to a phantom
        // extra when a malformed sibling was present-but-uncatalogued.
        write_doc(
            &store,
            "records/contacts/alice.md",
            "contact",
            Some("Alice"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        write_doc(
            &store,
            "records/contacts/bob.md",
            "contact",
            Some("Bob"),
            Some("2026-05-02T00:00:00Z"),
            "",
        );
        Index::rebuild_all(&store).expect("clean rebuild succeeds");

        // The catalogued record set (index.jsonl) and the rollup (N) must agree.
        let jsonl_lines = read(&store, "records/contacts/index.jsonl")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        assert_eq!(jsonl_lines, 2, "two well-formed files ⇒ two jsonl records");
        let layer_md = read(&store, "records/index.md");
        let root_md = read(&store, "index.md");
        assert!(
            layer_md.contains("- [[records/contacts/index|Contacts]] (2)"),
            "layer rollup (N) must equal the jsonl record count (2), not a raw .md walk:\n{layer_md}"
        );
        assert!(
            root_md.contains("- [[records/contacts/index|Contacts]] (2)\n")
                && root_md.contains("## Records (2)"),
            "root rollup (N)/layer total must equal the jsonl record count (2):\n{root_md}"
        );

        // The decisive write-through == rebuild_all byte-identity check on the
        // SAME end state: a single on_write must not rewrite the rollups to a
        // different (N). Under the skip-version, rebuild_all's rollup walked the
        // raw .md tree while on_write derived (N) from the jsonl, so the two
        // diverged; the loud abort keeps both deriving (N) from the catalogued
        // records, so the bytes match exactly.
        let (_d2, wt) = mk_store();
        write_doc(
            &wt,
            "records/contacts/alice.md",
            "contact",
            Some("Alice"),
            Some("2026-05-01T00:00:00Z"),
            "",
        );
        write_doc(
            &wt,
            "records/contacts/bob.md",
            "contact",
            Some("Bob"),
            Some("2026-05-02T00:00:00Z"),
            "",
        );
        Index::on_write(&wt, Path::new("records/contacts/alice.md")).unwrap();
        Index::on_write(&wt, Path::new("records/contacts/bob.md")).unwrap();

        let a = snapshot_artifacts(&wt);
        let b = snapshot_artifacts(&store);
        assert_eq!(
            a.keys().collect::<BTreeSet<_>>(),
            b.keys().collect::<BTreeSet<_>>(),
            "write-through and rebuild_all must produce the same artifact set"
        );
        for (k, v) in &a {
            assert_eq!(
                v, &b[k],
                "rollup bytes diverged between write-through and rebuild_all for {k} \
                 (a skip-version inflates rebuild_all's (N) above the jsonl record \
                 count, which write-through then rewrites):\n--- write-through ---\n{v}\n--- rebuild ---\n{}",
                b[k]
            );
        }
    }

    /// MEDIUM regression: a non-UTF-8 path component must be lossily decoded
    /// (kept, with U+FFFD), not silently dropped — so the index key points at the
    /// file, not its parent directory. Unix-only (ext4 allows the filename; APFS
    /// rejects it at the VFS layer).
    #[cfg(unix)]
    #[test]
    fn non_utf8_path_component_is_kept_not_dropped() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        // sources/emails/caf\xE9.md — the leaf has a non-UTF-8 byte.
        let mut leaf = b"caf".to_vec();
        leaf.push(0xE9);
        leaf.extend_from_slice(b".md");
        let p = Path::new("sources/emails").join(OsStr::from_bytes(&leaf));
        let unix = path_to_unix(&p);
        // The leaf is preserved (lossy), so the path is NOT collapsed to the
        // parent directory "sources/emails".
        assert_ne!(
            unix, "sources/emails",
            "non-UTF-8 leaf must not be dropped, collapsing the path to its parent dir"
        );
        assert!(
            unix.starts_with("sources/emails/caf"),
            "the lossy leaf must remain under its folder: {unix}"
        );
    }

    // ── loose files (directly at a layer root, no type-folder) ───────────────

    #[test]
    fn loose_file_is_catalogued_in_layer_jsonl_not_type_folder() {
        let (_d, store) = mk_store();
        // One canonical file (in a type-folder) and one loose file at the root.
        write_doc(
            &store,
            "records/contacts/alice.md",
            "contact",
            Some("Alice"),
            Some("2026-06-01T08:00:00Z"),
            "id: alice\n",
        );
        write_doc(
            &store,
            "records/loose.md",
            "contact",
            Some("Loose"),
            Some("2026-06-01T08:00:00Z"),
            "id: loose\n",
        );
        Index::rebuild_all(&store).unwrap();

        // The layer carries its own jsonl listing exactly the loose file —
        // disjoint from the type-folder jsonl, so no double-count.
        assert!(
            exists(&store, "records/index.jsonl"),
            "layer jsonl must exist when loose files are present"
        );
        let layer_jsonl = read(&store, "records/index.jsonl");
        assert!(
            layer_jsonl.contains("records/loose.md"),
            "layer jsonl must list the loose file, got:\n{layer_jsonl}"
        );
        assert!(
            !layer_jsonl.contains("records/contacts/alice.md"),
            "layer jsonl must NOT list type-folder files"
        );
        let tf_jsonl = read(&store, "records/contacts/index.jsonl");
        assert!(tf_jsonl.contains("records/contacts/alice.md"));
        assert!(!tf_jsonl.contains("records/loose.md"));

        // The layer index.md stays a pure type-folder rollup — no loose entry.
        let layer_md = read(&store, "records/index.md");
        assert!(
            layer_md.contains("records/contacts/index"),
            "layer md must roll up the type-folder, got:\n{layer_md}"
        );
        assert!(
            !layer_md.contains("records/loose"),
            "layer md must stay a rollup, not list loose files, got:\n{layer_md}"
        );
    }

    #[test]
    fn loose_file_write_through_equals_rebuild() {
        let (_d1, wt) = mk_store();
        let (_d2, rb) = mk_store();
        for s in [&wt, &rb] {
            write_doc(
                s,
                "records/contacts/alice.md",
                "contact",
                Some("Alice"),
                Some("2026-06-01T08:00:00Z"),
                "id: alice\n",
            );
            write_doc(
                s,
                "records/loose.md",
                "contact",
                Some("Loose"),
                Some("2026-06-02T08:00:00Z"),
                "id: loose\n",
            );
        }
        // wt: write-through (loop); rb: full rebuild (sweep). Must agree byte-wise.
        Index::on_write(&wt, Path::new("records/contacts/alice.md")).unwrap();
        Index::on_write(&wt, Path::new("records/loose.md")).unwrap();
        Index::rebuild_all(&rb).unwrap();

        let a = snapshot_artifacts(&wt);
        let b = snapshot_artifacts(&rb);
        assert_eq!(
            a.keys().collect::<Vec<_>>(),
            b.keys().collect::<Vec<_>>(),
            "loose-file loop and sweep must produce the same artifact set"
        );
        for (k, v) in &a {
            assert_eq!(
                v, &b[k],
                "loose-file artifact {k} differs between loop and sweep"
            );
        }
    }

    #[test]
    fn removing_last_loose_file_clears_layer_jsonl() {
        let (_d, store) = mk_store();
        write_doc(
            &store,
            "records/loose.md",
            "contact",
            Some("Loose"),
            Some("2026-06-01T08:00:00Z"),
            "id: loose\n",
        );
        Index::on_write(&store, Path::new("records/loose.md")).unwrap();
        assert!(
            exists(&store, "records/index.jsonl"),
            "layer jsonl present after a loose write"
        );
        fs::remove_file(store.root.join("records/loose.md")).unwrap();
        Index::on_remove(&store, Path::new("records/loose.md")).unwrap();
        assert!(
            !exists(&store, "records/index.jsonl"),
            "layer jsonl must be removed once the last loose file is gone"
        );
    }
}
