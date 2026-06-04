//! `render` — data structures for the structural views, **no output
//! formatting**.
//!
//! [`Tree`] groups the store by layer → type → file; [`Outline`] groups one
//! file by its `##` sections. Both are pure data; `dbmd-cli` formats them to
//! text or JSON. Keeping formatting out of the library lets every db.md-aware
//! tool render these structures its own way.

use std::path::{Path, PathBuf};

use crate::parser::Section;
use crate::store::{Layer, Store, StoreError};

/// The store as a tree, grouped layer → type-folder → file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    /// One branch per non-empty layer.
    pub layers: Vec<TreeLayer>,
}

/// A layer branch of a [`Tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeLayer {
    /// Which layer this branch is.
    pub layer: Layer,
    /// One branch per non-empty type-folder under the layer.
    pub type_folders: Vec<TreeTypeFolder>,
}

/// A type-folder branch of a [`Tree`], aggregated across date-shards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeTypeFolder {
    /// The type-folder's store-relative path (e.g. `records/contacts`).
    pub path: PathBuf,
    /// The store-relative file paths under it (across shards).
    pub files: Vec<PathBuf>,
}

/// One file's section hierarchy: the file path plus its `##` sections and their
/// sub-sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    /// The store-relative path of the outlined file.
    pub file: PathBuf,
    /// The file's sections, in document order (depth carried on each
    /// [`Section`]).
    pub sections: Vec<Section>,
}

/// **SWEEP.** Build a [`Tree`] of the whole store (layer → type-folder → file),
/// optionally scoped to one layer and/or one type. Off the interactive loop.
///
/// The grouping mirrors the db.md content model: a *type-folder* is an immediate
/// child directory of a layer (`records/contacts`, `sources/emails`); its files
/// are every `.md` content file beneath it, **aggregated across date-shards**
/// (`sources/emails/2026/05/*.md`). Meta files never appear: the per-folder
/// `index.md`, the root `DB.md`, and `log.md` / the `log/` archive dir are all
/// skipped, as are hidden dot-dirs. A loose `.md` file sitting directly under a
/// layer (with no enclosing type-folder) has no slot in the layer → type-folder
/// → file model and is therefore not listed.
///
/// Ordering is total and deterministic so two runs — and a human vs. a machine
/// reader — never disagree: layers in canonical [`Layer::all`] order, then
/// type-folders by store-relative path ascending, then files by store-relative
/// path ascending. Empty layers and empty type-folders are omitted.
pub fn tree(store: &Store, layer: Option<Layer>, type_: Option<&str>) -> Result<Tree, StoreError> {
    let mut layers = Vec::new();

    for l in Layer::all() {
        if let Some(want) = layer {
            if l != want {
                continue;
            }
        }

        let layer_abs = store.root.join(layer_dir_name(l));
        if !layer_abs.is_dir() {
            continue;
        }

        // Each immediate sub-directory of the layer is a type-folder. Sort the
        // type-folder names for a stable branch order.
        let mut type_dir_names: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&layer_abs)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_skipped_dir(&name) {
                continue;
            }
            type_dir_names.push(name);
        }
        type_dir_names.sort();

        let mut type_folders = Vec::new();
        for type_name in type_dir_names {
            if let Some(want) = type_ {
                if type_name != want {
                    continue;
                }
            }

            let type_abs = layer_abs.join(&type_name);
            let mut files: Vec<PathBuf> = Vec::new();
            collect_content_files(&store.root, &type_abs, &mut files)?;
            if files.is_empty() {
                continue;
            }
            files.sort();

            type_folders.push(TreeTypeFolder {
                path: PathBuf::from(layer_dir_name(l)).join(&type_name),
                files,
            });
        }

        if type_folders.is_empty() {
            continue;
        }

        layers.push(TreeLayer {
            layer: l,
            type_folders,
        });
    }

    Ok(Tree { layers })
}

/// The on-disk folder name for a layer. A render-local copy of the canonical
/// layer→dir mapping so the walk never depends on store-side helpers; the names
/// are fixed by the db.md spec (`sources` / `records` / `wiki`).
fn layer_dir_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Sources => "sources",
        Layer::Records => "records",
        Layer::Wiki => "wiki",
    }
}

/// Directory names skipped during the store walk: hidden dot-dirs and the
/// rotated-log archive folder.
fn is_skipped_dir(name: &str) -> bool {
    name == "log" || name.starts_with('.')
}

/// True if a file name is a content file we list in the tree: a `.md` file that
/// is not a per-folder `index.md` meta file. `index.jsonl`, `.DS_Store`, and
/// any non-`.md` artifact are not content.
fn is_content_md(name: &str) -> bool {
    name.ends_with(".md") && name != "index.md"
}

/// Recursively collect content `.md` files beneath a type-folder, descending
/// through date-shard subdirectories, into `out` as store-relative paths.
/// Skips hidden dirs and any nested `index.md` meta files.
fn collect_content_files(
    store_root: &Path,
    dir: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), StoreError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();

        if file_type.is_dir() {
            if name.starts_with('.') {
                continue;
            }
            collect_content_files(store_root, &entry.path(), out)?;
        } else if file_type.is_file() && is_content_md(&name) {
            let abs = entry.path();
            let rel = abs.strip_prefix(store_root).unwrap_or(&abs).to_path_buf();
            out.push(rel);
        }
    }
    Ok(())
}

/// Build the [`Outline`] of a single file from its `##` (and deeper) sections.
/// Loop-fast (one file).
///
/// `file` may be given store-relative or absolute; the read resolves against
/// [`Store::root`] when relative, and [`Outline::file`] is always normalized to
/// the store-relative form. Sections are extracted over the file **body** (after
/// the YAML frontmatter), so [`Section::line`] is 1-based within the body — the
/// same frame [`crate::parser::extract_sections`] uses. Only `##` and deeper
/// headings are sections (a single leading `#` title is not a section); headings
/// inside fenced code blocks are not mistaken for real headings.
pub fn outline(store: &Store, file: &Path) -> Result<Outline, StoreError> {
    let abs = if file.is_absolute() {
        file.to_path_buf()
    } else {
        store.root.join(file)
    };

    let rel = abs.strip_prefix(&store.root).unwrap_or(file).to_path_buf();

    let text = std::fs::read_to_string(&abs)?;
    let body = strip_frontmatter(&text);
    let sections = parse_sections(body);

    Ok(Outline {
        file: rel,
        sections,
    })
}

/// Return the file body with a leading YAML frontmatter block removed, so
/// section line numbers count from the first body line (matching the parser's
/// body frame). If the text does not open with a `---` fence, it is all body.
/// Lenient by design: an outline never fails just because a file is missing
/// frontmatter.
fn strip_frontmatter(text: &str) -> &str {
    // The opening fence must be the very first line, exactly `---`.
    let after_open = match text.strip_prefix("---\n") {
        Some(rest) => rest,
        None => match text.strip_prefix("---\r\n") {
            Some(rest) => rest,
            None => return text,
        },
    };

    // Find the closing `---` line; the body is everything after it.
    let mut search_from = 0usize;
    while let Some(rel_idx) = after_open[search_from..].find("---") {
        let idx = search_from + rel_idx;
        let at_line_start = idx == 0 || after_open.as_bytes()[idx - 1] == b'\n';
        let after = &after_open[idx + 3..];
        let line_ends = after.is_empty()
            || after.starts_with('\n')
            || after.starts_with("\r\n")
            || after.starts_with('\r');
        if at_line_start && line_ends {
            // Skip past the closing fence's own line terminator.
            if let Some(stripped) = after.strip_prefix("\r\n") {
                return stripped;
            }
            if let Some(stripped) = after.strip_prefix('\n') {
                return stripped;
            }
            if let Some(stripped) = after.strip_prefix('\r') {
                return stripped;
            }
            return after; // closing fence is the last line, no trailing body
        }
        search_from = idx + 3;
    }

    // Unterminated frontmatter: treat the whole thing as body rather than error.
    text
}

/// Parse the `##`-and-deeper sections of a markdown body into a flat list in
/// document order, with each section's body spanning from its heading line to
/// the next sibling-or-shallower heading (exclusive). Headings inside fenced
/// code blocks (``` / ~~~) are ignored.
fn parse_sections(body: &str) -> Vec<Section> {
    // Split into lines, remembering each line's start byte so we can slice the
    // original body verbatim (preserving its exact newlines).
    let lines: Vec<&str> = body.split_inclusive('\n').collect();

    // First pass: classify every line's heading level (0 = not a heading),
    // honoring fenced-code-block state so fenced `## x` is not a heading.
    let mut levels: Vec<u8> = Vec::with_capacity(lines.len());
    let mut fence: Option<(u8, usize)> = None; // (fence byte, run length)
    for line in &lines {
        let content = line.trim_end_matches(['\n', '\r']);
        if let Some(f) = fence {
            if is_closing_fence(content, f) {
                fence = None;
            }
            levels.push(0);
            continue;
        }
        if let Some(opened) = opening_fence(content) {
            fence = Some(opened);
            levels.push(0);
            continue;
        }
        levels.push(heading_level(content));
    }

    // Second pass: for each `##`+ heading, find the next heading at an
    // equal-or-shallower level; the section body is the inclusive line range
    // [heading, that next heading).
    let mut sections = Vec::new();
    for (i, &lvl) in levels.iter().enumerate() {
        if lvl < 2 {
            continue;
        }
        let heading_line = lines[i].trim_end_matches(['\n', '\r']);
        let heading = heading_text(heading_line, lvl);

        let mut end = lines.len();
        for (j, &other) in levels.iter().enumerate().skip(i + 1) {
            if other != 0 && other <= lvl {
                end = j;
                break;
            }
        }

        let body_slice: String = lines[i..end].concat();

        sections.push(Section {
            heading,
            level: lvl,
            line: (i + 1) as u32,
            body: body_slice,
        });
    }

    sections
}

/// The ATX heading level of a line (number of leading `#`), or 0 if the line is
/// not a heading. Allows up to three leading spaces (CommonMark), requires a
/// space (or end-of-line) after the `#` run, and caps the run at six.
fn heading_level(line: &str) -> u8 {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return 0;
    }
    let rest = &line[indent..];
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return 0;
    }
    let after = &rest[hashes..];
    if after.is_empty() || after.starts_with(' ') || after.starts_with('\t') {
        hashes as u8
    } else {
        0
    }
}

/// The heading text of a heading line: the content after the `#` run, trimmed,
/// with any trailing closing `#` sequence removed (ATX closing fence).
fn heading_text(line: &str, level: u8) -> String {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let after_hashes = &line[indent + level as usize..];
    let trimmed = after_hashes.trim();
    // Strip an optional trailing run of `#` (ATX closing sequence), e.g.
    // `## Title ##`.
    let no_trailing = trimmed.trim_end_matches('#');
    if no_trailing.len() == trimmed.len() {
        trimmed.to_string()
    } else {
        no_trailing.trim_end().to_string()
    }
}

/// If `line` opens a fenced code block, return its `(fence byte, run length)`.
/// A fence is at least three backticks or tildes, with up to three leading
/// spaces of indentation.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Config;
    use std::fs;
    use tempfile::TempDir;

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// A real temp store on disk plus an opened [`Store`] pointed at it.
    ///
    /// We construct the `Store` from its public fields rather than `Store::open`
    /// so these tests exercise *render* against real files without depending on
    /// store-side parsing.
    struct Fixture {
        _dir: TempDir,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            // A real store is marked by a DB.md at the root.
            fs::write(dir.path().join("DB.md"), "---\ntype: db\n---\n").expect("write DB.md");
            let store = Store {
                root: dir.path().to_path_buf(),
                config: Config::default(),
            };
            Fixture { _dir: dir, store }
        }

        /// Write `contents` to a store-relative path, creating parent dirs.
        fn write(&self, rel: &str, contents: &str) {
            let abs = self.store.root.join(rel);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent).expect("create parents");
            }
            fs::write(abs, contents).expect("write file");
        }

        fn mkdir(&self, rel: &str) {
            fs::create_dir_all(self.store.root.join(rel)).expect("mkdir");
        }
    }

    /// A minimal valid content file body (frontmatter + a heading).
    fn doc(summary: &str) -> String {
        format!("---\ntype: contact\nsummary: {summary}\n---\n\nbody\n")
    }

    /// Collect a tree's `(type-folder path, [file paths])` as strings, in the
    /// order the tree presents them — the structure under test.
    fn shape(tree: &Tree) -> Vec<(Layer, String, Vec<String>)> {
        let mut out = Vec::new();
        for layer in &tree.layers {
            for tf in &layer.type_folders {
                let files = tf
                    .files
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                out.push((layer.layer, tf.path.to_string_lossy().into_owned(), files));
            }
        }
        out
    }

    // ── tree() ──────────────────────────────────────────────────────────────

    #[test]
    fn tree_groups_by_layer_then_type_folder_in_canonical_order() {
        let fx = Fixture::new();
        // Deliberately seed wiki before records before sources on disk by name
        // so a naive readdir order would be alphabetical (records, sources,
        // wiki) — the tree must instead emit the canonical Sources→Records→Wiki.
        fx.write("wiki/people/sarah.md", &doc("sarah bio"));
        fx.write("records/contacts/sarah-chen.md", &doc("sarah contact"));
        fx.write("sources/emails/a.md", &doc("an email"));

        let tree = tree(&fx.store, None, None).expect("tree");
        let layer_order: Vec<Layer> = tree.layers.iter().map(|l| l.layer).collect();
        assert_eq!(
            layer_order,
            vec![Layer::Sources, Layer::Records, Layer::Wiki],
            "layers must come back in canonical order regardless of on-disk name order"
        );

        assert_eq!(
            shape(&tree),
            vec![
                (
                    Layer::Sources,
                    "sources/emails".to_string(),
                    vec!["sources/emails/a.md".to_string()]
                ),
                (
                    Layer::Records,
                    "records/contacts".to_string(),
                    vec!["records/contacts/sarah-chen.md".to_string()]
                ),
                (
                    Layer::Wiki,
                    "wiki/people".to_string(),
                    vec!["wiki/people/sarah.md".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn tree_type_folders_and_files_are_sorted_ascending() {
        let fx = Fixture::new();
        // Two type-folders, out of alphabetical order on creation.
        fx.write("records/expenses/z.md", &doc("z"));
        fx.write("records/contacts/b.md", &doc("b"));
        fx.write("records/contacts/a.md", &doc("a"));

        let tree = tree(&fx.store, None, None).expect("tree");
        let records = tree
            .layers
            .iter()
            .find(|l| l.layer == Layer::Records)
            .expect("records layer");

        let folder_paths: Vec<String> = records
            .type_folders
            .iter()
            .map(|tf| tf.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            folder_paths,
            vec![
                "records/contacts".to_string(),
                "records/expenses".to_string()
            ],
            "type-folders sorted by path ascending"
        );

        let contacts = &records.type_folders[0];
        let files: Vec<String> = contacts
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            files,
            vec![
                "records/contacts/a.md".to_string(),
                "records/contacts/b.md".to_string()
            ],
            "files sorted by store-relative path ascending"
        );
    }

    #[test]
    fn tree_aggregates_files_across_date_shards_into_one_type_folder() {
        let fx = Fixture::new();
        fx.write("sources/emails/2026/05/newer.md", &doc("newer"));
        fx.write("sources/emails/2026/04/older.md", &doc("older"));
        fx.write("sources/emails/loose.md", &doc("loose at folder root"));

        let tree = tree(&fx.store, None, None).expect("tree");
        let emails: Vec<&TreeTypeFolder> = tree
            .layers
            .iter()
            .flat_map(|l| &l.type_folders)
            .filter(|tf| tf.path == Path::new("sources/emails"))
            .collect();

        assert_eq!(
            emails.len(),
            1,
            "all shards of one type fold into a single type-folder branch, not one per shard"
        );
        let files: Vec<String> = emails[0]
            .files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            files,
            vec![
                "sources/emails/2026/04/older.md".to_string(),
                "sources/emails/2026/05/newer.md".to_string(),
                "sources/emails/loose.md".to_string(),
            ],
            "every file under the type-folder, across shards, appears once"
        );
    }

    #[test]
    fn tree_excludes_index_and_log_and_db_meta_files() {
        let fx = Fixture::new();
        // Real content.
        fx.write("records/contacts/sarah.md", &doc("sarah"));
        // Meta files at every level that must NOT show up as content.
        fx.write("index.md", "---\ntype: index\n---\n"); // root index
        fx.write("records/index.md", "---\ntype: index\n---\n"); // layer index
        fx.write("records/contacts/index.md", "---\ntype: index\n---\n"); // type-folder index
        fx.write("records/contacts/index.jsonl", "{}\n"); // machine twin
        fx.write("log.md", "log\n"); // active log
        fx.write("log/2026-04.md", "rotated\n"); // rotated log archive

        let tree = tree(&fx.store, None, None).expect("tree");
        let all_files: Vec<String> = tree
            .layers
            .iter()
            .flat_map(|l| &l.type_folders)
            .flat_map(|tf| &tf.files)
            .map(|p| p.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            all_files,
            vec!["records/contacts/sarah.md".to_string()],
            "only the real content file survives; no index.md/index.jsonl/log files"
        );
        // The `log/` dir at the root is not a layer, so it never produces a branch.
        assert!(tree
            .layers
            .iter()
            .all(|l| matches!(l.layer, Layer::Sources | Layer::Records | Layer::Wiki)));
    }

    #[test]
    fn tree_omits_empty_layers_and_empty_type_folders() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &doc("a"));
        // An empty type-folder (dir exists, no content files).
        fx.mkdir("records/companies");
        // An empty layer (dir exists, nothing under it).
        fx.mkdir("wiki");
        // A type-folder holding only a meta file is effectively empty content.
        fx.write("sources/emails/index.md", "---\ntype: index\n---\n");

        let tree = tree(&fx.store, None, None).expect("tree");

        let layers: Vec<Layer> = tree.layers.iter().map(|l| l.layer).collect();
        assert_eq!(
            layers,
            vec![Layer::Records],
            "empty wiki layer and meta-only sources layer are omitted"
        );
        let folders: Vec<String> = tree.layers[0]
            .type_folders
            .iter()
            .map(|tf| tf.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            folders,
            vec!["records/contacts".to_string()],
            "the empty companies type-folder is omitted"
        );
    }

    #[test]
    fn tree_layer_filter_restricts_to_one_layer() {
        let fx = Fixture::new();
        fx.write("sources/emails/a.md", &doc("a"));
        fx.write("records/contacts/b.md", &doc("b"));
        fx.write("wiki/people/c.md", &doc("c"));

        let tree = tree(&fx.store, Some(Layer::Records), None).expect("tree");
        let layers: Vec<Layer> = tree.layers.iter().map(|l| l.layer).collect();
        assert_eq!(
            layers,
            vec![Layer::Records],
            "only the requested layer is walked"
        );
    }

    #[test]
    fn tree_type_filter_keeps_only_matching_folder_name_across_layers() {
        let fx = Fixture::new();
        // Same folder name `notes` under two layers; a sibling folder to exclude.
        fx.write("sources/notes/s.md", &doc("source note"));
        fx.write("wiki/notes/w.md", &doc("wiki note"));
        fx.write("records/contacts/c.md", &doc("contact"));

        let tree = tree(&fx.store, None, Some("notes")).expect("tree");
        let folders: Vec<String> = tree
            .layers
            .iter()
            .flat_map(|l| &l.type_folders)
            .map(|tf| tf.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            folders,
            vec!["sources/notes".to_string(), "wiki/notes".to_string()],
            "type filter matches the folder name in every layer, excludes other folders"
        );
    }

    #[test]
    fn tree_excludes_loose_files_directly_under_a_layer() {
        let fx = Fixture::new();
        fx.write("records/contacts/real.md", &doc("real"));
        // A loose .md directly under the layer, not in any type-folder.
        fx.write("records/stray.md", &doc("stray"));

        let tree = tree(&fx.store, None, None).expect("tree");
        let all_files: Vec<String> = tree
            .layers
            .iter()
            .flat_map(|l| &l.type_folders)
            .flat_map(|tf| &tf.files)
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            all_files,
            vec!["records/contacts/real.md".to_string()],
            "a layer-direct file has no type-folder slot and is not listed"
        );
    }

    #[test]
    fn tree_skips_hidden_directories() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &doc("a"));
        // A hidden type-folder and a hidden shard inside a real one.
        fx.write(".git/objects/x.md", &doc("vcs junk"));
        fx.write("records/.hidden/h.md", &doc("hidden type folder"));
        fx.write("sources/emails/.tmp/draft.md", &doc("hidden shard"));

        let tree = tree(&fx.store, None, None).expect("tree");
        let all_files: Vec<String> = tree
            .layers
            .iter()
            .flat_map(|l| &l.type_folders)
            .flat_map(|tf| &tf.files)
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            all_files,
            vec!["records/contacts/a.md".to_string()],
            "hidden dirs are skipped at the type-folder and shard levels"
        );
    }

    #[test]
    fn tree_paths_are_store_relative_not_absolute() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &doc("a"));

        let tree = tree(&fx.store, None, None).expect("tree");
        let tf = &tree.layers[0].type_folders[0];
        assert!(
            tf.path.is_relative() && tf.files[0].is_relative(),
            "tree paths must be store-relative"
        );
        // And they must not leak the absolute root prefix.
        let root_str = fx.store.root.to_string_lossy().into_owned();
        assert!(!tf.files[0].to_string_lossy().contains(&root_str));
    }

    #[test]
    fn tree_on_store_with_no_layers_is_empty() {
        let fx = Fixture::new(); // only DB.md, no layer dirs
        let tree = tree(&fx.store, None, None).expect("tree");
        assert!(
            tree.layers.is_empty(),
            "a store with no content has an empty tree"
        );
    }

    // ── outline() ─────────────────────────────────────────────────────────────

    /// Heading text + level + 1-based body line, for compact assertions.
    fn headings(o: &Outline) -> Vec<(String, u8, u32)> {
        o.sections
            .iter()
            .map(|s| (s.heading.clone(), s.level, s.line))
            .collect()
    }

    #[test]
    fn outline_extracts_sections_with_levels_and_body_relative_lines() {
        let fx = Fixture::new();
        // 4-line frontmatter block; the body starts at the blank line after it.
        // Body line 1: ""   2: "# Title"  3: ""  4: "## Alpha"  5: "text"
        //      6: "### Sub"  7: "more"  8: "## Beta"  9: "end"
        let file = "---\ntype: note\nsummary: s\n---\n\n# Title\n\n## Alpha\ntext\n### Sub\nmore\n## Beta\nend\n";
        fx.write("wiki/notes/n.md", file);

        let o = outline(&fx.store, Path::new("wiki/notes/n.md")).expect("outline");
        assert_eq!(
            headings(&o),
            vec![
                ("Alpha".to_string(), 2, 4),
                ("Sub".to_string(), 3, 6),
                ("Beta".to_string(), 2, 8),
            ],
            "only ##+ headings, with body-relative 1-based line numbers; the # title is not a section"
        );
        assert_eq!(o.file, PathBuf::from("wiki/notes/n.md"));
    }

    #[test]
    fn outline_section_body_spans_to_next_sibling_or_shallower_heading() {
        let fx = Fixture::new();
        let file = "---\nx: 1\n---\n## Alpha\na1\na2\n### Sub\ns1\n## Beta\nb1\n";
        fx.write("wiki/notes/n.md", file);

        let o = outline(&fx.store, Path::new("wiki/notes/n.md")).expect("outline");
        let alpha = &o.sections[0];
        // Alpha (##) absorbs its own lines AND the nested ### Sub, stopping at ## Beta.
        assert_eq!(alpha.heading, "Alpha");
        assert_eq!(
            alpha.body, "## Alpha\na1\na2\n### Sub\ns1\n",
            "a ## body runs through deeper headings up to the next sibling-or-shallower heading"
        );

        let sub = &o.sections[1];
        assert_eq!(sub.heading, "Sub");
        assert_eq!(
            sub.body, "### Sub\ns1\n",
            "the nested ### body stops at the next ## (shallower) heading"
        );

        let beta = &o.sections[2];
        assert_eq!(
            beta.body, "## Beta\nb1\n",
            "the trailing ## body runs to end of file"
        );
    }

    #[test]
    fn outline_shallower_heading_terminates_a_section_body() {
        let fx = Fixture::new();
        // A later level-1 `#` is shallower than `##` and must close the ## body.
        let file = "---\nx: 1\n---\n## Sec\nbody1\n# NewTitle\nafter\n";
        fx.write("wiki/notes/n.md", file);

        let o = outline(&fx.store, Path::new("wiki/notes/n.md")).expect("outline");
        assert_eq!(headings(&o), vec![("Sec".to_string(), 2, 1)]);
        assert_eq!(
            o.sections[0].body, "## Sec\nbody1\n",
            "the level-1 heading is shallower and ends the section, and is itself not a section"
        );
    }

    #[test]
    fn outline_ignores_headings_inside_fenced_code_blocks() {
        let fx = Fixture::new();
        let file = "---\nx: 1\n---\n## Real\n```\n## fake heading in code\n### also fake\n```\nafter\n## AlsoReal\n";
        fx.write("wiki/notes/n.md", file);

        let o = outline(&fx.store, Path::new("wiki/notes/n.md")).expect("outline");
        // Body lines: 1 `## Real`, 2 ```, 3/4 fenced fakes, 5 ```, 6 `after`,
        // 7 `## AlsoReal` — so AlsoReal is heading on body line 7.
        assert_eq!(
            headings(&o),
            vec![("Real".to_string(), 2, 1), ("AlsoReal".to_string(), 2, 7)],
            "## inside a ``` fence is code, not a heading"
        );
        // The fenced lines belong to Real's body, not their own sections.
        assert!(o.sections[0].body.contains("## fake heading in code"));
    }

    #[test]
    fn outline_ignores_tilde_fences_too() {
        let fx = Fixture::new();
        let file = "---\nx: 1\n---\n## Real\n~~~\n## fake\n~~~\ntail\n";
        fx.write("wiki/notes/n.md", file);

        let o = outline(&fx.store, Path::new("wiki/notes/n.md")).expect("outline");
        assert_eq!(headings(&o), vec![("Real".to_string(), 2, 1)]);
    }

    #[test]
    fn outline_rejects_non_heading_hash_lines() {
        let fx = Fixture::new();
        // `#tag` (no space) is not a heading; 7 hashes exceeds ATX max of 6.
        let file = "---\nx: 1\n---\n#nospace\n####### sevenhashes\n## Good\n";
        fx.write("wiki/notes/n.md", file);

        let o = outline(&fx.store, Path::new("wiki/notes/n.md")).expect("outline");
        assert_eq!(
            headings(&o),
            vec![("Good".to_string(), 2, 3)],
            "only the well-formed ## heading counts"
        );
    }

    #[test]
    fn outline_strips_atx_closing_hashes_from_heading_text() {
        let fx = Fixture::new();
        let file = "---\nx: 1\n---\n## Title ##\n";
        fx.write("wiki/notes/n.md", file);

        let o = outline(&fx.store, Path::new("wiki/notes/n.md")).expect("outline");
        assert_eq!(o.sections[0].heading, "Title");
    }

    #[test]
    fn outline_handles_file_without_frontmatter_numbering_from_line_one() {
        let fx = Fixture::new();
        // No `---` block at all; the whole file is body, so ## is on line 1.
        let file = "## First\ntext\n## Second\n";
        fx.write("wiki/notes/n.md", file);

        let o = outline(&fx.store, Path::new("wiki/notes/n.md")).expect("outline");
        assert_eq!(
            headings(&o),
            vec![("First".to_string(), 2, 1), ("Second".to_string(), 2, 3)],
            "with no frontmatter the body is the whole file and lines count from 1"
        );
    }

    #[test]
    fn outline_accepts_absolute_path_and_returns_store_relative_file() {
        let fx = Fixture::new();
        fx.write("records/contacts/x.md", "---\nx: 1\n---\n## H\n");
        let abs = fx.store.root.join("records/contacts/x.md");

        let o = outline(&fx.store, &abs).expect("outline");
        assert_eq!(
            o.file,
            PathBuf::from("records/contacts/x.md"),
            "an absolute input path is normalized to store-relative in the Outline"
        );
        assert_eq!(o.sections.len(), 1);
    }

    #[test]
    fn outline_of_a_file_with_no_headings_is_empty() {
        let fx = Fixture::new();
        fx.write(
            "wiki/notes/n.md",
            "---\nx: 1\n---\njust prose, no headings\n",
        );

        let o = outline(&fx.store, Path::new("wiki/notes/n.md")).expect("outline");
        assert!(
            o.sections.is_empty(),
            "a heading-free body yields no sections"
        );
    }

    #[test]
    fn outline_missing_file_is_an_io_error() {
        let fx = Fixture::new();
        let err = outline(&fx.store, Path::new("wiki/notes/does-not-exist.md"))
            .expect_err("missing file should error");
        assert!(
            matches!(err, StoreError::Io(_)),
            "a missing file surfaces as a StoreError::Io, got {err:?}"
        );
    }

    #[test]
    fn outline_handles_crlf_frontmatter_and_indented_headings() {
        let fx = Fixture::new();
        // CRLF frontmatter terminator + a heading indented up to 3 spaces (still
        // a heading per CommonMark) and one indented 4 (a code indent — not).
        let file = "---\r\nx: 1\r\n---\r\n   ## Indented3\nbody\n    ## Indented4Code\n";
        fx.write("wiki/notes/n.md", file);

        let o = outline(&fx.store, Path::new("wiki/notes/n.md")).expect("outline");
        assert_eq!(
            headings(&o),
            vec![("Indented3".to_string(), 2, 1)],
            "<=3 leading spaces is a heading; 4 spaces is indented code, not a heading"
        );
    }
}
