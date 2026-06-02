//! `graph` — the wiki-link **relationship layer**.
//!
//! Wiki-links are curated-relevance edges (the LLM wrote them), so the graph's
//! job is to **assemble the relevant context around a seed**, not to be
//! analyzed. **All ops are on-demand — there is no maintained graph** (a
//! persistent graph is the roadmap engine).
//!
//! [`backlinks`] / [`forwardlinks`] are loop ops (O(changed), never O(store)).
//! [`neighborhood`] is the high-value context-hydration op. [`orphans`] is a
//! SWEEP curation worklist.
//!
//! Whole-graph analytics (connected components, cycle detection, shortest
//! path, sinks/sources, DOT/JSON export) are deliberately **not** here — a
//! human studying the graph opens the store in Obsidian; broken-link detection
//! is [`crate::validate`]'s job (`WIKI_LINK_BROKEN`).
//!
//! ## Implementation note — two paths for the incoming-edge scan
//!
//! The scale contract (SPEC § Tooling, plan: *"the interactive loop is
//! O(changed), never O(store)"*) is the load-bearing rule here. [`backlinks`]
//! is a loop op, so it must **not** open and `read_to_string` every content file
//! in the store on each call. It resolves incoming edges by one of two paths,
//! chosen by whether the call is scoped:
//!
//! - **Unscoped** (`dbmd graph backlinks <x>`, no `--type`/`--in`): one
//!   embedded-ripgrep pass for the literal `[[<target>]]` over the tree, via
//!   [`Store::find_links_to`] (`grep` + `ignore`, early-exit per file) — the
//!   same scan engine [`crate::validate`]'s working-set incoming-linker step
//!   uses. A single store traversal with cheap presence-only matching, not N
//!   whole-file parses; that is what keeps the unscoped call inside the loop
//!   budget. [`backlinks`] then filters the raw hits to content files and emits
//!   canonical bare targets (its relationship view), where the lower-level
//!   [`Store::find_links_to`] returns every `.md` the text appears in.
//! - **Scoped** (`--type` / `--in`): the candidate set is enumerated from the
//!   relevant type-folder `index.jsonl` sidecars — one sequential read per
//!   type-folder (via [`crate::query::Query`], which sits on
//!   [`Store::read_type_index`]) — and each candidate is confirmed by a
//!   single-file parse. That is what makes `--type` / `--in` an *I/O* scope, not
//!   just a result filter: a typed/layer-scoped `backlinks` reads only the
//!   relevant folder(s)' sidecars and parses only those files.
//!
//! **Why the scoped path confirms by parsing the candidate, not by trusting the
//! sidecar's `links` field.** A sidecar record's `links` is the file's
//! *frontmatter* `links:` list only — it does **not** capture wiki-links written
//! in the body or inside other typed frontmatter fields (`company: [[…]]`,
//! `attendees: [ … ]`, `derived_from: [ … ]`). [`forwardlinks`] extracts edges
//! from the whole file, so to keep the two directions on the **same** edge set
//! (an incoming edge to X is exactly: some file whose [`forwardlinks`] contains
//! X) the incoming-edge confirmation re-parses each candidate file the same way.
//! The sidecar bounds *which* files are candidates; the parse decides whether
//! each truly links. The unscoped ripgrep path stays on that same edge set by
//! matching the link text wherever it lives in the file (frontmatter or body).
//! A node's `summary` / `type` likewise read frontmatter directly (the source of
//! truth the sidecar is derived from; never stale).

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use regex::Regex;

use crate::index::IndexRecord;
use crate::query::Query;
use crate::store::{Layer, Store, StoreError};

/// Which edge directions a traversal follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Incoming edges only (backlinks).
    Incoming,
    /// Outgoing edges only (forwardlinks).
    Outgoing,
    /// Both directions.
    Both,
}

/// One node reached during a [`neighborhood`] hydration: the file, its
/// `summary`, and how it connects back toward the seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextNode {
    /// The store-relative path of the reached file.
    pub path: PathBuf,
    /// The file's `summary` (read from its sidecar entry / frontmatter).
    pub summary: String,
    /// The file's `type`, when known.
    pub type_: Option<String>,
    /// Hop distance from the seed (the seed itself is 0).
    pub hops: u32,
    /// The relationship edge that brought this node into the slice: the path it
    /// links to/from one hop closer to the seed, and the direction.
    pub via: Option<(PathBuf, Direction)>,
}

/// The readable working-set digest [`neighborhood`] returns: the seed plus the
/// reached nodes with their summaries and connections. The relationship-axis
/// "turn a seed into context" primitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSlice {
    /// The seed the slice was hydrated from.
    pub seed: PathBuf,
    /// The reached nodes (excluding the seed), in BFS order.
    pub nodes: Vec<ContextNode>,
}

/// Incoming edges to `path`: files that wiki-link to it. The blast-radius /
/// dependents primitive before an edit. Store-wide (every layer / every type);
/// see [`backlinks_filtered`] for the `--type` / `--in`-scoped form.
///
/// `path` is the store-relative target as it would be written inside a
/// wiki-link (with or without a trailing `.md`; both resolve to the same
/// target). Returns each linking file as its **canonical bare wiki-link path**
/// (store-relative, no `.md`) — the same key [`forwardlinks`] emits, so the two
/// directions round-trip and [`neighborhood`] can use one node identity.
/// Deduped, sorted, never including the seed itself.
pub fn backlinks(store: &Store, path: &Path) -> Result<Vec<PathBuf>, StoreError> {
    backlinks_filtered(store, path, &[], None)
}

/// Incoming edges to `path`, scoped by the linking file's `type` and/or layer —
/// the `dbmd graph backlinks --type/--in` surface.
///
/// **Scale (the loop contract).** Two paths, by whether the call is scoped:
///
/// - **Unscoped** (`types` empty *and* `layer` `None`): one embedded-ripgrep
///   pass for `[[<target>]]` across the store via [`Store::find_links_to`] — a
///   single `grep` + `ignore` traversal with early-exit per file, never a
///   `read_to_string` of every content file. This is the same scan engine
///   [`crate::validate::validate_working_set`]'s incoming-linker step rides, and
///   it keeps the unscoped call inside the loop budget (the old per-candidate
///   confirm-read re-opened every file in the store → O(store)).
/// - **Scoped** (`types` and/or `layer` set): the candidate set — the files that
///   *might* link to `path` — is read from the relevant type-folder
///   `index.jsonl` sidecars, so the call touches only the named folder(s):
///   O(folder), the sanctioned loop cost. Each candidate is then confirmed by a
///   single-file parse. When `types` lists several types, every named type's
///   folder is read and the candidate sets unioned; a `layer` further restricts
///   the candidate paths to that layer.
///
/// **Correctness (one edge set, both paths).** An incoming edge to X is exactly:
/// some file whose [`forwardlinks`] contains X — a wiki-link in the body or in
/// *any* frontmatter field (`company: [[…]]`, `attendees: [ … ]`), not just the
/// sidecar's frontmatter `links:` projection. Both paths honor that:
/// - The unscoped scan matches the literal `[[<target>]]` text wherever it lives
///   in a file (frontmatter or body), the same edges [`forwardlinks`] extracts.
///   [`Store::find_links_to`] returns *every* `.md` carrying the link text
///   (including `index.md` catalogs); [`backlinks`] is the relationship view, so
///   the results are filtered to content files ([`is_content_rel`]) and emitted
///   as canonical bare targets, self-excluded.
/// - The scoped path confirms each candidate via [`file_links_to`], which
///   delegates to [`forwardlinks`] (body + every frontmatter field) — so a
///   body-only or typed-field edge is caught, not just the sidecar's `links:`
///   list.
///
/// Result form (canonical bare paths, deduped, sorted, seed excluded) is
/// identical on both paths and matches [`backlinks`].
pub fn backlinks_filtered(
    store: &Store,
    path: &Path,
    types: &[String],
    layer: Option<Layer>,
) -> Result<Vec<PathBuf>, StoreError> {
    let target = normalize_target(path);
    if target.is_empty() {
        return Ok(Vec::new());
    }

    // Unscoped: one embedded-ripgrep pass over the store (O(store) scan with
    // early-exit per file), not a per-candidate read of every content file.
    // `find_links_to` returns every `.md` carrying the link text (incl. catalog
    // `index.md`); narrow to content files and canonicalize to the bare target
    // form `backlinks` emits, dropping the seed's self-link.
    if types.is_empty() && layer.is_none() {
        let mut hits: BTreeSet<PathBuf> = BTreeSet::new();
        for rel in store.find_links_to(path)? {
            if !is_content_rel(&rel) {
                continue;
            }
            let linker = normalize_target(&rel);
            if linker.is_empty() || linker == target {
                // A file never counts as its own backlink.
                continue;
            }
            hits.insert(PathBuf::from(linker));
        }
        return Ok(hits.into_iter().collect());
    }

    // Scoped: read only the named folder(s)' sidecars for the candidate set, then
    // confirm each candidate with a single-file parse — O(folder), the I/O scope
    // `--type` / `--in` buys.
    let mut hits: BTreeSet<PathBuf> = BTreeSet::new();
    for candidate in candidate_records(store, types, layer)? {
        let rel = &candidate.path;
        let candidate_target = normalize_target(rel);
        if candidate_target.is_empty() || candidate_target == target {
            // A file never counts as its own backlink.
            continue;
        }
        // Confirm the edge by parsing the candidate file the same way
        // forwardlinks does (body + all frontmatter), so body/typed-field links
        // are caught — the sidecar's `links` field alone would miss them.
        if file_links_to(store, rel, &target)? {
            hits.insert(PathBuf::from(candidate_target));
        }
    }

    Ok(hits.into_iter().collect())
}

/// Outgoing edges from `path`: the wiki-link targets extracted from that single
/// file. Loop-fast; follow the evidence chain.
///
/// `path` is the store-relative path of the file to read. Targets are returned
/// as store-relative paths (bare, no `.md`), deduped and sorted; the file's
/// links to itself are dropped. A missing file yields an empty list (a
/// dangling seed has no outgoing edges to report — broken-link detection is
/// [`crate::validate`]'s job).
pub fn forwardlinks(store: &Store, path: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let self_target = normalize_target(path);
    let abs = match resolve_existing(store, path) {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    let body = match std::fs::read_to_string(&abs) {
        Ok(b) => b,
        // A file that isn't valid UTF-8 (e.g. a binary source) carries no
        // wiki-links we can extract.
        Err(e) if e.kind() == io::ErrorKind::InvalidData => return Ok(Vec::new()),
        Err(e) => return Err(StoreError::Io(e)),
    };

    let mut out: BTreeSet<PathBuf> = BTreeSet::new();
    for target in extract_link_targets(&body) {
        if target.is_empty() || target == self_target {
            continue;
        }
        out.insert(PathBuf::from(target));
    }
    Ok(out.into_iter().collect())
}

/// The candidate set for an incoming-edge scan: the sidecar records that could
/// link to the target, read from the type-folder `index.jsonl` sidecars (never
/// a content-tree walk). `types`/`layer` narrow *which* sidecars are read — the
/// I/O scope that keeps a typed/layer backlinks O(folder).
///
/// - `types` non-empty: read each type's folder sidecar (via [`Query`], which
///   sits on [`Store::read_type_index`]), optionally layer-scoped, and union the
///   records by path (a file appears once even if two type reads surface it).
/// - `types` empty: every sidecar record under `layer` (or store-wide when
///   `None`) via [`Store::sidecar_records`].
fn candidate_records(
    store: &Store,
    types: &[String],
    layer: Option<Layer>,
) -> Result<Vec<IndexRecord>, StoreError> {
    if types.is_empty() {
        return store.sidecar_records(layer);
    }
    let mut by_path: std::collections::BTreeMap<PathBuf, IndexRecord> =
        std::collections::BTreeMap::new();
    for type_ in types {
        let mut q = Query::new().with_type(type_);
        if let Some(layer) = layer {
            q = q.with_layer(layer);
        }
        for rec in q.execute(store)? {
            by_path.insert(rec.path.clone(), rec);
        }
    }
    Ok(by_path.into_values().collect())
}

/// True if the store file at `rel` carries a wiki-link whose canonical target
/// equals `target`. Delegates to [`forwardlinks`] so the incoming-edge predicate
/// is *exactly* the outgoing-edge extraction — body + every frontmatter field —
/// keeping the two directions on one edge set. `forwardlinks` already emits
/// canonical bare targets, so `target` (likewise normalized by the caller) is
/// compared directly. A missing/binary file links to nothing.
fn file_links_to(store: &Store, rel: &Path, target: &str) -> Result<bool, StoreError> {
    let edges = forwardlinks(store, rel)?;
    Ok(edges.iter().any(|e| e.as_os_str() == target))
}

/// **Context hydration.** Bounded BFS from `seed` over backlinks + forwardlinks
/// out to `hops`, reading each reached file's `summary` + relationship, and
/// returning a readable [`ContextSlice`]. Optionally filtered by `types` and
/// `direction`. On-demand; no maintained graph. What the agent reaches for to
/// assemble a working set in one call.
///
/// Traversal semantics:
/// - **`hops`** bounds true graph distance from the seed. `hops == 0` returns
///   an empty slice (the seed alone is no context).
/// - **`direction`** selects which edges are followed: `Incoming` walks
///   backlinks, `Outgoing` walks forwardlinks, `Both` walks the union.
/// - **`types`**, when non-empty, filters which reached nodes appear in the
///   slice — but traversal still passes *through* off-type nodes, so a
///   `meeting` two hops out is still reachable through a `contact` even when
///   filtering to `meeting`. (An empty `types` slice imposes no filter.)
/// - Each node records the lowest hop count at which it is first reached (BFS
///   order); the seed is never included as a node.
pub fn neighborhood(
    store: &Store,
    seed: &Path,
    hops: u32,
    types: &[String],
    direction: Direction,
) -> Result<ContextSlice, StoreError> {
    let seed_rel = PathBuf::from(normalize_target(seed));
    let type_filter: HashSet<&str> = types.iter().map(|s| s.as_str()).collect();

    // `discovered` guards against revisiting a node (and against re-adding the
    // seed). BFS by levels so the first time we reach a node is its true min
    // hop distance.
    let mut discovered: HashSet<PathBuf> = HashSet::new();
    discovered.insert(seed_rel.clone());

    let mut nodes: Vec<ContextNode> = Vec::new();
    let mut frontier: VecDeque<PathBuf> = VecDeque::new();
    frontier.push_back(seed_rel.clone());

    let mut hop = 0u32;
    while hop < hops && !frontier.is_empty() {
        hop += 1;
        let level_size = frontier.len();
        for _ in 0..level_size {
            let current = frontier.pop_front().expect("frontier non-empty");

            // Collect this node's edges in the requested direction(s). Each
            // edge carries the neighbor path + the direction we traversed it.
            let mut edges: Vec<(PathBuf, Direction)> = Vec::new();
            if matches!(direction, Direction::Outgoing | Direction::Both) {
                for nbr in forwardlinks(store, &current)? {
                    edges.push((nbr, Direction::Outgoing));
                }
            }
            if matches!(direction, Direction::Incoming | Direction::Both) {
                for nbr in backlinks(store, &current)? {
                    edges.push((nbr, Direction::Incoming));
                }
            }

            for (neighbor, dir) in edges {
                if !discovered.insert(neighbor.clone()) {
                    continue;
                }
                let (summary, type_) = read_summary_and_type(store, &neighbor);
                let include = type_filter.is_empty()
                    || type_
                        .as_deref()
                        .map(|t| type_filter.contains(t))
                        .unwrap_or(false);
                if include {
                    nodes.push(ContextNode {
                        path: neighbor.clone(),
                        summary,
                        type_,
                        hops: hop,
                        via: Some((current.clone(), dir)),
                    });
                }
                // Off-type nodes are not emitted but still seed the next BFS
                // level, so the type filter narrows the *result*, not the
                // reachable graph.
                frontier.push_back(neighbor);
            }
        }
    }

    Ok(ContextSlice {
        seed: seed_rel,
        nodes,
    })
}

/// **SWEEP.** Content files with no incoming AND no outgoing wiki-links — the
/// curation worklist ("ingested but not yet wired into the wiki"). Off the
/// loop. Optionally scoped to a layer.
///
/// A file is an orphan iff it neither links out to another store file nor is
/// linked to by one. Incoming edges are counted across the *whole* store
/// (a link from any layer un-orphans a file), even when `layer` scopes the
/// candidate set. Returns store-relative paths, sorted.
pub fn orphans(store: &Store, layer: Option<Layer>) -> Result<Vec<PathBuf>, StoreError> {
    // One walk of the whole store: for every content file, record (a) whether
    // it has any outgoing link, and (b) accumulate the set of every target any
    // file links to (its incoming-edge set). Both come from a single read per
    // file — the SWEEP cost.
    let all = walk_content_files(store)?;

    let mut linked_to: HashSet<PathBuf> = HashSet::new();
    let mut has_outgoing: HashMap<PathBuf, bool> = HashMap::new();

    for abs in &all {
        let rel = match rel_path(store, abs) {
            Some(r) => r,
            None => continue,
        };
        let self_target = normalize_target(&rel);

        let body = match std::fs::read_to_string(abs) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => String::new(),
            Err(e) => return Err(StoreError::Io(e)),
        };

        let mut outgoing = false;
        for target in extract_link_targets(&body) {
            if target.is_empty() || target == self_target {
                continue;
            }
            outgoing = true;
            linked_to.insert(PathBuf::from(target));
        }
        has_outgoing.insert(rel, outgoing);
    }

    let mut out: BTreeSet<PathBuf> = BTreeSet::new();
    for abs in &all {
        let rel = match rel_path(store, abs) {
            Some(r) => r,
            None => continue,
        };
        if let Some(layer) = layer {
            if path_layer(&rel) != Some(layer) {
                continue;
            }
        }
        let outgoing = has_outgoing.get(&rel).copied().unwrap_or(false);
        let incoming = linked_to.contains(&PathBuf::from(normalize_target(&rel)));
        if !outgoing && !incoming {
            out.insert(rel);
        }
    }

    Ok(out.into_iter().collect())
}

/// **Write-side.** Rewrite every incoming `[[old]]` wiki-link in `text` to
/// `[[new]]`, preserving any `|display` override and emitting the canonical bare
/// target (no `.md`). The write-side twin of [`backlinks`]: where `backlinks`
/// *finds* the files carrying an edge to `old`, this *retargets* that edge to
/// `new` inside one file's contents.
///
/// `old` and `new` are store-relative paths in the wiki-link sense — both are
/// passed through the same [`normalize_target`] the read side keys on, so the
/// `.md` and bare spellings of `old` collapse to one target and a match here is
/// exactly a match [`backlinks`] / [`Store::find_links_to`](crate::Store::find_links_to)
/// would report. A link is rewritten iff its normalized target equals
/// `normalize_target(old)`; prefix collisions (`old=a/b` vs `[[a/bc]]`) and
/// short-form links never match. Returns the rewritten text (identical to the
/// input when nothing matched), so the caller can cheaply detect a no-op.
///
/// Operates on the raw bytes (not a parser round-trip) so a link in frontmatter
/// or body is retargeted uniformly and nothing else is reflowed.
pub fn rewrite_links_to(text: &str, old: &Path, new: &Path) -> String {
    let old_target = normalize_target(old);
    let new_target = normalize_target(new);
    if old_target.is_empty() {
        // No target to match → never rewrite anything.
        return text.to_string();
    }

    let re = rewrite_link_re();
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for caps in re.captures_iter(text) {
        let whole = caps.get(0).expect("group 0 always present");
        let raw_target = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        // Match on the SAME normalized key the read side uses, so `[[old]]`,
        // `[[old.md]]`, and `[[./old]]` all retarget and a prefix like
        // `[[old-jr]]` never does.
        if normalize_target(Path::new(raw_target)) != old_target {
            continue;
        }
        // Copy the gap since the previous rewrite verbatim, then the rebuilt
        // link: canonical bare new target + the original display, if any.
        out.push_str(&text[last..whole.start()]);
        out.push_str("[[");
        out.push_str(&new_target);
        if let Some(display) = caps.get(2) {
            out.push('|');
            out.push_str(display.as_str());
        }
        out.push_str("]]");
        last = whole.end();
    }
    out.push_str(&text[last..]);
    out
}

// ── Private helpers ─────────────────────────────────────────────────────────

/// The wiki-link regex used by the **write side** ([`rewrite_links_to`]):
/// captures the target (group 1) AND the optional `|display` (group 2) so a
/// rewrite can re-emit the display verbatim. The target/display character
/// classes match [`wiki_link_re`] exactly, so the write side recognizes a link
/// iff the read side does — the two never disagree on what a `[[…]]` is.
fn rewrite_link_re() -> Regex {
    Regex::new(r"\[\[([^\]\|\n]+?)(?:\|([^\]\n]*))?\]\]")
        .expect("static wiki-link rewrite regex compiles")
}

/// Normalize a store-relative path into the canonical wiki-link target form:
/// forward slashes, no leading `./` or `/`, and no trailing `.md`. This is the
/// single key that incoming/outgoing edges are compared on, so the `.md` and
/// bare forms of a target unify.
fn normalize_target(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('\\', "/");
    while let Some(rest) = s.strip_prefix("./") {
        s = rest.to_string();
    }
    let s = s.trim_start_matches('/');
    let s = s.strip_suffix(".md").unwrap_or(s);
    s.trim().to_string()
}

/// The wiki-link regex: `[[target]]` / `[[target|display]]`. Captures the raw
/// target (group 1). Compiled once per call site; cheap.
fn wiki_link_re() -> Regex {
    // target = anything up to the first `|` or `]`. display (optional) is
    // discarded. Matches across a single line/body slice.
    Regex::new(r"\[\[([^\]\|\n]+?)(?:\|[^\]\n]*)?\]\]").expect("static wiki-link regex compiles")
}

/// Extract every wiki-link target from a body, normalized to the canonical
/// store-relative form. Order-preserving; duplicates kept (callers dedup).
fn extract_link_targets(body: &str) -> Vec<String> {
    let re = wiki_link_re();
    re.captures_iter(body)
        .filter_map(|c| c.get(1))
        .map(|m| normalize_target(Path::new(m.as_str().trim())))
        .filter(|t| !t.is_empty())
        .collect()
}

/// Resolve the store root + a store-relative path to the absolute on-disk file,
/// trying the path as written and then with a `.md` extension. `None` if
/// neither exists.
fn resolve_existing(store: &Store, store_relative: &Path) -> Option<PathBuf> {
    let direct = store.root.join(store_relative);
    if direct.is_file() {
        return Some(direct);
    }
    let normalized = normalize_target(store_relative);
    let with_md = store.root.join(format!("{normalized}.md"));
    if with_md.is_file() {
        return Some(with_md);
    }
    None
}

/// Convert an absolute path under the store root into its store-relative form.
fn rel_path(store: &Store, abs: &Path) -> Option<PathBuf> {
    abs.strip_prefix(&store.root).ok().map(|p| p.to_path_buf())
}

/// Which layer a store-relative path sits in, by its first component.
fn path_layer(rel: &Path) -> Option<Layer> {
    let first = rel.components().next()?;
    match first.as_os_str().to_str()? {
        "sources" => Some(Layer::Sources),
        "records" => Some(Layer::Records),
        "wiki" => Some(Layer::Wiki),
        _ => None,
    }
}

/// True if a store-relative path is a *content* file: under `sources/`,
/// `records/`, or `wiki/`, a `.md` file, and not an `index.md`. Meta files
/// (`DB.md`, `log.md`, `log/…`, sidecars) are excluded.
fn is_content_rel(rel: &Path) -> bool {
    if path_layer(rel).is_none() {
        return false;
    }
    match rel.extension().and_then(|e| e.to_str()) {
        Some("md") => {}
        _ => return false,
    }
    rel.file_name().and_then(|n| n.to_str()) != Some("index.md")
}

/// Walk every content `.md` file in the store via the **`ignore`** walker
/// (the ripgrep directory engine). Only the three layer roots
/// (`sources/`/`records/`/`wiki/`) are descended, so `DB.md`, `log.md`, and
/// `log/` at the store root are structurally never reached; hidden dirs and
/// per-folder `index.md` sidecars are filtered out ([`is_content_rel`]). Honors
/// `.gitignore` the way `rg` does. Returns absolute paths. SWEEP-class.
fn walk_content_files(store: &Store) -> Result<Vec<PathBuf>, StoreError> {
    let mut out = Vec::new();
    for layer in Layer::all() {
        let dir = store.root.join(layer_dir_name(layer));
        if !dir.is_dir() {
            continue;
        }
        let walker = WalkBuilder::new(&dir)
            .hidden(true)
            .git_ignore(true)
            .git_global(false)
            .require_git(false)
            .build();
        for result in walker {
            let entry = result.map_err(|e| StoreError::Search {
                root: store.root.clone(),
                message: format!("walk failed: {e}"),
            })?;
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let abs = entry.into_path();
            if let Some(rel) = rel_path(store, &abs) {
                if is_content_rel(&rel) {
                    out.push(abs);
                }
            }
        }
    }
    Ok(out)
}

/// The on-disk folder name for a layer. Mirrors `Layer::dir_name`; kept local
/// so the graph module owns its own copy rather than coupling to that body.
fn layer_dir_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Sources => "sources",
        Layer::Records => "records",
        Layer::Wiki => "wiki",
    }
}

/// Read a reached node's `summary` and `type` from its frontmatter. A missing
/// file, missing frontmatter, or unparseable YAML degrades to an empty summary
/// / unknown type rather than failing the whole hydration — `neighborhood` is
/// best-effort context assembly, not validation.
fn read_summary_and_type(store: &Store, rel: &Path) -> (String, Option<String>) {
    let abs = match resolve_existing(store, rel) {
        Some(a) => a,
        None => return (String::new(), None),
    };
    let text = match std::fs::read_to_string(&abs) {
        Ok(t) => t,
        Err(_) => return (String::new(), None),
    };
    let yaml = match frontmatter_block(&text) {
        Some(y) => y,
        None => return (String::new(), None),
    };
    let value: serde_norway::Value = match serde_norway::from_str(yaml) {
        Ok(v) => v,
        Err(_) => return (String::new(), None),
    };
    let summary = value
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let type_ = value
        .get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    (summary, type_)
}

/// Return the YAML between the opening and closing `---` fences (exclusive), or
/// `None` if the text has no leading frontmatter block. Local mirror of the
/// parser's split so the graph module stays self-contained.
fn frontmatter_block(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    // Find the closing fence: a line that is exactly `---`.
    let mut idx = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            return Some(&rest[..idx]);
        }
        idx += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    use crate::parser::Config;

    // ── Fixture builder ─────────────────────────────────────────────────────
    //
    // A real on-disk store in a tempdir. We write actual files (frontmatter +
    // wiki-links) and exercise the real code paths. The fixture constructs the
    // `Store` by its public fields rather than `Store::open`, so the graph
    // tests stand on their own and do not depend on any other module's
    // behavior. Each test asserts the behavior the SPEC promises, derived from
    // intent, never from echoing the function's own output.
    //
    // `backlinks` (and `neighborhood` in any incoming direction) enumerate their
    // candidate set from the type-folder `index.jsonl` sidecars — the loop
    // contract: never a whole-store content walk. A real db.md store maintains
    // those sidecars write-through, so a test that exercises backlinks must call
    // [`Fixture::reindex`] after writing its files to build them (the SWEEP that
    // `dbmd index rebuild` runs). Forwardlinks/orphans read content directly and
    // need no sidecar.

    struct Fixture {
        _tmp: TempDir,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().expect("tempdir");
            let root = tmp.path().to_path_buf();
            fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n# store\n").expect("DB.md");
            let store = Store {
                root,
                config: Config::default(),
            };
            Fixture { _tmp: tmp, store }
        }

        /// Write a content file at a store-relative path with the given type,
        /// summary, and body. Creates parent dirs.
        fn write(&self, rel: &str, type_: &str, summary: &str, body: &str) {
            let abs = self.store.root.join(rel);
            fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
            let contents = format!(
                "---\ntype: {type_}\ncreated: 2026-05-01T00:00:00Z\nupdated: 2026-05-01T00:00:00Z\nsummary: {summary}\n---\n{body}\n"
            );
            fs::write(&abs, contents).expect("write file");
        }

        /// Write a raw file verbatim (for frontmatter-shape edge cases).
        fn write_raw(&self, rel: &str, contents: &str) {
            let abs = self.store.root.join(rel);
            fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
            fs::write(&abs, contents).expect("write raw");
        }

        /// Build the type-folder `index.jsonl` sidecars from the content written
        /// so far — the state a real store is always in (write-through), and the
        /// candidate set `backlinks` reads. Call after writing files in any test
        /// that exercises `backlinks` or an incoming-direction `neighborhood`.
        fn reindex(&self) {
            crate::index::Index::rebuild_all(&self.store).expect("rebuild sidecars");
        }

        fn p(&self, rel: &str) -> PathBuf {
            PathBuf::from(rel)
        }
    }

    fn paths(v: &[PathBuf]) -> Vec<String> {
        v.iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    // ── normalize_target ────────────────────────────────────────────────────

    #[test]
    fn normalize_strips_md_and_leading_dotslash() {
        assert_eq!(
            normalize_target(Path::new("records/contacts/sarah.md")),
            "records/contacts/sarah"
        );
        assert_eq!(
            normalize_target(Path::new("./wiki/people/elena")),
            "wiki/people/elena"
        );
        assert_eq!(normalize_target(Path::new("/records/x")), "records/x");
        // Bare and `.md` forms must collapse to the same key, or edges won't unify.
        assert_eq!(
            normalize_target(Path::new("a/b")),
            normalize_target(Path::new("a/b.md"))
        );
    }

    // ── extract_link_targets (forwardlinks core) ────────────────────────────

    #[test]
    fn extract_handles_display_text_and_md_suffix() {
        let body = "See [[wiki/people/sarah-chen|Sarah]] and [[records/contacts/elena.md]].";
        let got = extract_link_targets(body);
        assert_eq!(
            got,
            vec!["wiki/people/sarah-chen", "records/contacts/elena"]
        );
    }

    #[test]
    fn extract_ignores_external_markdown_links() {
        // Standard markdown links are NOT wiki-links and must not be extracted
        // (SPEC: external refs don't participate in the graph).
        let body = "[Acme](https://acme.io) but [[records/companies/acme]] is internal.";
        let got = extract_link_targets(body);
        assert_eq!(got, vec!["records/companies/acme"]);
    }

    #[test]
    fn extract_display_text_is_not_treated_as_a_target() {
        // A `|display` segment that looks path-like must not become a target;
        // only the part before `|` is the link target.
        let body = "[[records/contacts/sarah|sources/emails/decoy]]";
        let got = extract_link_targets(body);
        assert_eq!(got, vec!["records/contacts/sarah"]);
    }

    // ── rewrite_links_to (write-side twin of backlinks) ─────────────────────

    #[test]
    fn rewrite_plain_link_to_canonical_new_target() {
        let got = rewrite_links_to(
            "See [[records/contacts/sarah-chen]] today.",
            Path::new("records/contacts/sarah-chen"),
            Path::new("records/contacts/sarah-chen-acme"),
        );
        assert_eq!(got, "See [[records/contacts/sarah-chen-acme]] today.");
    }

    #[test]
    fn rewrite_preserves_display_override() {
        let got = rewrite_links_to(
            "With [[records/contacts/sarah-chen|Sarah]].",
            Path::new("records/contacts/sarah-chen"),
            Path::new("records/contacts/sarah-chen-acme"),
        );
        assert_eq!(got, "With [[records/contacts/sarah-chen-acme|Sarah]].");
    }

    #[test]
    fn rewrite_matches_md_suffixed_old_and_emits_bare_new() {
        // The `.md` spelling of the old target must match (it normalizes to the
        // same key the read side uses), and the new target is emitted bare —
        // the writer doctrine validate enforces (`WIKI_LINK_HAS_EXTENSION`).
        let got = rewrite_links_to(
            "[[records/contacts/sarah-chen.md]]",
            Path::new("records/contacts/sarah-chen"),
            Path::new("records/contacts/new.md"),
        );
        assert_eq!(got, "[[records/contacts/new]]");
    }

    #[test]
    fn rewrite_leaves_prefix_collisions_and_short_form_untouched() {
        // Boundary correctness, anchored to the SAME normalize_target the read
        // side keys on: `records/contacts/sarah-chen` must NOT match the longer
        // `[[…-jr]]`, the short-form `[[sarah-chen]]`, or an unrelated target.
        let input = "[[records/contacts/sarah-chen-jr]] [[sarah-chen]] [[wiki/topics/x]]";
        let got = rewrite_links_to(
            input,
            Path::new("records/contacts/sarah-chen"),
            Path::new("records/contacts/new"),
        );
        assert_eq!(got, input, "no genuine edge to the seed → text unchanged");
    }

    #[test]
    fn rewrite_handles_multiple_occurrences_and_mixed_spellings() {
        let got = rewrite_links_to(
            "[[records/x]] then [[./records/x]] and [[records/x.md|d]] end",
            Path::new("records/x"),
            Path::new("records/y"),
        );
        // All three spellings of the same target retarget; the display survives.
        assert_eq!(
            got,
            "[[records/y]] then [[records/y]] and [[records/y|d]] end"
        );
    }

    #[test]
    fn rewrite_retargets_exactly_the_edges_the_core_parser_sees() {
        // The load-bearing property of moving the rewrite into core: the write
        // side must operate on EXACTLY the edge set the read side recognizes —
        // the same `extract_link_targets` / `normalize_target` grammar that
        // `forwardlinks` is built on. Anchor the test to that grammar (via
        // `forwardlinks` on a real file) rather than re-listing literals, so a
        // future divergence between the read parser and the write rewrite fails
        // here. (Coupled to `forwardlinks` — the single-file edge extractor —
        // not the multi-file `backlinks` traversal, so it tests the grammar, not
        // the walk.)
        let fx = Fixture::new();
        let body = "Met [[records/contacts/sarah.md|Sarah]] and not [[records/contacts/sarah-2]].";
        fx.write("wiki/people/bio.md", "wiki-page", "bio", body);

        // Read side: the parser sees two outgoing edges, both in canonical bare
        // form (the `.md` spelling collapsed). `sarah` is a real edge here.
        let edges = forwardlinks(&fx.store, &fx.p("wiki/people/bio.md")).unwrap();
        assert_eq!(
            paths(&edges),
            vec!["records/contacts/sarah", "records/contacts/sarah-2"],
            "fixture must contain exactly the two edges this test reasons about"
        );

        // Write side: rewriting `sarah → sarah-chen` must retarget the edge the
        // parser recognized (matching the `.md` spelling), preserve the display,
        // and leave the unrelated `sarah-2` edge untouched.
        let got = rewrite_links_to(
            body,
            Path::new("records/contacts/sarah"),
            Path::new("records/contacts/sarah-chen"),
        );
        assert_eq!(
            got,
            "Met [[records/contacts/sarah-chen|Sarah]] and not [[records/contacts/sarah-2]]."
        );

        // Cross-check through the parser: the rewritten text's edge set is the
        // original with `sarah` swapped for `sarah-chen` — proving the rewrite
        // moved exactly one edge, the one the read side keyed on.
        fx.write("wiki/people/bio.md", "wiki-page", "bio", &got);
        let after = forwardlinks(&fx.store, &fx.p("wiki/people/bio.md")).unwrap();
        assert_eq!(
            paths(&after),
            vec!["records/contacts/sarah-2", "records/contacts/sarah-chen"],
            "after rewrite the parser must see the new target and not the old"
        );
    }

    #[test]
    fn rewrite_empty_old_target_is_a_no_op() {
        // A degenerate `old` (normalizes to empty) must never rewrite anything,
        // mirroring backlinks' empty-target guard.
        let input = "[[records/x]] [[]] text";
        let got = rewrite_links_to(input, Path::new(""), Path::new("records/y"));
        assert_eq!(got, input);
    }

    #[test]
    fn rewrite_no_match_returns_input_unchanged() {
        let input = "no links, [external](https://x), and [[wiki/topics/y]]";
        let got = rewrite_links_to(input, Path::new("records/x"), Path::new("records/z"));
        assert_eq!(got, input);
    }

    // ── forwardlinks ─────────────────────────────────────────────────────────

    #[test]
    fn forwardlinks_returns_sorted_deduped_targets_excluding_self() {
        let fx = Fixture::new();
        fx.write(
            "wiki/projects/renewal.md",
            "wiki-page",
            "Renewal project",
            "Links: [[records/contacts/sarah]] [[records/companies/acme]] [[records/contacts/sarah]] and itself [[wiki/projects/renewal]].",
        );
        // The targets need not exist on disk for forwardlinks (it reads the one
        // file only). Self-links are dropped; duplicates collapse; sorted asc.
        let got = forwardlinks(&fx.store, &fx.p("wiki/projects/renewal.md")).unwrap();
        assert_eq!(
            paths(&got),
            vec!["records/companies/acme", "records/contacts/sarah"]
        );
    }

    #[test]
    fn forwardlinks_picks_up_wiki_links_in_frontmatter() {
        // SPEC: wiki-links appear in scalar + block-sequence frontmatter fields,
        // not just the body. forwardlinks must follow those edges too.
        let fx = Fixture::new();
        fx.write_raw(
            "records/meetings/m1.md",
            "---\ntype: meeting\ncreated: 2026-05-01T00:00:00Z\nupdated: 2026-05-01T00:00:00Z\nsummary: Renewal sync\ncompany: [[records/companies/acme]]\nattendees:\n  - [[records/contacts/sarah]]\n  - [[records/contacts/elena]]\n---\nNotes about [[wiki/projects/renewal]].\n",
        );
        let got = forwardlinks(&fx.store, &fx.p("records/meetings/m1.md")).unwrap();
        assert_eq!(
            paths(&got),
            vec![
                "records/companies/acme",
                "records/contacts/elena",
                "records/contacts/sarah",
                "wiki/projects/renewal",
            ]
        );
    }

    #[test]
    fn forwardlinks_missing_file_is_empty_not_error() {
        let fx = Fixture::new();
        let got = forwardlinks(&fx.store, &fx.p("wiki/people/ghost.md")).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn forwardlinks_resolves_seed_given_without_md_extension() {
        let fx = Fixture::new();
        fx.write(
            "wiki/people/sarah.md",
            "wiki-page",
            "Sarah bio",
            "Works at [[records/companies/acme]].",
        );
        // Seed passed in bare wiki-link form (no `.md`) must still resolve.
        let got = forwardlinks(&fx.store, &fx.p("wiki/people/sarah")).unwrap();
        assert_eq!(paths(&got), vec!["records/companies/acme"]);
    }

    // ── backlinks ──────────────────────────────────────────────────────────

    #[test]
    fn backlinks_finds_incoming_across_layers_and_link_forms() {
        let fx = Fixture::new();
        // Target.
        fx.write("records/contacts/sarah.md", "contact", "Sarah Chen", "");
        // Three different incoming-link spellings, all to the same target.
        fx.write(
            "wiki/people/sarah.md",
            "wiki-page",
            "bio",
            "See [[records/contacts/sarah]].",
        );
        fx.write(
            "records/meetings/m1.md",
            "meeting",
            "Renewal call",
            "Attendee [[records/contacts/sarah|Sarah]].",
        );
        fx.write(
            "sources/emails/e1.md",
            "email",
            "Hi",
            "From [[records/contacts/sarah.md]] today.",
        );
        // A file that links to a DIFFERENT contact must not be a backlink.
        fx.write(
            "wiki/people/other.md",
            "wiki-page",
            "x",
            "[[records/contacts/sarah-2]]",
        );
        fx.reindex();

        // All three link forms ([[x]], [[x|d]], [[x.md]]) resolve to the same
        // target and are found; the linkers are returned in canonical bare form.
        let got = backlinks(&fx.store, &fx.p("records/contacts/sarah.md")).unwrap();
        assert_eq!(
            paths(&got),
            vec![
                "records/meetings/m1",
                "sources/emails/e1",
                "wiki/people/sarah",
            ]
        );
    }

    #[test]
    fn backlinks_and_forwardlinks_round_trip_on_same_key() {
        // If A forwardlinks to B, then B backlinks to A — both expressed in the
        // identical bare key, so neighborhood can dedup across directions.
        let fx = Fixture::new();
        fx.write(
            "wiki/people/a.md",
            "wiki-page",
            "A",
            "Knows [[wiki/people/b]].",
        );
        fx.write("wiki/people/b.md", "wiki-page", "B", "");
        fx.reindex();
        let fwd = forwardlinks(&fx.store, &fx.p("wiki/people/a.md")).unwrap();
        let back = backlinks(&fx.store, &fx.p("wiki/people/b.md")).unwrap();
        assert_eq!(paths(&fwd), vec!["wiki/people/b"]);
        assert_eq!(paths(&back), vec!["wiki/people/a"]);
    }

    #[test]
    fn backlinks_does_not_match_path_prefix_collisions() {
        let fx = Fixture::new();
        fx.write("records/contacts/sam.md", "contact", "Sam", "");
        // `sam-smith` shares the `sam` prefix; must NOT count as a backlink to `sam`.
        fx.write(
            "wiki/people/x.md",
            "wiki-page",
            "x",
            "[[records/contacts/sam-smith]]",
        );
        // The genuine backlink.
        fx.write(
            "wiki/people/y.md",
            "wiki-page",
            "y",
            "[[records/contacts/sam]]",
        );
        fx.reindex();

        let got = backlinks(&fx.store, &fx.p("records/contacts/sam")).unwrap();
        assert_eq!(paths(&got), vec!["wiki/people/y"]);
    }

    #[test]
    fn backlinks_excludes_self_reference() {
        let fx = Fixture::new();
        // A page that links to itself is not its own backlink.
        fx.write(
            "wiki/synthesis/overview.md",
            "wiki-page",
            "Overview",
            "This page [[wiki/synthesis/overview]] references itself.",
        );
        fx.reindex();
        let got = backlinks(&fx.store, &fx.p("wiki/synthesis/overview.md")).unwrap();
        assert!(
            got.is_empty(),
            "self-link must not appear as a backlink, got {got:?}"
        );
    }

    #[test]
    fn backlinks_empty_when_nobody_links() {
        let fx = Fixture::new();
        fx.write("records/contacts/lonely.md", "contact", "Lonely", "");
        fx.write(
            "wiki/people/unrelated.md",
            "wiki-page",
            "x",
            "[[records/companies/acme]]",
        );
        fx.reindex();
        let got = backlinks(&fx.store, &fx.p("records/contacts/lonely.md")).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn backlinks_ignores_index_and_meta_files() {
        let fx = Fixture::new();
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        // An index.md that lists the target must NOT be reported as a backlink
        // (indexes are catalog, not relationship edges).
        fx.write_raw(
            "records/contacts/index.md",
            "---\ntype: index\nscope: folder\nfolder: records/contacts\n---\n- [[records/contacts/sarah]] — Sarah\n",
        );
        fx.reindex();
        let got = backlinks(&fx.store, &fx.p("records/contacts/sarah.md")).unwrap();
        assert!(got.is_empty(), "index.md must be excluded, got {got:?}");
    }

    #[test]
    fn backlinks_finds_body_only_edge_not_in_frontmatter_links_field() {
        // REGRESSION: the sidecar's `links` field carries only the file's
        // frontmatter `links:` list; it does NOT include wiki-links written in
        // the body or in other typed frontmatter fields. Answering backlinks
        // from `links[]` alone would silently miss this edge. The candidate set
        // is sidecar-bounded, but each candidate's edge is confirmed by parsing
        // the file (the same extraction forwardlinks uses), so a body-only link
        // must still register as a backlink.
        let fx = Fixture::new();
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        // `meeting.md` links to sarah ONLY in its body — its frontmatter has no
        // `links:` field at all, so the sidecar record's `links` is empty.
        fx.write(
            "records/meetings/standup.md",
            "meeting",
            "Standup",
            "Discussed renewal with [[records/contacts/sarah]].",
        );
        fx.reindex();

        // Guard the premise: the sidecar record really does carry an empty
        // `links` (so this test fails loudly if the index ever starts extracting
        // body links — at which point the backlink predicate could be revisited).
        let rec = fx
            .store
            .find_by_type("meeting")
            .unwrap()
            .into_iter()
            .find(|r| r.path == fx.p("records/meetings/standup.md"))
            .expect("meeting is catalogued in its sidecar");
        assert!(
            rec.links.is_empty(),
            "premise: the body link is NOT projected into the sidecar `links` field; got {:?}",
            rec.links
        );

        // Yet backlinks still finds it — because it confirms via the file parse,
        // not via the sidecar `links` field.
        let got = backlinks(&fx.store, &fx.p("records/contacts/sarah.md")).unwrap();
        assert_eq!(
            paths(&got),
            vec!["records/meetings/standup"],
            "a body-only wiki-link must register as a backlink"
        );
    }

    #[test]
    fn backlinks_finds_edge_in_typed_frontmatter_field() {
        // A wiki-link inside a *typed* frontmatter field (`company:`) is a real
        // edge forwardlinks follows, so backlinks must find it too — even though
        // the sidecar's `links` field (the `links:` key only) does not list it.
        let fx = Fixture::new();
        fx.write("records/companies/acme.md", "company", "Acme", "");
        fx.write_raw(
            "records/contacts/sarah.md",
            "---\ntype: contact\ncreated: 2026-05-01T00:00:00Z\nupdated: 2026-05-01T00:00:00Z\nsummary: Sarah\ncompany: [[records/companies/acme]]\n---\nBody with no links.\n",
        );
        fx.reindex();
        let got = backlinks(&fx.store, &fx.p("records/companies/acme.md")).unwrap();
        assert_eq!(
            paths(&got),
            vec!["records/contacts/sarah"],
            "a wiki-link in a typed frontmatter field is an incoming edge"
        );
    }

    #[test]
    fn backlinks_unscoped_scans_the_tree_not_only_the_sidecar() {
        // REGRESSION (loop budget): an UNSCOPED `backlinks` must resolve incoming
        // edges with a SINGLE embedded-ripgrep pass over the tree
        // (`Store::find_links_to`), NOT by reading the sidecar candidate set and
        // then `read_to_string`-confirming each candidate (which re-opens every
        // content file → O(store); the documented >3x budget miss). A ripgrep
        // pass is the same scan engine `validate`/`rename`/`dbmd links` ride, and
        // the tree — not the sidecar — is its ground truth: a linker that is on
        // disk but absent from every sidecar (stale / never-built index) is still
        // found. We assert that behaviorally, which fails loudly if the unscoped
        // path ever reverts to the sidecar-bounded per-candidate confirm loop
        // (that loop would NOT find the unindexed linker).
        let fx = Fixture::new();
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        fx.write(
            "wiki/people/indexed.md",
            "wiki-page",
            "Indexed",
            "[[records/contacts/sarah]]",
        );
        fx.reindex(); // builds sidecars for sarah + the indexed linker

        // Now drop a NEW linker on disk WITHOUT reindexing — it is on disk but in
        // no sidecar.
        fx.write(
            "wiki/people/unindexed.md",
            "wiki-page",
            "Unindexed",
            "[[records/contacts/sarah]]",
        );

        let got = backlinks(&fx.store, &fx.p("records/contacts/sarah.md")).unwrap();
        assert_eq!(
            paths(&got),
            vec!["wiki/people/indexed", "wiki/people/unindexed"],
            "unscoped backlinks ripgrep-scans the tree, so the on-disk-but-unindexed \
             linker is found too — not only the sidecar-catalogued one"
        );
    }

    #[test]
    fn backlinks_scoped_candidates_come_from_the_sidecar_not_a_tree_walk() {
        // REGRESSION (scale contract): the SCOPED form (`--type` / `--in`) is the
        // I/O-scoped path — it enumerates candidates from the relevant type-folder
        // `index.jsonl` sidecars and parses only those, NOT a whole-tree walk.
        // That is what makes the scope an I/O scope, not just a result filter:
        // a linker that is on disk but ABSENT from the sidecar (stale / never-built
        // index) is NOT discovered by the scoped call (the sidecar bounds which
        // files are candidates). This is the loop-vs-walk distinction the SPEC
        // draws, and it is exactly the inverse of the unscoped tree scan above.
        let fx = Fixture::new();
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        fx.write(
            "wiki/people/indexed.md",
            "wiki-page",
            "Indexed",
            "[[records/contacts/sarah]]",
        );
        fx.reindex(); // builds sidecars for sarah + the indexed linker

        // Drop a NEW wiki-page linker on disk WITHOUT reindexing — on disk, in no
        // sidecar.
        fx.write(
            "wiki/people/unindexed.md",
            "wiki-page",
            "Unindexed",
            "[[records/contacts/sarah]]",
        );

        // Scoped to the `wiki-page` type: the candidate set is the sidecar's, so
        // only the catalogued linker is found — the unindexed one is invisible.
        let only_wiki_pages = vec!["wiki-page".to_string()];
        let got = backlinks_filtered(
            &fx.store,
            &fx.p("records/contacts/sarah.md"),
            &only_wiki_pages,
            None,
        )
        .unwrap();
        assert_eq!(
            paths(&got),
            vec!["wiki/people/indexed"],
            "scoped backlinks reads the sidecar candidate set; the on-disk-but-unindexed \
             linker is not tree-walked"
        );
    }

    #[test]
    fn backlinks_filtered_type_scopes_the_candidate_set() {
        // `--type` narrows backlinks to linkers of that type. Two files link to
        // the target — one `meeting`, one `wiki-page`; filtering to `meeting`
        // returns only the meeting.
        let fx = Fixture::new();
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        fx.write(
            "records/meetings/m1.md",
            "meeting",
            "Call",
            "[[records/contacts/sarah]]",
        );
        fx.write(
            "wiki/people/bio.md",
            "wiki-page",
            "Bio",
            "[[records/contacts/sarah]]",
        );
        fx.reindex();

        let only_meetings = vec!["meeting".to_string()];
        let got = backlinks_filtered(
            &fx.store,
            &fx.p("records/contacts/sarah.md"),
            &only_meetings,
            None,
        )
        .unwrap();
        assert_eq!(
            paths(&got),
            vec!["records/meetings/m1"],
            "--type meeting must exclude the wiki-page linker"
        );

        // Unfiltered, both come back — proving the filter (not the data) dropped one.
        let all = backlinks(&fx.store, &fx.p("records/contacts/sarah.md")).unwrap();
        assert_eq!(paths(&all), vec!["records/meetings/m1", "wiki/people/bio"]);
    }

    #[test]
    fn backlinks_filtered_layer_scopes_the_candidate_set() {
        // `--in <layer>` narrows backlinks to linkers under that layer.
        let fx = Fixture::new();
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        fx.write(
            "records/meetings/m1.md",
            "meeting",
            "Call",
            "[[records/contacts/sarah]]",
        );
        fx.write(
            "wiki/people/bio.md",
            "wiki-page",
            "Bio",
            "[[records/contacts/sarah]]",
        );
        fx.reindex();

        let got = backlinks_filtered(
            &fx.store,
            &fx.p("records/contacts/sarah.md"),
            &[],
            Some(Layer::Wiki),
        )
        .unwrap();
        assert_eq!(
            paths(&got),
            vec!["wiki/people/bio"],
            "--in wiki must keep only the wiki-layer linker"
        );

        let records_only = backlinks_filtered(
            &fx.store,
            &fx.p("records/contacts/sarah.md"),
            &[],
            Some(Layer::Records),
        )
        .unwrap();
        assert_eq!(paths(&records_only), vec!["records/meetings/m1"]);
    }

    // ── neighborhood ─────────────────────────────────────────────────────────

    #[test]
    fn neighborhood_hops_zero_is_empty() {
        let fx = Fixture::new();
        fx.write("wiki/people/a.md", "wiki-page", "A", "[[wiki/people/b]]");
        fx.write("wiki/people/b.md", "wiki-page", "B", "");
        let slice = neighborhood(
            &fx.store,
            &fx.p("wiki/people/a.md"),
            0,
            &[],
            Direction::Both,
        )
        .unwrap();
        assert_eq!(slice.seed, fx.p("wiki/people/a"));
        assert!(slice.nodes.is_empty());
    }

    #[test]
    fn neighborhood_outgoing_one_hop_reads_summary_and_type() {
        let fx = Fixture::new();
        fx.write(
            "wiki/people/a.md",
            "wiki-page",
            "Person A",
            "Knows [[records/contacts/b]].",
        );
        fx.write("records/contacts/b.md", "contact", "Contact B summary", "");
        let slice = neighborhood(
            &fx.store,
            &fx.p("wiki/people/a.md"),
            1,
            &[],
            Direction::Outgoing,
        )
        .unwrap();
        assert_eq!(slice.nodes.len(), 1);
        let n = &slice.nodes[0];
        assert_eq!(n.path, fx.p("records/contacts/b"));
        assert_eq!(n.summary, "Contact B summary");
        assert_eq!(n.type_.as_deref(), Some("contact"));
        assert_eq!(n.hops, 1);
        assert_eq!(n.via, Some((fx.p("wiki/people/a"), Direction::Outgoing)));
    }

    #[test]
    fn neighborhood_incoming_only_walks_backlinks() {
        let fx = Fixture::new();
        // a -> seed (incoming to seed). seed -> c (outgoing from seed).
        fx.write(
            "wiki/people/seed.md",
            "wiki-page",
            "Seed",
            "Out to [[wiki/people/c]].",
        );
        fx.write(
            "wiki/people/a.md",
            "wiki-page",
            "A",
            "In to [[wiki/people/seed]].",
        );
        fx.write("wiki/people/c.md", "wiki-page", "C", "");
        fx.reindex();
        let slice = neighborhood(
            &fx.store,
            &fx.p("wiki/people/seed.md"),
            1,
            &[],
            Direction::Incoming,
        )
        .unwrap();
        // Incoming direction: only `a` (which links TO seed), not `c`.
        assert_eq!(
            paths(
                &slice
                    .nodes
                    .iter()
                    .map(|n| n.path.clone())
                    .collect::<Vec<_>>()
            ),
            vec!["wiki/people/a"]
        );
        assert_eq!(
            slice.nodes[0].via,
            Some((fx.p("wiki/people/seed"), Direction::Incoming))
        );
    }

    #[test]
    fn neighborhood_bounded_bfs_respects_hop_limit_and_min_distance() {
        let fx = Fixture::new();
        // Chain a -> b -> c -> d, all outgoing.
        fx.write("wiki/c/a.md", "wiki-page", "A", "[[wiki/c/b]]");
        fx.write("wiki/c/b.md", "wiki-page", "B", "[[wiki/c/c]]");
        fx.write("wiki/c/c.md", "wiki-page", "C", "[[wiki/c/d]]");
        fx.write("wiki/c/d.md", "wiki-page", "D", "");
        let slice =
            neighborhood(&fx.store, &fx.p("wiki/c/a.md"), 2, &[], Direction::Outgoing).unwrap();
        // 2 hops reaches b (1) and c (2), not d (3).
        let by_path: HashMap<String, u32> = slice
            .nodes
            .iter()
            .map(|n| (n.path.to_string_lossy().to_string(), n.hops))
            .collect();
        assert_eq!(by_path.get("wiki/c/b").copied(), Some(1));
        assert_eq!(by_path.get("wiki/c/c").copied(), Some(2));
        assert_eq!(by_path.get("wiki/c/d"), None);
        assert_eq!(slice.nodes.len(), 2);
    }

    #[test]
    fn neighborhood_records_min_hops_on_diamond() {
        let fx = Fixture::new();
        // Diamond: a -> b, a -> c, b -> d, c -> d. d is reachable at hop 2 from
        // either branch; it must be recorded once, at hop 2.
        fx.write("wiki/d/a.md", "wiki-page", "A", "[[wiki/d/b]] [[wiki/d/c]]");
        fx.write("wiki/d/b.md", "wiki-page", "B", "[[wiki/d/d]]");
        fx.write("wiki/d/c.md", "wiki-page", "C", "[[wiki/d/d]]");
        fx.write("wiki/d/d.md", "wiki-page", "D", "");
        let slice =
            neighborhood(&fx.store, &fx.p("wiki/d/a.md"), 3, &[], Direction::Outgoing).unwrap();
        let d_nodes: Vec<&ContextNode> = slice
            .nodes
            .iter()
            .filter(|n| n.path == fx.p("wiki/d/d"))
            .collect();
        assert_eq!(d_nodes.len(), 1, "d must appear exactly once");
        assert_eq!(d_nodes[0].hops, 2, "d's min distance from a is 2");
        // b and c at hop 1, d at hop 2 => 3 nodes total, no cycle blowup.
        assert_eq!(slice.nodes.len(), 3);
    }

    #[test]
    fn neighborhood_type_filter_narrows_results_but_not_traversal() {
        let fx = Fixture::new();
        // seed -> contact -> meeting. Filtering to `meeting` must still reach
        // the meeting THROUGH the (excluded) contact at hop 2.
        fx.write(
            "wiki/people/seed.md",
            "wiki-page",
            "Seed",
            "[[records/contacts/sarah]]",
        );
        fx.write(
            "records/contacts/sarah.md",
            "contact",
            "Sarah",
            "[[records/meetings/m1]]",
        );
        fx.write("records/meetings/m1.md", "meeting", "Renewal call", "");
        let only_meetings = vec!["meeting".to_string()];
        let slice = neighborhood(
            &fx.store,
            &fx.p("wiki/people/seed.md"),
            2,
            &only_meetings,
            Direction::Outgoing,
        )
        .unwrap();
        // Only the meeting is returned; the contact is traversed but filtered out.
        assert_eq!(slice.nodes.len(), 1);
        assert_eq!(slice.nodes[0].path, fx.p("records/meetings/m1"));
        assert_eq!(slice.nodes[0].type_.as_deref(), Some("meeting"));
        assert_eq!(slice.nodes[0].hops, 2);
    }

    #[test]
    fn neighborhood_cycle_terminates() {
        let fx = Fixture::new();
        // a <-> b cycle. Must not loop forever; each appears once.
        fx.write("wiki/g/a.md", "wiki-page", "A", "[[wiki/g/b]]");
        fx.write("wiki/g/b.md", "wiki-page", "B", "[[wiki/g/a]]");
        fx.reindex();
        let slice =
            neighborhood(&fx.store, &fx.p("wiki/g/a.md"), 10, &[], Direction::Both).unwrap();
        // From a: b is the only other node (a is the seed, excluded).
        assert_eq!(
            paths(
                &slice
                    .nodes
                    .iter()
                    .map(|n| n.path.clone())
                    .collect::<Vec<_>>()
            ),
            vec!["wiki/g/b"]
        );
    }

    // ── orphans ──────────────────────────────────────────────────────────────

    #[test]
    fn orphans_finds_files_with_no_edges_either_direction() {
        let fx = Fixture::new();
        // Wired pair: a links to b (a has outgoing, b has incoming).
        fx.write("wiki/people/a.md", "wiki-page", "A", "[[wiki/people/b]]");
        fx.write("wiki/people/b.md", "wiki-page", "B", "");
        // Orphan: no links in or out.
        fx.write(
            "sources/emails/lonely.md",
            "email",
            "Lonely email",
            "Just text, no links.",
        );
        let got = orphans(&fx.store, None).unwrap();
        assert_eq!(paths(&got), vec!["sources/emails/lonely.md"]);
    }

    #[test]
    fn orphans_file_with_only_outgoing_is_not_orphan() {
        let fx = Fixture::new();
        // `a` links out to a target that does NOT exist on disk. `a` still has
        // an outgoing edge, so it is not an orphan (orphan = no edges at all).
        fx.write(
            "wiki/people/a.md",
            "wiki-page",
            "A",
            "[[records/contacts/ghost]]",
        );
        let got = orphans(&fx.store, None).unwrap();
        assert!(
            !paths(&got).contains(&"wiki/people/a.md".to_string()),
            "outgoing-only is not an orphan: {got:?}"
        );
    }

    #[test]
    fn orphans_file_with_only_incoming_is_not_orphan() {
        let fx = Fixture::new();
        // `target` has no outgoing links but IS linked to by `linker` — not an orphan.
        fx.write("records/contacts/target.md", "contact", "Target", "");
        fx.write(
            "wiki/people/linker.md",
            "wiki-page",
            "Linker",
            "[[records/contacts/target]]",
        );
        let got = orphans(&fx.store, None).unwrap();
        assert!(
            !paths(&got).contains(&"records/contacts/target.md".to_string()),
            "incoming-only is not an orphan: {got:?}"
        );
        // `linker` has outgoing, so also not an orphan.
        assert!(!paths(&got).contains(&"wiki/people/linker.md".to_string()));
    }

    #[test]
    fn orphans_incoming_link_from_other_layer_unorphans() {
        let fx = Fixture::new();
        // Candidate in records/, only incoming edge comes from wiki/ — a
        // cross-layer link must still un-orphan it even when scoped to records.
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        fx.write(
            "wiki/people/sarah.md",
            "wiki-page",
            "bio",
            "[[records/contacts/sarah]]",
        );
        // A genuine orphan in records/ to prove the scope still returns something.
        fx.write("records/contacts/nemo.md", "contact", "Nemo", "");
        let got = orphans(&fx.store, Some(Layer::Records)).unwrap();
        assert_eq!(paths(&got), vec!["records/contacts/nemo.md"]);
    }

    #[test]
    fn orphans_layer_scope_filters_candidates() {
        let fx = Fixture::new();
        // One orphan per layer.
        fx.write("sources/emails/s.md", "email", "S", "no links");
        fx.write("records/contacts/r.md", "contact", "R", "");
        fx.write("wiki/people/w.md", "wiki-page", "W", "");
        let only_wiki = orphans(&fx.store, Some(Layer::Wiki)).unwrap();
        assert_eq!(paths(&only_wiki), vec!["wiki/people/w.md"]);
        let only_sources = orphans(&fx.store, Some(Layer::Sources)).unwrap();
        assert_eq!(paths(&only_sources), vec!["sources/emails/s.md"]);
        // No scope: all three, sorted (records, sources, wiki).
        let all = orphans(&fx.store, None).unwrap();
        assert_eq!(
            paths(&all),
            vec![
                "records/contacts/r.md",
                "sources/emails/s.md",
                "wiki/people/w.md",
            ]
        );
    }

    #[test]
    fn orphans_self_link_does_not_count_as_an_edge() {
        let fx = Fixture::new();
        // A page that only links to itself has no real edges => still an orphan.
        fx.write(
            "wiki/synthesis/solo.md",
            "wiki-page",
            "Solo",
            "I reference [[wiki/synthesis/solo]] only.",
        );
        let got = orphans(&fx.store, None).unwrap();
        assert_eq!(paths(&got), vec!["wiki/synthesis/solo.md"]);
    }

    #[test]
    fn orphans_excludes_index_and_db_files() {
        let fx = Fixture::new();
        // A lone index.md / DB.md must never be reported as an orphan content file.
        fx.write_raw(
            "wiki/index.md",
            "---\ntype: index\nscope: layer\nfolder: wiki\n---\n# wiki\n",
        );
        fx.write(
            "wiki/people/real-orphan.md",
            "wiki-page",
            "Real",
            "no links",
        );
        let got = orphans(&fx.store, None).unwrap();
        assert_eq!(paths(&got), vec!["wiki/people/real-orphan.md"]);
    }

    // ── frontmatter_block helper ─────────────────────────────────────────────

    #[test]
    fn frontmatter_block_extracts_between_fences() {
        let text = "---\ntype: contact\nsummary: hi\n---\nbody here\n";
        assert_eq!(
            frontmatter_block(text),
            Some("type: contact\nsummary: hi\n")
        );
    }

    #[test]
    fn frontmatter_block_none_without_leading_fence() {
        let text = "no frontmatter here\n";
        assert_eq!(frontmatter_block(text), None);
    }
}
