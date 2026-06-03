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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, SecondsFormat};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// `(N)` counts and a newest-file `summary` preview (≤ 80 chars).
    pub fn build_layer(store: &Store, layer: Layer) -> crate::Result<Index> {
        let mut child_counts = BTreeMap::new();
        for tf in type_folders_in_layer(store, layer) {
            let abs = store.root.join(&tf);
            let n = walk_type_folder_files(&abs).len();
            if n > 0 {
                child_counts.insert(tf, n);
            }
        }
        Ok(Index {
            level: IndexLevel::Layer(layer),
            records: Vec::new(),
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

    /// Render this type-folder catalog as the complete `index.jsonl` (one JSON
    /// object per file, stable key order so diffs stay minimal). Type-folder
    /// level only — root and layer stay markdown rollups.
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
        let file_abs = store.root.join(&file_rel);
        let folder = type_folder_of(&file_rel)
            .ok_or_else(|| bad_index(&file_rel, "file is not inside a layer/type-folder"))?;
        let record = record_from_file(&file_abs, file_rel.clone())?;

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
        let old_folder = type_folder_of(&old_rel)
            .ok_or_else(|| bad_index(&old_rel, "source is not inside a layer/type-folder"))?;
        let new_folder = type_folder_of(&new_rel)
            .ok_or_else(|| bad_index(&new_rel, "target is not inside a layer/type-folder"))?;

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
        let folder = type_folder_of(&file_rel)
            .ok_or_else(|| bad_index(&file_rel, "file is not inside a layer/type-folder"))?;
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
    /// `index.jsonl` in non-canonical folders (empty folders, or date-shards
    /// that should carry none). Symmetric with index creation.
    pub fn cleanup(store: &Store) -> crate::Result<()> {
        for layer in Layer::all() {
            let layer_dir = store.root.join(layer_dir_name(layer));
            if !layer_dir.is_dir() {
                continue;
            }
            for tf in type_folders_in_layer(store, layer) {
                let tf_abs = store.root.join(&tf);
                // Any index inside a shard (below the type-folder root) is
                // non-canonical: delete it.
                for entry in walkdir::WalkDir::new(&tf_abs)
                    .min_depth(1)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    let p = entry.path();
                    if is_index_artifact(p) {
                        remove_if_exists(p)?;
                    }
                }
                // Empty type-folder → no index at its root either.
                if walk_type_folder_files(&tf_abs).is_empty() {
                    remove_if_exists(&tf_abs.join("index.md"))?;
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
/// **loop path**, O(changed). Counts come from the type-folders' on-disk
/// `index.jsonl` sidecars ([`child_counts_from_jsonl`]), NOT from a content-tree
/// walk: a single write touches only the affected layer's sidecars (for the
/// layer rollup) and one sidecar per type-folder (for the root rollup) — never
/// the millions of files under the shards. `build_layer` / `build_root` (which
/// *do* walk the content tree) are reserved for the from-scratch sweeps
/// ([`Index::rebuild_all`], [`Index::write_level`], [`Index::render_dry_run`]).
/// The result is byte-identical to those builders because in the loop — exactly
/// as in `rebuild_all` — every touched folder's jsonl is rewritten before its
/// parents are rolled up, so `jsonl_record_count == walk_type_folder_files.len()`
/// for every folder read here.
fn update_parents(store: &Store, folder: &Path) -> crate::Result<()> {
    let layer = folder
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .and_then(layer_from_dir_name);
    if let Some(layer) = layer {
        let idx = Index {
            level: IndexLevel::Layer(layer),
            records: Vec::new(),
            child_counts: child_counts_from_jsonl(store, &[layer])?,
        };
        let p = store.root.join(layer_dir_name(layer)).join("index.md");
        if idx.child_counts.is_empty() {
            remove_if_exists(&p)?;
        } else {
            write_atomic(&p, render_layer_md_with_store(store, &idx))?;
        }
    }
    let root = Index {
        level: IndexLevel::Root,
        records: Vec::new(),
        child_counts: child_counts_from_jsonl(store, &Layer::all())?,
    };
    let rp = store.root.join("index.md");
    if root.child_counts.is_empty() {
        remove_if_exists(&rp)?;
    } else {
        write_atomic(&rp, render_root_md_with_store(store, &root))?;
    }
    Ok(())
}

/// Render a layer `index.md`, reading each child's newest summary + max-updated
/// straight from its on-disk `index.jsonl` (so the rollup matches the folder
/// artifacts exactly, write-through and rebuild alike).
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
        let display = capitalize(folder_basename(tf));
        let preview = newest
            .map(|r| truncate(&r.summary, 80))
            .filter(|p| !p.is_empty() && p != MISSING_SUMMARY);
        match preview {
            Some(p) => entries.push_str(&format!("- [[{tf_unix}/index|{display}]] ({n}) — {p}\n")),
            None => entries.push_str(&format!("- [[{tf_unix}/index|{display}]] ({n})\n")),
        }
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
/// `index.jsonl`.
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
            let display = capitalize(folder_basename(tf));
            s.push_str(&format!("- [[{tf_unix}/index|{display}]] ({n})\n"));
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
    let mut line = format!("- [[{path}]] — {}", rec.summary);
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
    records.sort_by(|a, b| {
        match (b.updated, a.updated) {
            (Some(bu), Some(au)) => bu.cmp(&au),
            (Some(_), None) => std::cmp::Ordering::Greater, // a is None → after b
            (None, Some(_)) => std::cmp::Ordering::Less,    // b is None → after a
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.path.cmp(&b.path))
    });
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
    let meta = read_frontmatter(abs)?;
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
fn read_frontmatter(abs: &Path) -> crate::Result<FileMeta> {
    let text = fs::read_to_string(abs)?;
    let yaml = extract_frontmatter_block(&text).unwrap_or_default();
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
            "type" => type_ = v.as_str().map(str::to_string),
            "summary" => summary = v.as_str().map(str::to_string),
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

/// Count the distinct content files a type-folder's `index.jsonl` catalogs —
/// the **loop-path** count primitive, the rollup analogue of reading the
/// per-folder sidecar. It reads only the one small sidecar (one line per file),
/// never the content tree, so a rollup recompute over `K` type-folders is
/// `O(K · folder)` sidecar reads — never `O(store files)` like
/// [`walk_type_folder_files`]. Distinct-`path` (last-write-wins) so the count is
/// byte-identical to [`read_jsonl_records`]`.len()` even on a half-compacted
/// jsonl; a missing sidecar is `0`. Within the loop and within
/// [`Index::rebuild_all`] the folder's jsonl is always rewritten before its
/// parents are rolled up, so this equals `walk_type_folder_files(folder).len()`.
fn jsonl_record_count(jsonl: &Path) -> crate::Result<usize> {
    let text = match fs::read_to_string(jsonl) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut paths: BTreeSet<PathBuf> = BTreeSet::new();
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
        paths.insert(rec.path);
    }
    Ok(paths.len())
}

/// Per-child rollup counts for `layers`, read from each type-folder's on-disk
/// `index.jsonl` (via [`jsonl_record_count`]) rather than walked from the
/// content tree. The **loop-path** counterpart to the from-scratch counting in
/// [`Index::build_layer`] / [`Index::build_root`]: it keeps [`update_parents`]
/// `O(type-folders)` so a single write never re-enumerates the whole store.
fn child_counts_from_jsonl(
    store: &Store,
    layers: &[Layer],
) -> crate::Result<BTreeMap<PathBuf, usize>> {
    let mut child_counts = BTreeMap::new();
    for &layer in layers {
        for tf in type_folders_in_layer(store, layer) {
            let n = jsonl_record_count(&store.root.join(&tf).join("index.jsonl"))?;
            if n > 0 {
                child_counts.insert(tf, n);
            }
        }
    }
    Ok(child_counts)
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

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_str().map(|s| s.starts_with('.')).unwrap_or(false)
}

fn layer_dir_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Sources => "sources",
        Layer::Records => "records",
        Layer::Wiki => "wiki",
    }
}

/// Local layer-name parse. Mirrors the contract of [`Layer::from_dir_name`];
/// kept local to keep this module's walk self-contained (see the module header).
fn layer_from_dir_name(name: &str) -> Option<Layer> {
    match name {
        "sources" => Some(Layer::Sources),
        "records" => Some(Layer::Records),
        "wiki" => Some(Layer::Wiki),
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
fn path_to_unix(p: &Path) -> String {
    p.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// ASCII-capitalize the first character.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Truncate to at most `max` chars (char-boundary safe), single-line.
fn truncate(s: &str, max: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        one_line.chars().take(max).collect()
    }
}

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
            "created: 2026-05-10T09:00:00Z\nstatus: paid\namount: 42\ncompany: [[records/companies/acme]]\nrelated:\n  - [[wiki/themes/spend]]\ntags:\n  - food\nlinks:\n  - wiki/themes/spend\n  - [[wiki/themes/renewal]]\n",
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
                "wiki/themes/spend".to_string(),
                "[[wiki/themes/renewal]]".to_string()
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
            Some(&serde_json::json!(["[[wiki/themes/spend]]"]))
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
                r#"{"path":"records/expenses/2026/05/e1.md","type":"expense","summary":"Lunch with vendor","tags":["food"],"links":["wiki/themes/spend","[[wiki/themes/renewal]]"],"created":"2026-05-10T09:00:00Z","updated":"2026-05-10T10:00:00Z","#
            ),
            "jsonl key order not stable:\n{}",
            lines[1]
        );
        // The flattened extras come in BTreeMap (sorted) order.
        assert!(
            lines[1].ends_with(r#""amount":42,"company":"[[records/companies/acme]]","related":["[[wiki/themes/spend]]"],"status":"paid"}"#),
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
    fn layer_index_lists_type_folders_with_counts_and_preview() {
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
        // Count + display + newest-summary preview.
        assert!(
            md.contains("- [[records/contacts/index|Contacts]] (2) — Contact B newest\n"),
            "contacts entry:\n{md}"
        );
        assert!(
            md.contains("- [[records/companies/index|Companies]] (1) — Acme Inc\n"),
            "companies entry:\n{md}"
        );
        // Layer `updated` is the max across children (contacts b = 05-09).
        assert!(
            md.contains("updated: 2026-05-09T00:00:00Z\n"),
            "layer updated must be max child:\n{md}"
        );
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
                "links:\n  - wiki/people/sarah\n",
            ),
            (
                "records/contacts/elena.md",
                "contact",
                "Elena",
                "2026-05-20T10:00:00Z",
                "status: active\n",
            ),
            (
                "wiki/people/sarah.md",
                "wiki-page",
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
        // (layer rollup appends a summary preview, root does not)
        assert!(
            layer_md.contains("- [[records/contacts/index|Contacts]] (1) — Sarah\n"),
            "layer must reflect the written folder:\n{layer_md}"
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

    /// Regression: a wiki-page filed at the path the toolkit ITSELF computes
    /// (`Store::shard_path_for`) must be indexable end-to-end. The bug was that
    /// `shard_path_for("wiki-page", …)` returned a 2-component `wiki/<file>`
    /// path, which `type_folder_of` treats as having no type-folder. That made
    /// the producer (path computation) disagree with the consumer (index): the
    /// loop path crashed (`on_write` → `Err`, it tried to write `index.md`
    /// *inside* a file) while the sweep path silently dropped the page from
    /// every catalog. This test drives both paths through the real
    /// `shard_path_for` output and asserts (1) `on_write` succeeds, (2) the page
    /// appears in the rebuilt catalog, and (3) write-through == rebuild.
    #[test]
    fn wiki_page_at_shard_path_for_is_indexable_end_to_end() {
        let (_d1, wt) = mk_store();
        let (_d2, rb) = mk_store();

        // The toolkit's own canonical write path for a wiki-page.
        let rel = wt
            .shard_path_for(
                "wiki-page",
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
            "wiki-page",
            Some("Renewal theme"),
            Some("2026-05-21T10:00:00Z"),
            "",
        );
        write_doc(
            &rb,
            &rel_str,
            "wiki-page",
            Some("Renewal theme"),
            Some("2026-05-21T10:00:00Z"),
            "",
        );

        // (1) Loop path must NOT error (the old `wiki/<file>` shape returned
        // Err(Io(NotADirectory))).
        Index::on_write(&wt, &rel)
            .expect("on_write must succeed for a toolkit-computed wiki-page path");
        Index::rebuild_all(&rb).unwrap();

        // (2) The page is present in the rebuilt catalog (the old flat-path bug
        // silently omitted it from every artifact). The individual page link
        // lives in the *type-folder* index; the *layer* index rolls the
        // type-folder up — assert both, since the bug erased both.
        let page_link = wiki_target(&rel); // wiki/topics/renewal-theme
        let tf_md = read(&rb, "wiki/topics/index.md");
        assert!(
            tf_md.contains(&format!("[[{page_link}]]")),
            "type-folder index must list the page link, got:\n{tf_md}"
        );
        assert!(
            exists(&rb, "wiki/topics/index.jsonl"),
            "type-folder jsonl must exist"
        );
        assert!(
            read(&rb, "wiki/topics/index.jsonl").contains(&rel_str),
            "type-folder jsonl must contain the page row"
        );
        // The layer index rolls the type-folder up (proves the page's folder is
        // visible to the layer catalog, not dropped).
        let layer_md = read(&rb, "wiki/index.md");
        assert!(
            layer_md.contains("wiki/topics/index"),
            "layer index must roll up the wiki/topics type-folder, got:\n{layer_md}"
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
                "wiki-page artifact {k} differs between on_write and rebuild"
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

        let folders = ["sources/emails", "records/contacts", "wiki/people"];
        let types = ["email", "contact", "wiki-page"];
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
}
