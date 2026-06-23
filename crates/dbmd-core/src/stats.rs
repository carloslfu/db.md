//! `stats` — store overview, **computed on demand** (a SWEEP, like `du` —
//! never a maintained or precomputed cache).
//!
//! Serves both the human (how big is my brain, what's the shape) and the agent
//! (orientation). Deliberately excludes graph density / degree / top-linked
//! analytics — low agent value, and a human who wants graph metrics opens the
//! store in Obsidian, so we never build the full graph just for stats.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::store::{Layer, Store};

/// A point-in-time overview of a store. Pure data; the CLI formats it to text
/// or JSON.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    /// Total content-file count across all layers.
    pub total_files: usize,
    /// File count per layer.
    pub files_per_layer: BTreeMap<Layer, usize>,
    /// Total size on disk, in bytes.
    pub total_size_bytes: u64,
    /// Count per `type:` value (the type distribution).
    pub type_distribution: BTreeMap<String, usize>,
    /// Number of orphan files (no incoming and no outgoing wiki-links).
    pub orphan_count: usize,
    /// Number of broken wiki-links (target file doesn't exist).
    pub broken_link_count: usize,
    /// Top types by count, descending (ties broken by type name ascending).
    pub top_types: Vec<(String, usize)>,
}

/// How many entries [`Stats::top_types`] holds.
const TOP_TYPES_LIMIT: usize = 10;

/// One content file discovered by the SWEEP, with everything `stats` needs:
/// where it lives, how big it is, its declared `type`, and the wiki-link
/// targets it emits (store-relative, `.md` stripped, short-form excluded).
struct FileFacts {
    /// Store-relative path *without* the `.md` extension — the node id used to
    /// resolve wiki-links and detect orphans.
    node_id: PathBuf,
    /// The layer this file lives under.
    layer: Layer,
    /// File size on disk, in bytes.
    size_bytes: u64,
    /// The declared `type:`, if the frontmatter has one.
    type_: Option<String>,
    /// Every wiki-link target this file emits, store-relative with any trailing
    /// `.md` stripped, in source order (not deduped, short-form included).
    /// Resolved against the complete node set in a second pass.
    raw_targets: Vec<PathBuf>,
}

impl FileFacts {
    /// The subset of [`raw_targets`](FileFacts::raw_targets) that could resolve
    /// to a store node: full store-relative paths. Short-form targets (no `/`)
    /// are dropped — they're a `WIKI_LINK_SHORT_FORM` validation error, not a
    /// graph edge, so stats neither counts them as broken nor lets them wire a
    /// file out of orphan status.
    fn resolvable_targets(&self) -> impl Iterator<Item = &PathBuf> {
        self.raw_targets.iter().filter(|t| is_full_path(t))
    }
}

/// **SWEEP.** Walk the store once and compute its [`Stats`]. Run occasionally
/// (overview / orientation), never on the interactive loop.
pub fn compute(store: &Store) -> crate::Result<Stats> {
    let link_re = wiki_link_regex();

    // First pass: walk every layer once, recording per-file facts and the set
    // of node ids that exist on disk. Link resolution waits for the second
    // pass, once every node's existence is known.
    let mut existing_nodes: HashSet<PathBuf> = HashSet::new();
    let mut facts: Vec<FileFacts> = Vec::new();

    for layer in Layer::all() {
        let layer_root = store.root.join(layer_dir_name(layer));
        for abs in walk_layer_content_files(&layer_root)? {
            let rel = abs.strip_prefix(&store.root).unwrap_or(&abs).to_path_buf();
            let node_id = strip_md(&rel);
            existing_nodes.insert(node_id.clone());

            let size_bytes = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
            let text = std::fs::read_to_string(&abs).unwrap_or_default();
            let type_ = parse_type(&text);
            let raw_targets = extract_link_targets(&text, &link_re);

            facts.push(FileFacts {
                node_id,
                layer,
                size_bytes,
                type_,
                raw_targets,
            });
        }
    }

    // Second pass: classify every file's links against the complete node set,
    // counting broken links (full-path targets with no file on disk) and
    // recording which nodes receive an incoming edge. Short-form targets are a
    // validation error elsewhere, not a stats edge, so they're skipped here:
    // they neither wire a file in nor count as broken.
    let mut stats = Stats::default();
    let mut linked_to: HashSet<PathBuf> = HashSet::new();
    for file in &facts {
        for target in file.resolvable_targets() {
            // A self-link is not a graph edge — skip it (matches `graph::orphans`,
            // so the two surfaces agree on whether a self-only-linking file is an
            // orphan). It is neither incoming nor broken.
            if target == &file.node_id {
                continue;
            }
            if existing_nodes.contains(target) {
                linked_to.insert(target.clone());
            } else if target_resolves_on_disk(&store.root, target) {
                // A link to an existing non-`.md` source artifact (a `.eml`,
                // `.pdf`, …) is a live edge, not a broken one — `sources/` holds
                // such files by design and `graph` resolves them on disk. The
                // target has no `.md` node, so it can't be `linked_to` (no `.md`
                // file is un-orphaned by it), but it must NOT be counted broken.
            } else {
                // Broken links count occurrences, not distinct targets.
                stats.broken_link_count += 1;
            }
        }
    }

    // Third pass: roll the per-file facts up into the aggregate Stats. A file is
    // an orphan iff it has neither a resolvable outgoing edge nor an incoming one.
    for file in &facts {
        stats.total_files += 1;
        *stats.files_per_layer.entry(file.layer).or_insert(0) += 1;
        stats.total_size_bytes += file.size_bytes;

        if let Some(t) = &file.type_ {
            *stats.type_distribution.entry(t.clone()).or_insert(0) += 1;
        }

        let has_outgoing = file.resolvable_targets().any(|t| {
            t != &file.node_id
                && (existing_nodes.contains(t) || target_resolves_on_disk(&store.root, t))
        });
        let has_incoming = linked_to.contains(&file.node_id);
        if !has_outgoing && !has_incoming {
            stats.orphan_count += 1;
        }
    }

    stats.top_types = top_types(&stats.type_distribution, TOP_TYPES_LIMIT);

    Ok(stats)
}

/// On-disk folder name for a layer. Local copy so `stats` doesn't couple to
/// [`Layer::dir_name`].
fn layer_dir_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Sources => "sources",
        Layer::Records => "records",
    }
}

/// Recursively collect the `.md` **content** files under one layer root,
/// skipping hidden entries (`.git`, dotfiles), the layer's immediate `log/`
/// archive directory, and the `index.md` catalog meta files. Returns absolute
/// paths. A missing layer root yields an empty list (a store need not have all
/// three layers).
///
/// Only an immediate child of the layer named `log` (`sources/log/`) is the
/// rotation-archive directory and skipped — matching `render::tree`, which
/// skips `log` only as an immediate layer child, and the indexer, which indexes
/// `log` dirs nested deeper. A directory named `log` nested under a type-folder
/// (`sources/emails/log/`) is ordinary content and is counted, so stats agrees
/// with `tree` / `index` / `query` instead of making the subtree invisible.
fn walk_layer_content_files(layer_root: &Path) -> crate::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !layer_root.is_dir() {
        return Ok(out);
    }
    let walker = walkdir::WalkDir::new(layer_root)
        .into_iter()
        .filter_entry(|e| {
            // Skip hidden dirs/files. `depth()` is relative to the layer root
            // (root = 0), so the layer's immediate `log/` archive is depth 1.
            let name = e.file_name().to_string_lossy();
            if name.starts_with('.') {
                return false;
            }
            if e.file_type().is_dir() && name == "log" && e.depth() == 1 {
                return false;
            }
            true
        });
    for entry in walker {
        let entry = entry.map_err(|e| {
            crate::Error::Io(
                e.into_io_error()
                    .unwrap_or_else(|| std::io::Error::other("walk error")),
            )
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy();
        // Content files are `.md`; `index.md` is a meta catalog file, not
        // content, and `index.jsonl` / other sidecars aren't `.md` at all.
        if !name.ends_with(".md") || name == "index.md" {
            continue;
        }
        out.push(path.to_path_buf());
    }
    out.sort();
    Ok(out)
}

/// The wiki-link matcher: `[[target]]` or `[[target|display]]`. Captures the
/// target (group 1), excluding `]` and `|`. Anchored on the literal brackets so
/// it ignores `[markdown](links)`.
fn wiki_link_regex() -> Regex {
    // `[^\[\]|]+` keeps the target free of brackets and the display pipe.
    Regex::new(r"\[\[([^\[\]|]+)(?:\|[^\]]*)?\]\]").expect("static wiki-link regex is valid")
}

/// Every wiki-link target in a file's full text (frontmatter + body), trimmed,
/// with any trailing `.md` removed. Order-preserving; not deduped.
///
/// Fenced code blocks (```/~~~) are skipped, mirroring
/// `validate::extract_wiki_links`: a `[[...]]` that lives only inside a code
/// fence is illustrative syntax in a doc, not a graph edge, so stats must not
/// count it as broken or use it to un-orphan a file. (Frontmatter never carries
/// code fences, so this scan stays line-based over the whole file without
/// dropping the frontmatter links stats deliberately counts as edges.)
fn extract_link_targets(text: &str, re: &Regex) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // Track the open fence as `(fence byte, run length)`, not a single boolean:
    // an inner fence of the *other* character (a `~~~` line inside an open ```
    // block, or vice versa) — or a shorter run — is content, and must NOT close
    // the block. A naive toggle inverts the fence state on such a line and then
    // mis-classifies every link for the rest of the file. Mirrors `render`'s
    // `opening_fence` / `is_closing_fence`.
    let mut fence: Option<(u8, usize)> = None;
    for line in text.lines() {
        let content = line.trim_end_matches(['\n', '\r']);
        if let Some(f) = fence {
            if is_closing_fence(content, f) {
                fence = None;
            }
            continue;
        }
        if let Some(opened) = opening_fence(content) {
            fence = Some(opened);
            continue;
        }
        for cap in re.captures_iter(line) {
            if let Some(m) = cap.get(1) {
                let raw = m.as_str().trim();
                out.push(strip_md(Path::new(raw)));
            }
        }
    }
    out
}

/// If `line` opens a fenced code block, return its `(fence byte, run length)`.
/// A fence is at least three backticks or tildes, with up to three leading
/// spaces of indentation. Mirrors `render::opening_fence`.
fn opening_fence(line: &str) -> Option<(u8, usize)> {
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

/// True if `line` closes the currently open fence `(byte, len)`: same fence
/// char, a run at least as long, and nothing else but trailing whitespace.
/// Mirrors `render::is_closing_fence`.
fn is_closing_fence(line: &str, fence: (u8, usize)) -> bool {
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

/// Drop a trailing `.md` from a path, leaving everything else intact.
fn strip_md(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    match s.strip_suffix(".md") {
        Some(stem) => PathBuf::from(stem),
        None => path.to_path_buf(),
    }
}

/// True if a wiki-link target is a full store-relative path: it has a path
/// separator AND its first segment is a recognized layer (`sources`/`records`/
/// `wiki`) with a non-empty remainder. Short-form targets like `sarah-chen`
/// are false, and so are non-layer multi-segment targets like
/// `contacts/sarah-chen` (a missing layer prefix). Doctrine: only true
/// store-relative paths resolve to a node.
///
/// This mirrors `validate::is_full_store_path` so `stats.broken_link_count`
/// agrees with `validate`'s `WIKI_LINK_BROKEN` total: a non-layer target like
/// `[[contacts/sarah]]` is a short-form error in `validate` (never broken), and
/// must likewise be excluded here rather than counted as a broken edge.
fn is_full_path(target: &Path) -> bool {
    let mut parts = target.components();
    let first = match parts.next() {
        Some(std::path::Component::Normal(s)) => s.to_string_lossy(),
        _ => return false,
    };
    let has_rest = parts.next().is_some();
    matches!(first.as_ref(), "sources" | "records") && has_rest
}

/// True if `target` stays inside the store: every component is `Normal` (a
/// `CurDir` `.` is harmless and allowed), with no `..` (`ParentDir`), absolute
/// (`RootDir`), or platform-prefix component. Mirrors
/// `graph::is_within_store_target` and validate's `is_safe_store_relative_path`,
/// so the containment decision is identical across the three surfaces. Used to
/// gate any on-disk probe in [`target_resolves_on_disk`] before a `join`.
fn is_within_store_target(target: &Path) -> bool {
    target.components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
}

/// True if a full-path wiki-link `target` (already `.md`-stripped, store-
/// relative) resolves to a real **non-`.md`** file on disk — a source artifact
/// like a `.eml` or `.pdf` under `sources/`. Called only after the `.md` node
/// set has already been checked, so this exists to reconcile stats with `graph`
/// (which resolves on disk) and `validate`: a link to an existing source file
/// is a live edge, never a broken link or an orphan-maker.
///
/// Two on-disk shapes are recognized, mirroring `graph::resolve_existing` plus
/// the bare-stem case sources use:
///
/// - the target as written is itself a real file (`[[sources/emails/msg.eml]]`
///   → `sources/emails/msg.eml`);
/// - the target is a bare stem and a sibling file shares that stem with a
///   non-`.md` extension (`[[sources/emails/msg]]` → `sources/emails/msg.eml`).
///
/// A bare `.md` target is *not* handled here (an existing `.md` file is already
/// a node in `existing_nodes`); this is strictly the non-`.md` source case.
///
/// **Containment gate.** A target that escapes the store root (any `..`,
/// absolute, or platform-prefix component) is never probed: it returns `false`
/// before any `join`/`is_file`/`read_dir`, so `[[sources/../../secret]]` can
/// never reach the filesystem as a live edge or existence oracle outside the
/// store. This mirrors `graph::is_within_store_target` and validate's
/// `is_safe_store_relative_path` (which reject `..` before any probe), keeping
/// the broken-link surface in agreement: an escaping target is counted broken
/// (validate's `WIKI_LINK_BROKEN`), never silently treated as resolved.
fn target_resolves_on_disk(store_root: &Path, target: &Path) -> bool {
    // Reject any non-`Normal` component (`..`, RootDir, Prefix) up front — never
    // let a wiki-link turn a stats probe into a filesystem escape.
    if !is_within_store_target(target) {
        return false;
    }
    // The target as written points at a real file (e.g. an explicit `.eml`).
    let literal = store_root.join(target);
    if literal.is_file() {
        return true;
    }
    // Bare-stem case: look for a sibling `<stem>.<ext>` with a non-`.md`
    // extension in the target's parent directory. Restricted to the bare form
    // (no extension on the target) so an explicit but missing `.pdf` link still
    // reads as broken rather than silently matching a different file.
    if target.extension().is_some() {
        return false;
    }
    let stem = match target.file_name() {
        Some(name) => name,
        None => return false,
    };
    let parent_abs = store_root.join(match target.parent() {
        Some(p) => p,
        None => return false,
    });
    let entries = match std::fs::read_dir(&parent_abs) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // Same stem, and an extension that is present and not `.md`.
        if path.file_stem() == Some(stem) {
            match path.extension().and_then(|e| e.to_str()) {
                Some("md") | None => continue,
                Some(_) => return true,
            }
        }
    }
    false
}

/// Read the `type:` value from a file's leading YAML frontmatter block, if the
/// file has one. Returns `None` when there's no frontmatter or no `type` key.
/// Self-contained (does not route through the crate's parser): split on the
/// `---` fences, parse the block as a YAML mapping, read `type` as a string.
fn parse_type(text: &str) -> Option<String> {
    let yaml = frontmatter_block(text)?;
    let value: serde_norway::Value = serde_norway::from_str(&yaml).ok()?;
    let mapping = value.as_mapping()?;
    let type_val = mapping.get(serde_norway::Value::String("type".to_string()))?;
    let s = type_val.as_str()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Extract the raw YAML between a leading `---` fence and its closing `---`.
/// The opening fence must be the very first line of the file (the universal
/// frontmatter contract: frontmatter is the first thing in the file).
fn frontmatter_block(text: &str) -> Option<String> {
    // Normalize away a leading BOM, but require `---` as the first line.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut body = String::new();
    for line in lines {
        if line.trim_end() == "---" {
            return Some(body);
        }
        body.push_str(line);
        body.push('\n');
    }
    // No closing fence: not a valid frontmatter block.
    None
}

/// Sort a type distribution into the top `limit` types by count descending,
/// ties broken by type name ascending.
fn top_types(dist: &BTreeMap<String, usize>, limit: usize) -> Vec<(String, usize)> {
    let mut pairs: Vec<(String, usize)> = dist.iter().map(|(k, v)| (k.clone(), *v)).collect();
    // BTreeMap iteration is already name-ascending; a stable sort by count
    // descending therefore yields (count desc, name asc).
    pairs.sort_by_key(|p| std::cmp::Reverse(p.1));
    pairs.truncate(limit);
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Config;
    use std::fs;
    use tempfile::TempDir;

    /// Build a `Store` rooted at a fresh tempdir with an empty `DB.md` marker.
    /// Bypasses `Store::open` by constructing the struct directly —
    /// `stats::compute` only reads `store.root`.
    fn temp_store() -> (TempDir, Store) {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join("DB.md"), "---\ntype: db-md\n---\n").expect("write DB.md");
        let store = Store {
            root: dir.path().to_path_buf(),
            config: Config::default(),
        };
        (dir, store)
    }

    /// Like [`temp_store`], but roots the store one level *inside* the tempdir
    /// (`<tempdir>/store`) so `store.root.parent()` is the test's own private
    /// tempdir rather than the shared OS temp root. Tests that plant a file
    /// "above the store root" must use this — writing into `store.root.parent()`
    /// of a top-level `TempDir` lands in `$TMPDIR`, which is shared across every
    /// parallel test (and across test binaries under `cargo test --workspace`),
    /// so two such tests collide on the same path and race.
    fn temp_store_nested() -> (TempDir, Store) {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("store");
        fs::create_dir_all(&root).expect("create store root");
        fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n").expect("write DB.md");
        let store = Store {
            root,
            config: Config::default(),
        };
        (dir, store)
    }

    /// Write a content file at a store-relative path, creating parent dirs.
    fn write_rel(store: &Store, rel: &str, contents: &str) {
        let abs = store.root.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).expect("mkdir parents");
        }
        fs::write(abs, contents).expect("write content file");
    }

    /// A minimal content file body: frontmatter with the given type, no links.
    fn doc(type_: &str, summary: &str) -> String {
        format!("---\ntype: {type_}\nsummary: \"{summary}\"\n---\n\nbody\n")
    }

    #[test]
    fn empty_store_is_all_zeros() {
        let (_d, store) = temp_store();
        let s = compute(&store).expect("compute");
        assert_eq!(s.total_files, 0);
        assert_eq!(s.total_size_bytes, 0);
        assert!(s.files_per_layer.is_empty());
        assert!(s.type_distribution.is_empty());
        assert_eq!(s.orphan_count, 0);
        assert_eq!(s.broken_link_count, 0);
        assert!(s.top_types.is_empty());
    }

    #[test]
    fn counts_files_per_layer_and_total() {
        let (_d, store) = temp_store();
        write_rel(&store, "sources/emails/a.md", &doc("email", "a"));
        write_rel(&store, "sources/emails/b.md", &doc("email", "b"));
        write_rel(&store, "records/contacts/c.md", &doc("contact", "c"));
        // A conclusion record (former wiki-page) lives in the records layer.
        write_rel(&store, "records/profiles/p.md", &doc("profile", "p"));

        let s = compute(&store).expect("compute");
        assert_eq!(s.total_files, 4);
        assert_eq!(s.files_per_layer.get(&Layer::Sources), Some(&2));
        assert_eq!(s.files_per_layer.get(&Layer::Records), Some(&2));
    }

    #[test]
    fn ignores_meta_files_and_non_md_and_dotdirs_and_log() {
        let (_d, store) = temp_store();
        // Real content.
        write_rel(&store, "records/contacts/real.md", &doc("contact", "real"));
        // Meta + non-content that must NOT be counted.
        write_rel(
            &store,
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\n---\n",
        );
        write_rel(&store, "records/contacts/index.jsonl", "{}\n");
        write_rel(&store, "records/notes.txt", "not markdown\n");
        // `log/` archive tree under a layer is skipped wholesale.
        write_rel(&store, "sources/log/2026-04.md", &doc("email", "archived"));
        // Hidden dir contents are skipped.
        write_rel(
            &store,
            "records/.obsidian/cache.md",
            &doc("profile", "hidden"),
        );

        let s = compute(&store).expect("compute");
        assert_eq!(s.total_files, 1, "only the one real content file counts");
        assert_eq!(s.files_per_layer.get(&Layer::Records), Some(&1));
        assert_eq!(s.files_per_layer.get(&Layer::Sources), None);
    }

    #[test]
    fn total_size_is_sum_of_content_file_bytes() {
        let (_d, store) = temp_store();
        let a = doc("email", "a");
        let b = "---\ntype: contact\nsummary: x\n---\n\nlonger body text here\n".to_string();
        write_rel(&store, "sources/emails/a.md", &a);
        write_rel(&store, "records/contacts/b.md", &b);
        // A skipped file's bytes must not be included.
        write_rel(
            &store,
            "records/contacts/index.md",
            "---\ntype: index\n---\nbig meta file padding padding\n",
        );

        let s = compute(&store).expect("compute");
        let expected = a.len() as u64 + b.len() as u64;
        assert_eq!(s.total_size_bytes, expected);
    }

    #[test]
    fn type_distribution_counts_each_type_value() {
        let (_d, store) = temp_store();
        write_rel(&store, "sources/emails/a.md", &doc("email", "a"));
        write_rel(&store, "sources/emails/b.md", &doc("email", "b"));
        write_rel(&store, "sources/emails/c.md", &doc("email", "c"));
        write_rel(&store, "records/contacts/d.md", &doc("contact", "d"));
        write_rel(&store, "records/proposals/e.md", &doc("proposal", "e"));

        let s = compute(&store).expect("compute");
        assert_eq!(s.type_distribution.get("email"), Some(&3));
        assert_eq!(s.type_distribution.get("contact"), Some(&1));
        assert_eq!(s.type_distribution.get("proposal"), Some(&1));
        assert_eq!(s.type_distribution.len(), 3);
    }

    #[test]
    fn file_without_type_is_counted_in_totals_but_not_distribution() {
        let (_d, store) = temp_store();
        // A content file with frontmatter but no `type:` key.
        write_rel(
            &store,
            "records/themes/x.md",
            "---\nsummary: no type here\n---\n\nbody\n",
        );
        // A content file with no frontmatter at all.
        write_rel(
            &store,
            "records/themes/y.md",
            "just a body, no frontmatter\n",
        );

        let s = compute(&store).expect("compute");
        assert_eq!(s.total_files, 2, "untyped files still count toward totals");
        assert_eq!(s.files_per_layer.get(&Layer::Records), Some(&2));
        assert!(
            s.type_distribution.is_empty(),
            "no type key => no distribution entry, not an empty-string bucket"
        );
    }

    #[test]
    fn top_types_orders_by_count_desc_then_name_asc() {
        let (_d, store) = temp_store();
        // contact x3, email x3 (tie), decision x1.
        write_rel(&store, "records/contacts/c1.md", &doc("contact", "1"));
        write_rel(&store, "records/contacts/c2.md", &doc("contact", "2"));
        write_rel(&store, "records/contacts/c3.md", &doc("contact", "3"));
        write_rel(&store, "sources/emails/e1.md", &doc("email", "1"));
        write_rel(&store, "sources/emails/e2.md", &doc("email", "2"));
        write_rel(&store, "sources/emails/e3.md", &doc("email", "3"));
        write_rel(&store, "records/decisions/d1.md", &doc("decision", "1"));

        let s = compute(&store).expect("compute");
        assert_eq!(
            s.top_types,
            vec![
                ("contact".to_string(), 3),
                ("email".to_string(), 3),
                ("decision".to_string(), 1),
            ],
            "ties (contact, email both 3) break by name ascending; decision trails"
        );
    }

    #[test]
    fn top_types_is_capped_at_ten() {
        let (_d, store) = temp_store();
        // 12 distinct custom types, each one file.
        for i in 0..12 {
            let t = format!("type{i:02}");
            write_rel(&store, &format!("records/{t}/f.md"), &doc(&t, "x"));
        }
        let s = compute(&store).expect("compute");
        assert_eq!(s.top_types.len(), 10, "top_types caps at 10");
        assert_eq!(
            s.type_distribution.len(),
            12,
            "distribution keeps all types"
        );
    }

    #[test]
    fn orphans_are_files_with_no_incoming_and_no_outgoing_links() {
        let (_d, store) = temp_store();
        // a -> b (a has outgoing, b has incoming). c is isolated => orphan.
        write_rel(
            &store,
            "records/contacts/a.md",
            "---\ntype: contact\nsummary: a\n---\n\nSee [[records/contacts/b]].\n",
        );
        write_rel(&store, "records/contacts/b.md", &doc("contact", "b"));
        write_rel(&store, "records/contacts/c.md", &doc("contact", "c"));

        let s = compute(&store).expect("compute");
        assert_eq!(s.orphan_count, 1, "only c is an orphan");
    }

    #[test]
    fn a_file_with_only_a_self_link_is_an_orphan_matching_graph() {
        let (_d, store) = temp_store();
        // A file that links only to ITSELF has no real graph edge, so it must be
        // an orphan — consistent with `graph::orphans` (which skips self-links).
        write_rel(
            &store,
            "records/contacts/solo.md",
            "---\ntype: contact\nsummary: solo\n---\n\nSee [[records/contacts/solo]].\n",
        );
        let s = compute(&store).expect("compute");
        assert_eq!(
            s.orphan_count, 1,
            "a self-only-linking file is an orphan: {s:?}"
        );
    }

    #[test]
    fn a_file_with_only_an_incoming_link_is_not_an_orphan() {
        let (_d, store) = temp_store();
        // b has no outgoing links, but a links to it => b is NOT an orphan.
        // a itself has an outgoing link => also not an orphan. Zero orphans.
        write_rel(
            &store,
            "wiki/people/a.md",
            "---\ntype: wiki-page\nsummary: a\n---\n\n[[wiki/people/b]]\n",
        );
        write_rel(&store, "wiki/people/b.md", &doc("wiki-page", "b"));

        let s = compute(&store).expect("compute");
        assert_eq!(s.orphan_count, 0);
    }

    #[test]
    fn frontmatter_wiki_links_count_as_edges_for_orphans() {
        let (_d, store) = temp_store();
        // The link lives in a frontmatter field, not the body. It must still
        // wire `contact` -> `company`, so neither is an orphan.
        write_rel(
            &store,
            "records/contacts/sarah.md",
            "---\ntype: contact\nsummary: s\ncompany: [[records/companies/acme]]\n---\n\nbody\n",
        );
        write_rel(&store, "records/companies/acme.md", &doc("company", "acme"));

        let s = compute(&store).expect("compute");
        assert_eq!(
            s.orphan_count, 0,
            "a frontmatter wiki-link is a real edge; neither endpoint is orphaned"
        );
    }

    #[test]
    fn broken_links_count_targets_that_do_not_exist() {
        let (_d, store) = temp_store();
        // Two links: one to an existing file, one to a missing file.
        write_rel(
            &store,
            "records/profiles/a.md",
            "---\ntype: profile\nsummary: a\n---\n\n[[records/profiles/b]] and [[records/contacts/ghost]]\n",
        );
        write_rel(&store, "records/profiles/b.md", &doc("profile", "b"));

        let s = compute(&store).expect("compute");
        assert_eq!(s.broken_link_count, 1, "only the ghost target is broken");
    }

    #[test]
    fn broken_link_resolves_with_md_extension_stripped() {
        let (_d, store) = temp_store();
        // Link written WITH a `.md` extension still resolves to the real file
        // (the parser accepts `.md`; validate only warns). Not broken.
        write_rel(
            &store,
            "wiki/people/a.md",
            "---\ntype: wiki-page\nsummary: a\n---\n\n[[wiki/people/b.md]]\n",
        );
        write_rel(&store, "wiki/people/b.md", &doc("wiki-page", "b"));

        let s = compute(&store).expect("compute");
        assert_eq!(
            s.broken_link_count, 0,
            "a `.md`-suffixed target resolves to the same node and is not broken"
        );
    }

    #[test]
    fn short_form_links_are_not_broken_and_do_not_wire_the_graph() {
        let (_d, store) = temp_store();
        // `[[b]]` is a short-form (no `/`): a validation error elsewhere, but
        // for stats it neither counts as broken (it doesn't resolve to a node)
        // nor wires `a` into the graph. So `a` (no other links) is an orphan.
        write_rel(
            &store,
            "records/contacts/a.md",
            "---\ntype: contact\nsummary: a\n---\n\n[[b]]\n",
        );
        write_rel(&store, "records/contacts/b.md", &doc("contact", "b"));

        let s = compute(&store).expect("compute");
        assert_eq!(
            s.broken_link_count, 0,
            "short-form links are not counted as broken by stats"
        );
        // a has only a short-form link (not an edge) => orphan. b has no links
        // and no real incoming edge => orphan. Both orphaned.
        assert_eq!(s.orphan_count, 2);
    }

    #[test]
    fn display_alias_links_resolve_to_the_target_not_the_alias() {
        let (_d, store) = temp_store();
        // `[[wiki/people/b|Bob]]` targets b, displays "Bob". The alias must be
        // stripped: the edge goes to b (exists), so it's not broken and b is
        // not an orphan.
        write_rel(
            &store,
            "wiki/people/a.md",
            "---\ntype: wiki-page\nsummary: a\n---\n\nmet [[wiki/people/b|Bob]] today\n",
        );
        write_rel(&store, "wiki/people/b.md", &doc("wiki-page", "b"));

        let s = compute(&store).expect("compute");
        assert_eq!(s.broken_link_count, 0, "alias target resolves and exists");
        assert_eq!(s.orphan_count, 0, "a links out, b is linked to");
    }

    #[test]
    fn duplicate_links_in_one_file_count_broken_per_occurrence() {
        let (_d, store) = temp_store();
        // The same missing target twice => two broken-link occurrences.
        write_rel(
            &store,
            "records/profiles/a.md",
            "---\ntype: profile\nsummary: a\n---\n\n[[records/contacts/ghost]] [[records/contacts/ghost]]\n",
        );
        let s = compute(&store).expect("compute");
        assert_eq!(
            s.broken_link_count, 2,
            "broken links count occurrences, not distinct targets"
        );
    }

    #[test]
    fn markdown_links_are_not_treated_as_wiki_links() {
        let (_d, store) = temp_store();
        // A standard markdown link to an external URL must not register as a
        // wiki edge (so this file stays an orphan) nor as a broken link.
        write_rel(
            &store,
            "records/profiles/a.md",
            "---\ntype: profile\nsummary: a\n---\n\nSee [Acme](https://acme.io/path).\n",
        );
        let s = compute(&store).expect("compute");
        assert_eq!(s.broken_link_count, 0, "markdown links aren't graph edges");
        assert_eq!(s.orphan_count, 1, "the file has no wiki-links => orphan");
    }

    #[test]
    fn regression_non_layer_multi_segment_link_is_not_broken() {
        // Finding #20: a target like `[[contacts/sarah-chen]]` omits the layer
        // prefix. It has a `/` but its first segment (`contacts`) is not a
        // recognized layer, so it's a short-form error in `validate`, NOT a
        // broken link. stats must agree: it counts neither as broken nor as an
        // outgoing edge. Pre-fix `is_full_path` (components().count() > 1)
        // accepted it and reported broken_link_count = 1.
        let (_d, store) = temp_store();
        write_rel(
            &store,
            "records/contacts/a.md",
            "---\ntype: contact\nsummary: a\n---\n\nSee [[contacts/sarah-chen]].\n",
        );
        let s = compute(&store).expect("compute");
        assert_eq!(
            s.broken_link_count, 0,
            "a non-layer multi-segment target is a short-form error, not broken"
        );
        // The non-layer link is not a graph edge, so `a` has no outgoing edge
        // and is an orphan — matching how validate/graph treat it.
        assert_eq!(
            s.orphan_count, 1,
            "the non-layer link does not wire `a` out of orphan status"
        );
    }

    #[test]
    fn regression_wiki_links_in_code_fences_are_ignored() {
        // Finding #21: a wiki-link that appears only inside a fenced code block
        // is illustrative syntax, not a graph edge. validate skips fenced
        // regions; stats must too. Pre-fix the regex ran over the whole file
        // with no fence tracking, so the fenced ghost link inflated
        // broken_link_count to 1 and the fenced real link un-orphaned the page.
        let (_d, store) = temp_store();
        // A howto page whose ONLY wiki-links live inside ``` and ~~~ fences:
        // one to a missing target, one to an existing target.
        write_rel(
            &store,
            "records/synthesis/howto.md",
            "---\ntype: synthesis\nsummary: howto\n---\n\
             \nWrite links like this:\n\
             \n```\n[[records/contacts/ghost]]\n```\n\
             \nor this:\n\
             \n~~~\n[[records/synthesis/real]]\n~~~\n",
        );
        write_rel(
            &store,
            "records/synthesis/real.md",
            &doc("synthesis", "real"),
        );
        let s = compute(&store).expect("compute");
        assert_eq!(
            s.broken_link_count, 0,
            "a `[[...]]` inside a code fence is not a real (broken) edge"
        );
        // howto has no real edges => orphan. real is not linked-to by any real
        // edge => orphan. Both orphaned (2), proving the fenced link to `real`
        // did not wire either file out of orphan status.
        assert_eq!(
            s.orphan_count, 2,
            "fenced wiki-links do not wire files out of orphan status: {s:?}"
        );
    }

    #[test]
    fn a_link_to_an_existing_file_in_another_layer_resolves() {
        let (_d, store) = temp_store();
        // A records-layer profile links to a source file in the other layer;
        // cross-layer full-path links resolve like any other.
        write_rel(
            &store,
            "records/profiles/a.md",
            "---\ntype: profile\nsummary: a\n---\n\nfrom [[sources/emails/2026/05/m]]\n",
        );
        write_rel(&store, "sources/emails/2026/05/m.md", &doc("email", "m"));

        let s = compute(&store).expect("compute");
        assert_eq!(s.broken_link_count, 0);
        assert_eq!(s.orphan_count, 0, "both endpoints are wired");
    }

    #[test]
    fn regression_tilde_line_inside_backtick_fence_does_not_invert_state() {
        // Finding #44/#11: a `~~~` line inside an open ``` fence (or any inner
        // fence of the other char / a shorter run) must NOT close the block.
        // Pre-fix a single boolean toggled on it, inverting fence state so the
        // fenced ghost link counted broken and the real link after the fence
        // was dropped. With (byte, run-length) tracking the block only closes on
        // a matching ``` fence.
        let (_d, store) = temp_store();
        write_rel(&store, "wiki/people/bob.md", &doc("wiki-page", "bob"));
        // ```text … ~~~ x (inner tilde line) … [[ghost]] … ``` then a real link.
        write_rel(
            &store,
            "wiki/pages/howto.md",
            "---\ntype: wiki-page\nsummary: howto\n---\n\
             \n```text\n~~~ x\n[[wiki/people/ghost]]\n```\n\
             \nReal: [[wiki/people/bob]]\n",
        );

        let s = compute(&store).expect("compute");
        assert_eq!(
            s.broken_link_count, 0,
            "the fenced ghost link is inside the unbroken ``` block, not broken: {s:?}"
        );
        // bob is linked from howto (a real edge after the fence closes), and
        // howto links out — neither is an orphan.
        assert_eq!(
            s.orphan_count, 0,
            "the real post-fence link wires both files: {s:?}"
        );
    }

    #[test]
    fn regression_nested_log_directory_is_counted_not_skipped() {
        // Finding #45: only the layer's IMMEDIATE `log/` archive is skipped. A
        // directory named `log` nested under a type-folder is ordinary content
        // and must be counted, matching tree/index/query. Pre-fix any `log` dir
        // at any depth was pruned, making the whole subtree invisible to stats.
        let (_d, store) = temp_store();
        write_rel(
            &store,
            "sources/emails/log/maillog.md",
            &doc(
                "email",
                "an archived mail log entry under a log subdirectory",
            ),
        );
        // The layer-immediate `log/` archive is still skipped.
        write_rel(&store, "sources/log/2026-04.md", &doc("email", "rotated"));

        let s = compute(&store).expect("compute");
        assert_eq!(
            s.total_files, 1,
            "the nested sources/emails/log file counts; the layer-immediate sources/log is skipped: {s:?}"
        );
        assert_eq!(s.files_per_layer.get(&Layer::Sources), Some(&1));
        assert_eq!(s.type_distribution.get("email"), Some(&1));
    }

    #[test]
    fn regression_link_to_existing_non_md_source_is_a_live_edge() {
        // Finding (high): a record that wiki-links to an existing non-`.md`
        // source artifact (a `.eml`) must read as a LIVE edge, not broken, and
        // the record is not an orphan. `sources/` holds such files by design.
        let (_d, store) = temp_store();
        // A real .eml source file (not a .md content file).
        write_rel(
            &store,
            "sources/emails/msg.eml",
            "From: someone@example.com\nSubject: Renewal\n\nBody text.\n",
        );
        // A record with the SPEC-canonical bare link to that source.
        write_rel(
            &store,
            "records/contacts/sarah.md",
            "---\ntype: contact\nsummary: s\n---\n\nLinked source: [[sources/emails/msg]]\n",
        );

        let s = compute(&store).expect("compute");
        assert_eq!(
            s.broken_link_count, 0,
            "a link to an existing .eml source is live, not broken: {s:?}"
        );
        assert_eq!(
            s.orphan_count, 0,
            "the linking record has a resolvable outgoing edge to the source: {s:?}"
        );
        // The explicit-extension form resolves the same way.
        write_rel(
            &store,
            "records/contacts/sarah.md",
            "---\ntype: contact\nsummary: s\n---\n\nLinked source: [[sources/emails/msg.eml]]\n",
        );
        let s2 = compute(&store).expect("compute");
        assert_eq!(s2.broken_link_count, 0, "explicit .eml target resolves too");
        assert_eq!(s2.orphan_count, 0);
    }

    #[test]
    fn regression_traversal_target_is_broken_not_a_filesystem_escape() {
        // SECURITY regression: a `..`-laden wiki-link target must never turn a
        // stats probe into a read of a file OUTSIDE the store. Pre-fix
        // `target_resolves_on_disk` joined the raw target onto the store root and
        // probed `is_file` / `read_dir` with no containment check, so
        // `[[sources/../../outside-secret]]` reached a file above the store and
        // was silently counted as a LIVE edge (un-orphaning the linker and never
        // counted broken) — diverging from validate (which flags it
        // WIKI_LINK_BROKEN) and graph (which drops it). The gate now rejects any
        // non-`Normal` component before any join, so it counts broken.
        // Nested store: `store.root.parent()` is this test's private tempdir,
        // never the shared `$TMPDIR` (which the sibling traversal test would also
        // write into, racing on the same filename under `--workspace`).
        let (_d, store) = temp_store_nested();
        // Every store has a `sources/` dir; the traversal needs its first
        // component to be a recognized layer to pass `is_full_path`.
        fs::create_dir_all(store.root.join("sources/emails")).unwrap();
        // Plant a secret ABOVE the store root (the parent of the store dir).
        let outside_dir = store.root.parent().expect("store has a parent");
        fs::write(outside_dir.join("outside-secret.txt"), "TOP SECRET\n").unwrap();

        // Bare-stem traversal (would hit the `read_dir` parent branch) and the
        // explicit-extension traversal (would hit the `is_file` literal branch).
        for target in [
            "sources/../../outside-secret",
            "sources/../../outside-secret.txt",
        ] {
            write_rel(
                &store,
                "records/contacts/a.md",
                &format!("---\ntype: contact\nsummary: s\n---\n\nEscape: [[{target}]]\n"),
            );
            let s = compute(&store).expect("compute");
            assert_eq!(
                s.broken_link_count, 1,
                "a `..` target escaping the store must be broken, not a live edge ({target}): {s:?}"
            );
            assert_eq!(
                s.orphan_count, 1,
                "an escaping link must NOT wire the linker out of orphan status ({target}): {s:?}"
            );
        }
        // The secret outside the store is untouched (we never followed the link).
        assert_eq!(
            fs::read_to_string(outside_dir.join("outside-secret.txt")).unwrap(),
            "TOP SECRET\n"
        );
    }

    #[test]
    fn regression_target_resolves_on_disk_rejects_traversal_before_any_probe() {
        // SECURITY regression at the helper level: `target_resolves_on_disk`
        // must return `false` for any `..`-laden / absolute / prefix target
        // BEFORE it joins, `is_file`s, or `read_dir`s — so a wiki-link can never
        // turn a stats existence-probe into a read of a file OUTSIDE the store.
        // Pre-fix the helper joined the raw target onto the store root with no
        // containment gate, so a real file above the store made it return
        // `true`. This asserts the gate directly on the helper (the end-to-end
        // `compute()` path is covered separately above), exercising BOTH on-disk
        // branches: the literal `is_file` branch (explicit extension) and the
        // bare-stem `read_dir` branch.
        // Nested store: `store.root.parent()` is this test's private tempdir, so
        // the "above the store" files below never land in the shared `$TMPDIR`
        // and can never collide with the sibling traversal test's identically
        // named planted files when both run in parallel.
        let (_d, store) = temp_store_nested();
        // A real `sources/` tree exists (the literal/parent joins would have
        // something to land near), matching a real store.
        fs::create_dir_all(store.root.join("sources/emails")).unwrap();
        // Plant matching files ABOVE the store root: one with the exact name the
        // explicit-extension target points at, and one whose stem the bare-stem
        // target would discover via `read_dir` of the (escaped) parent dir.
        let outside_dir = store.root.parent().expect("store has a parent");
        fs::write(outside_dir.join("outside-secret.txt"), "TOP SECRET\n").unwrap();
        fs::write(outside_dir.join("outside-secret.eml"), "secret mail\n").unwrap();

        // Explicit-extension traversal -> would hit the literal `is_file` branch.
        assert!(
            !target_resolves_on_disk(
                &store.root,
                &strip_md(Path::new("sources/../../outside-secret.txt"))
            ),
            "an explicit-extension `..` target escaping the store must not resolve on disk"
        );
        // Bare-stem traversal -> would hit the `read_dir(parent)` branch, where a
        // sibling `outside-secret.eml` (non-`.md`) sits beside the escaped parent.
        assert!(
            !target_resolves_on_disk(
                &store.root,
                &strip_md(Path::new("sources/../../outside-secret"))
            ),
            "a bare-stem `..` target escaping the store must not resolve on disk"
        );
        // A `..` that stays nominally under a layer prefix is still an escape and
        // is rejected before any probe.
        assert!(
            !target_resolves_on_disk(&store.root, Path::new("records/../wiki/secret")),
            "any `..` component is rejected before a probe, even one re-entering a layer"
        );

        // Sanity: a legitimate in-store non-`.md` source DOES still resolve, so
        // the gate did not over-reject and break the finding #117 behavior.
        write_rel(
            &store,
            "sources/emails/msg.eml",
            "From: a@b.com\nSubject: x\n\nbody\n",
        );
        assert!(
            target_resolves_on_disk(&store.root, Path::new("sources/emails/msg")),
            "a legitimate in-store bare-stem source link still resolves on disk"
        );

        // The secrets outside the store are untouched (we never followed a link).
        assert_eq!(
            fs::read_to_string(outside_dir.join("outside-secret.txt")).unwrap(),
            "TOP SECRET\n"
        );
    }

    #[test]
    fn regression_link_to_truly_missing_source_is_still_broken() {
        // Guard the source-resolution fix doesn't over-resolve: a bare link
        // whose target has NO file of any extension on disk is still broken.
        let (_d, store) = temp_store();
        write_rel(
            &store,
            "records/contacts/sarah.md",
            "---\ntype: contact\nsummary: s\n---\n\nLinked: [[sources/emails/missing]]\n",
        );
        let s = compute(&store).expect("compute");
        assert_eq!(
            s.broken_link_count, 1,
            "a target with no on-disk file in any form is broken: {s:?}"
        );
    }
}
