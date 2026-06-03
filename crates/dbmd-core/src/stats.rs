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

        let has_outgoing = file
            .resolvable_targets()
            .any(|t| t != &file.node_id && existing_nodes.contains(t));
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
        Layer::Wiki => "wiki",
    }
}

/// Recursively collect the `.md` **content** files under one layer root,
/// skipping hidden entries (`.git`, dotfiles), the `log/` archive tree, and the
/// `index.md` catalog meta files. Returns absolute paths. A missing layer root
/// yields an empty list (a store need not have all three layers).
fn walk_layer_content_files(layer_root: &Path) -> crate::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !layer_root.is_dir() {
        return Ok(out);
    }
    let walker = walkdir::WalkDir::new(layer_root)
        .into_iter()
        .filter_entry(|e| {
            // Skip hidden dirs/files and any `log` directory wholesale.
            let name = e.file_name().to_string_lossy();
            if name.starts_with('.') {
                return false;
            }
            if e.file_type().is_dir() && name == "log" {
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
fn extract_link_targets(text: &str, re: &Regex) -> Vec<PathBuf> {
    re.captures_iter(text)
        .filter_map(|c| c.get(1))
        .map(|m| {
            let raw = m.as_str().trim();
            strip_md(Path::new(raw))
        })
        .collect()
}

/// Drop a trailing `.md` from a path, leaving everything else intact.
fn strip_md(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    match s.strip_suffix(".md") {
        Some(stem) => PathBuf::from(stem),
        None => path.to_path_buf(),
    }
}

/// True if a wiki-link target is a full store-relative path (contains a path
/// separator). Short-form targets like `sarah-chen` are false. Doctrine: only
/// full paths resolve to a node.
fn is_full_path(target: &Path) -> bool {
    target.components().count() > 1
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
        write_rel(&store, "wiki/people/p.md", &doc("wiki-page", "p"));

        let s = compute(&store).expect("compute");
        assert_eq!(s.total_files, 4);
        assert_eq!(s.files_per_layer.get(&Layer::Sources), Some(&2));
        assert_eq!(s.files_per_layer.get(&Layer::Records), Some(&1));
        assert_eq!(s.files_per_layer.get(&Layer::Wiki), Some(&1));
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
            "wiki/.obsidian/cache.md",
            &doc("wiki-page", "hidden"),
        );

        let s = compute(&store).expect("compute");
        assert_eq!(s.total_files, 1, "only the one real content file counts");
        assert_eq!(s.files_per_layer.get(&Layer::Records), Some(&1));
        assert_eq!(s.files_per_layer.get(&Layer::Sources), None);
        assert_eq!(s.files_per_layer.get(&Layer::Wiki), None);
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
            "wiki/themes/x.md",
            "---\nsummary: no type here\n---\n\nbody\n",
        );
        // A content file with no frontmatter at all.
        write_rel(&store, "wiki/themes/y.md", "just a body, no frontmatter\n");

        let s = compute(&store).expect("compute");
        assert_eq!(s.total_files, 2, "untyped files still count toward totals");
        assert_eq!(s.files_per_layer.get(&Layer::Wiki), Some(&2));
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
            "wiki/people/a.md",
            "---\ntype: wiki-page\nsummary: a\n---\n\n[[wiki/people/b]] and [[records/contacts/ghost]]\n",
        );
        write_rel(&store, "wiki/people/b.md", &doc("wiki-page", "b"));

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
            "wiki/people/a.md",
            "---\ntype: wiki-page\nsummary: a\n---\n\n[[records/contacts/ghost]] [[records/contacts/ghost]]\n",
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
            "wiki/people/a.md",
            "---\ntype: wiki-page\nsummary: a\n---\n\nSee [Acme](https://acme.io/path).\n",
        );
        let s = compute(&store).expect("compute");
        assert_eq!(s.broken_link_count, 0, "markdown links aren't graph edges");
        assert_eq!(s.orphan_count, 1, "the file has no wiki-links => orphan");
    }

    #[test]
    fn a_link_to_an_existing_file_in_another_layer_resolves() {
        let (_d, store) = temp_store();
        // wiki page links to a source file in a different layer; cross-layer
        // full-path links resolve like any other.
        write_rel(
            &store,
            "wiki/people/a.md",
            "---\ntype: wiki-page\nsummary: a\n---\n\nfrom [[sources/emails/2026/05/m]]\n",
        );
        write_rel(&store, "sources/emails/2026/05/m.md", &doc("email", "m"));

        let s = compute(&store).expect("compute");
        assert_eq!(s.broken_link_count, 0);
        assert_eq!(s.orphan_count, 0, "both endpoints are wired");
    }
}
