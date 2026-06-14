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
//!   relevant layer's `index.jsonl` sidecars — the sidecars of the one layer the
//!   `--type` belongs to (via [`Store::sidecar_records`]), filtered to that type
//!   — and each candidate is confirmed by a single-file parse. That is what makes
//!   `--type` / `--in` an *I/O* scope, not just a result filter: a typed/layer-scoped
//!   `backlinks` reads only the relevant layer's sidecars (O(entities-in-layer))
//!   and parses only those files. A type's records can span several folders within
//!   its layer (`wiki-page` under any `wiki/<topic>/`), so the read is layer-wide,
//!   not a single canonical folder — otherwise off-canonical-folder linkers would
//!   be silently dropped.
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

use crate::index::IndexRecord;
use crate::store::{
    canonical_link_target, ensure_path_within_store, extract_edge_targets, fence_closes,
    fence_opens, layer_for_type, link_edge_key, Layer, Store, StoreError,
};

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
///   *might* link to `path` — is read from the relevant layer's `index.jsonl`
///   sidecars, so the call touches only the named layer(s): O(entities-in-layer),
///   the sanctioned loop cost. Each candidate is then confirmed by a single-file
///   parse. When `types` lists several types, the sidecars of each type's layer
///   are read and the candidate sets unioned (filtered to the type), so a type
///   whose records span multiple folders within its layer (e.g. `wiki-page` under
///   any `wiki/<topic>/`) is fully covered; a `layer` further restricts the
///   candidate paths to that layer.
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
    let target_key = edge_key(&target);

    // Unscoped: one content pass over the store (O(store) scan with early-exit
    // per file), not a per-candidate read of every content file. `find_links_to`
    // returns every `.md` carrying an edge to the target (incl. catalog
    // `index.md`); narrow to content files and canonicalize to the bare target
    // form `backlinks` emits, dropping the seed's self-link.
    if types.is_empty() && layer.is_none() {
        let mut hits: BTreeSet<PathBuf> = BTreeSet::new();
        for rel in store.find_links_to(path)? {
            if !is_content_rel(&rel) {
                continue;
            }
            let linker = normalize_target(&rel);
            if linker.is_empty() || edge_key(&linker) == target_key {
                // A file never counts as its own backlink (case-folded so a
                // case-variant self-link is still excluded).
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
        if candidate_target.is_empty() || edge_key(&candidate_target) == target_key {
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
    let self_key = edge_key(&normalize_target(path));
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
        // Self-link drop is case-folded so a case-variant self-reference is also
        // excluded on a case-insensitive filesystem.
        if target.is_empty() || edge_key(&target) == self_key {
            continue;
        }
        out.insert(PathBuf::from(target));
    }
    Ok(out.into_iter().collect())
}

/// The candidate set for an incoming-edge scan: the sidecar records that could
/// link to the target, read from the type-folder `index.jsonl` sidecars (never
/// a content-tree walk). `types`/`layer` narrow *which* sidecars are read — the
/// I/O scope that keeps a typed/layer backlinks O(entities-in-layer).
///
/// - `types` non-empty: for each type, read **the whole layer** the type belongs
///   to ([`layer_for_type`] → [`Store::sidecar_records`]) and keep the records of
///   that `type`, unioned by path across the requested types. A `layer` filter,
///   when given, intersects with the type's own layer (a type lives in exactly
///   one layer, so a mismatched `--in` simply yields no candidates).
/// - `types` empty: every sidecar record under `layer` (or store-wide when
///   `None`) via [`Store::sidecar_records`].
///
/// **Why the whole layer, not just the type's canonical folder.** A `type` can
/// legitimately span several folders within one layer — `wiki-page` is the
/// canonical case (SPEC files it under `wiki/<topic>/` for an *arbitrary* topic:
/// `wiki/topics/`, `wiki/people/`, `wiki/projects/`, …). Reading only the
/// single canonical-guess folder (`wiki/topics/`) would silently drop every
/// wiki-page filed elsewhere in the layer, so a scoped `backlinks --type
/// wiki-page` would under-report dependents the moment that canonical folder
/// exists — breaking the docstring's promise that the scoped edge set equals the
/// unscoped one. Reading the type's full layer subtree and filtering by `type`
/// is complete and still O(entities-in-layer), the sanctioned loop scope.
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
        // A type lives in exactly one layer; read that whole layer's sidecars so
        // a record filed under a non-canonical folder of the same type (e.g. a
        // `wiki-page` under `wiki/people/` rather than `wiki/topics/`) is still a
        // candidate. An explicit `--in` layer that disagrees with the type's
        // layer can never match the type, so skip the read entirely.
        let type_layer = layer_for_type(type_);
        if let Some(scope) = layer {
            if scope != type_layer {
                continue;
            }
        }
        for rec in store.sidecar_records(Some(type_layer))? {
            if rec.type_ == *type_ {
                by_path.insert(rec.path.clone(), rec);
            }
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
    let target_key = edge_key(target);
    // Compare on the case-folded edge key so a case-variant link (e.g.
    // `[[records/contacts/Sarah-Chen]]` to `sarah-chen.md`) is confirmed on a
    // case-insensitive filesystem, agreeing with the unscoped scan and validate.
    Ok(edges
        .iter()
        .any(|e| edge_key(&e.to_string_lossy()) == target_key))
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
///
/// Unbounded traversal: delegates to [`neighborhood_capped`] with no node cap, so
/// it expands every reachable node within `hops`. For a densely-interlinked store
/// this is one full-store backlinks scan **per reached node** (O(visited × store))
/// — prefer [`neighborhood_capped`] with a `max_nodes` cap to bound that work.
pub fn neighborhood(
    store: &Store,
    seed: &Path,
    hops: u32,
    types: &[String],
    direction: Direction,
) -> Result<ContextSlice, StoreError> {
    neighborhood_capped(store, seed, hops, types, direction, None)
}

/// [`neighborhood`] with a hard cap on how many nodes the BFS **traverses**.
///
/// `max_nodes` bounds the *traversal*, not just the result: each node the BFS
/// expands triggers a per-node incoming-edge scan (an unscoped [`backlinks`] is a
/// full-store ripgrep pass), so an uncapped neighborhood of a hub node costs
/// O(visited × store). A post-hoc `.take(n)` on the returned nodes caps the
/// *output* but not that work — the scans still run for every reached node. This
/// cap stops discovering (and therefore stops scanning) once `max_nodes` distinct
/// non-seed nodes have entered the BFS, so the expensive per-node scans are bounded
/// to at most `max_nodes` of them. `None` is unbounded (the [`neighborhood`]
/// behavior).
///
/// The cap is applied at *discovery* in BFS order, so the kept nodes are exactly
/// the first `max_nodes` reached (closest-first by hop), and each still records its
/// true minimum hop distance. Type-filtered (off-type) nodes count against the cap
/// because the BFS must still traverse *through* them to reach deeper on-type
/// nodes — the scan cost is paid when a node is expanded, on- or off-type alike.
pub fn neighborhood_capped(
    store: &Store,
    seed: &Path,
    hops: u32,
    types: &[String],
    direction: Direction,
    max_nodes: Option<usize>,
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

    // Count of distinct non-seed nodes admitted to the BFS. Once it hits
    // `max_nodes` we stop discovering new nodes, which stops enqueuing them, which
    // stops the per-node full-store backlinks scan they would have triggered — the
    // cap bounds the *traversal cost*, not only the printed result.
    let mut admitted = 0usize;
    let cap_reached = |admitted: usize| max_nodes.is_some_and(|cap| admitted >= cap);

    let mut hop = 0u32;
    while hop < hops && !frontier.is_empty() && !cap_reached(admitted) {
        hop += 1;
        let level_size = frontier.len();
        for _ in 0..level_size {
            if cap_reached(admitted) {
                break;
            }
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
                if cap_reached(admitted) {
                    break;
                }
                if !discovered.insert(neighbor.clone()) {
                    continue;
                }
                admitted += 1;
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

    // `linked_to` holds case-folded edge KEYS (not raw paths): the link text may
    // spell a target with different casing than the on-disk file (e.g.
    // `[[records/contacts/Sarah-Chen]]` → `sarah-chen.md`), and on a
    // case-insensitive filesystem that is a real incoming edge. Keying on
    // `edge_key` so the incoming-edge lookup case-folds is what stops the
    // false-positive orphan (a file with a live case-variant link reported as
    // orphaned) — and matches validate, which resolves the same link via the
    // case-insensitive filesystem.
    let mut linked_to: HashSet<String> = HashSet::new();
    let mut has_outgoing: HashMap<PathBuf, bool> = HashMap::new();

    for abs in &all {
        let rel = match rel_path(store, abs) {
            Some(r) => r,
            None => continue,
        };
        let self_key = edge_key(&normalize_target(&rel));

        let body = match std::fs::read_to_string(abs) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => String::new(),
            Err(e) => return Err(StoreError::Io(e)),
        };

        let mut outgoing = false;
        for target in extract_link_targets(&body) {
            if target.is_empty() || edge_key(&target) == self_key {
                continue;
            }
            if resolve_existing(store, Path::new(&target)).is_none() {
                continue;
            }
            outgoing = true;
            linked_to.insert(edge_key(&target));
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
        let incoming = linked_to.contains(&edge_key(&normalize_target(&rel)));
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
/// Operates on the raw text (not a parser round-trip) so a link in frontmatter
/// or body is retargeted uniformly and nothing else is reflowed — **except** a
/// `[[...]]` inside a ``` fenced code block, which is a documentation example,
/// not an edge: `rename` must NOT mutate fenced verbatim content (validate
/// treats fenced links as non-edges, so rewriting them silently corrupts the
/// example and makes rename disagree with validate). Matching is fence-aware,
/// whitespace-trimmed, and case-folded to the filesystem, the exact edge notion
/// [`backlinks`]/[`forwardlinks`] use — so rename retargets precisely the edges
/// those report and nothing else.
pub fn rewrite_links_to(text: &str, old: &Path, new: &Path) -> String {
    let old_target = normalize_target(old);
    let new_target = normalize_target(new);
    if old_target.is_empty() {
        // No target to match → never rewrite anything.
        return text.to_string();
    }
    let old_key = edge_key(&old_target);

    let mut out = String::with_capacity(text.len());
    // Track the fence as a `(char, run length)` exactly like validate and
    // `extract_edge_targets` (NOT a bool toggled on any ``` / ~~~ line). The
    // naive toggle flips mid-block on a nested/indented/long-run fence, so a
    // fenced example link would be rewritten — corrupting documentation and
    // making rename disagree with validate's edge notion.
    let mut fence: Option<(u8, usize)> = None;
    // `split_inclusive` keeps each line's trailing `\n`, so copying a chunk
    // verbatim preserves the original line endings exactly.
    for line in text.split_inclusive('\n') {
        // The fence rules key on line content without trailing `\r`/`\n`; the
        // full chunk (line endings intact) is what we copy verbatim.
        let content = line.trim_end_matches('\n').trim_end_matches('\r');
        if let Some(f) = fence {
            // Inside a fenced code block: copy verbatim, never rewrite. Only a
            // matching closing fence ends the block.
            if fence_closes(content, f) {
                fence = None;
            }
            out.push_str(line);
            continue;
        }
        if let Some(opened) = fence_opens(content) {
            fence = Some(opened);
            out.push_str(line);
            continue;
        }
        rewrite_links_in_line(line, &old_key, &new_target, &mut out);
    }
    out
}

/// Rewrite every `[[...]]` on a single (non-fenced) line whose target matches
/// `old_key`, appending the result to `out`. Preserves any `|display` override
/// verbatim and emits the canonical bare `new_target`. A `[[...]]` whose target
/// does not match (a prefix sibling, the short form, an unrelated target) is
/// copied through untouched.
fn rewrite_links_in_line(line: &str, old_key: &str, new_target: &str, out: &mut String) {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut last = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if let Some(close) = line[i + 2..].find("]]") {
                let inner = &line[i + 2..i + 2 + close];
                // An embedded newline means this isn't a single-line link.
                if !inner.contains('\n') {
                    let (raw_target, display) = match inner.split_once('|') {
                        Some((t, d)) => (t, Some(d)),
                        None => (inner, None),
                    };
                    let raw_target = raw_target.trim();
                    // Match on the SAME edge key the read side uses, so `[[old]]`,
                    // `[[old.md]]`, `[[ ./old ]]`, and (case-insensitive FS)
                    // `[[Old]]` all retarget while `[[old-jr]]` never does.
                    if !raw_target.is_empty()
                        && !raw_target.starts_with('[')
                        && edge_key(&canonical_link_target(raw_target)) == old_key
                    {
                        out.push_str(&line[last..i]);
                        out.push_str("[[");
                        out.push_str(new_target);
                        if let Some(display) = display {
                            out.push('|');
                            out.push_str(display);
                        }
                        out.push_str("]]");
                        i = i + 2 + close + 2;
                        last = i;
                        continue;
                    }
                }
                // Not a matching link: skip past this `]]` so an inner `[[`
                // isn't re-scanned, but leave the text for the verbatim copy.
                i = i + 2 + close + 2;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&line[last..]);
}

// ── Private helpers ─────────────────────────────────────────────────────────

/// Normalize a store-relative path into the canonical wiki-link target form:
/// forward slashes, no leading `./` or `/`, and no trailing `.md`. This is the
/// canonical (case-PRESERVING) identity used for output and rewrites; edge
/// *comparisons* go through [`edge_key`] so the `.md`/bare forms AND (on a
/// case-insensitive filesystem) case-variant spellings of a target unify. The
/// shared [`canonical_link_target`] is the single definition every db.md
/// link op keys on.
fn normalize_target(path: &Path) -> String {
    canonical_link_target(&path.to_string_lossy())
}

/// The comparison key for an edge: the canonical target case-folded to the
/// filesystem (identity on a case-sensitive FS, lowercased on macOS/Windows), so
/// the string-keyed graph compares agree with the filesystem's case-insensitive
/// `is_file()` resolution. `[[records/contacts/Sarah-Chen]]` and the on-disk
/// `sarah-chen.md` must be the same edge on a case-insensitive filesystem or
/// backlinks/orphans/rename silently disagree with validate.
fn edge_key(canonical_target: &str) -> String {
    link_edge_key(canonical_target)
}

/// Extract every wiki-link target from a body, normalized to the canonical
/// store-relative form. Fence-aware and whitespace-trimmed via the shared
/// [`extract_edge_targets`] — a `[[...]]` inside a ``` fenced code block is a
/// documentation example, NOT an edge (matching validate), and `[[ x ]]`
/// padding resolves identically to `[[x]]`. A target that would escape the store
/// root (a `..` component) is dropped here too, so an escaping `[[../outside/x]]`
/// is never reported as a forward edge and never seeds a [`neighborhood`]
/// traversal out of the store (the disclosure vector validate flags as an
/// error). Order-preserving; duplicates kept (callers dedup).
fn extract_link_targets(body: &str) -> Vec<String> {
    extract_edge_targets(body)
        .into_iter()
        .filter(|t| is_within_store_target(t))
        .collect()
}

/// True if a canonical target stays inside the store: it has no `..`
/// (`ParentDir`) component. The canonical form has already stripped any leading
/// `./` or `/`, so a `Normal`-only path is a safe store-relative key; a `..`
/// component is an escape and is rejected, mirroring validate's safe-path guard.
fn is_within_store_target(target: &str) -> bool {
    Path::new(target)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Resolve the store root + a store-relative path to the absolute on-disk file,
/// trying the path as written and then with a `.md` extension. `None` if neither
/// exists **or if the target resolves outside the store root** — a `..`-laden or
/// symlink-escaping wiki-link must never turn a graph read/traversal into a read
/// of an arbitrary file outside the store (the `dbmd graph neighborhood`
/// disclosure vector). Containment is enforced via the shared
/// [`ensure_path_within_store`] gate, matching validate's safe-path guard.
fn resolve_existing(store: &Store, store_relative: &Path) -> Option<PathBuf> {
    let direct = store.root.join(store_relative);
    if direct.is_file() && resolves_within_store(store, store_relative, &direct) {
        return Some(direct);
    }
    let normalized = normalize_target(store_relative);
    let with_md = store.root.join(format!("{normalized}.md"));
    if with_md.is_file() && resolves_within_store(store, Path::new(&normalized), &with_md) {
        return Some(with_md);
    }
    None
}

/// Containment check for a candidate on-disk path, with a cheap fast path. A
/// store-relative path made of only `Normal` components (no `..`, no absolute /
/// platform prefix) is trivially inside the root, so the common case avoids the
/// `canonicalize` syscalls entirely. Anything with a `..`/absolute/prefix
/// component falls through to the authoritative [`ensure_path_within_store`]
/// gate (symlink-resolving), which is the only thing that can prove an escaping
/// or symlink-redirected path actually stays inside the store.
fn resolves_within_store(store: &Store, store_relative: &Path, abs: &Path) -> bool {
    let plain_relative = !store_relative.is_absolute()
        && store_relative
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
    if plain_relative {
        return true;
    }
    ensure_path_within_store(&store.root, abs).is_ok()
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
            // Follow symlinks so a symlinked `.md` content file or a symlinked
            // type folder is walked like any other content (consistent with the
            // store SWEEP walker), rather than silently vanishing from orphans.
            .follow_links(true)
            .build();
        for result in walker {
            let entry = result.map_err(|e| StoreError::Search {
                root: store.root.clone(),
                message: format!("walk failed: {e}"),
            })?;
            // A followed symlink entry reports its own type as `is_symlink()`, so
            // also accept a symlink whose target is a regular file.
            let is_file = match entry.file_type() {
                Some(ft) if ft.is_file() => true,
                Some(ft) if ft.is_symlink() => std::fs::metadata(entry.path())
                    .map(|m| m.is_file())
                    .unwrap_or(false),
                _ => false,
            };
            if !is_file {
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
    // Tolerate a single leading UTF-8 BOM, matching parser/store/index/validate.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
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

    #[test]
    fn rewrite_does_not_corrupt_links_in_nested_or_long_run_fences() {
        // Regression for the naive `starts_with("```")/("~~~")` toggle in the
        // rewriter: a fenced example documenting wiki-link syntax must be copied
        // VERBATIM, never retargeted — matching validate's edge notion. The
        // standard nested-fence convention (a ````-run block wrapping a ```
        // example) used to flip the bool mid-block, so the example link was
        // rewritten (silent documentation corruption).
        let body = "\
Here is how to write a link:

````
```
[[records/contacts/bob]]
```
still fenced [[records/contacts/bob]]
````

Real link: [[records/contacts/bob]].
";
        let got = rewrite_links_to(
            body,
            Path::new("records/contacts/bob"),
            Path::new("records/contacts/robert"),
        );
        // The two fenced examples are untouched; only the real link retargets.
        let expected = "\
Here is how to write a link:

````
```
[[records/contacts/bob]]
```
still fenced [[records/contacts/bob]]
````

Real link: [[records/contacts/robert]].
";
        assert_eq!(
            got, expected,
            "fenced example links must survive a rename verbatim; only live edges retarget"
        );
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

    #[test]
    fn backlinks_scoped_type_spans_all_topic_folders_in_its_layer() {
        // REGRESSION (finding #12): a `type` can legitimately span several folders
        // within one layer — `wiki-page` is filed under `wiki/<topic>/` for an
        // arbitrary topic (SPEC). The scoped candidate set must read the whole
        // `wiki/` layer and filter by type, NOT just the canonical-guess folder
        // `wiki/topics/`. Before the fix, `find_by_type("wiki-page")` read ONLY
        // `wiki/topics/index.jsonl` whenever that sidecar existed, silently
        // dropping every wiki-page linker filed under any other topic folder — so
        // `backlinks --type wiki-page` under-reported dependents (a wrong
        // blast-radius check) the moment a `wiki/topics/` page also existed.
        //
        // The trigger needs BOTH: a populated `wiki/topics/` (so its canonical
        // sidecar exists) AND a wiki-page elsewhere in the layer that links the
        // target. The earlier
        // `backlinks_scoped_candidates_come_from_the_sidecar_not_a_tree_walk` test
        // masks this bug precisely because its fixture has no `wiki/topics/`.
        let fx = Fixture::new();
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        // A wiki-page in the CANONICAL topic folder, NOT linking the target — its
        // only purpose is to make `wiki/topics/index.jsonl` exist on disk.
        fx.write(
            "wiki/topics/glossary.md",
            "wiki-page",
            "Glossary",
            "No link to sarah here.",
        );
        // A wiki-page in a NON-canonical topic folder that DOES link the target.
        fx.write(
            "wiki/people/sarah.md",
            "wiki-page",
            "Sarah bio",
            "Profile of [[records/contacts/sarah]].",
        );
        fx.reindex(); // builds wiki/topics/index.jsonl AND wiki/people/index.jsonl

        // Scoped to `wiki-page`: the off-canonical linker MUST be found. Pre-fix,
        // the candidate set was only `wiki/topics/`'s sidecar, so this was empty.
        let scoped = backlinks_filtered(
            &fx.store,
            &fx.p("records/contacts/sarah.md"),
            &["wiki-page".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(
            paths(&scoped),
            vec!["wiki/people/sarah"],
            "a wiki-page filed outside wiki/topics/ must still be a scoped backlink"
        );

        // Cross-check: the unscoped path (ripgrep tree scan) finds the same single
        // linker, proving the scoped result is now complete — not over- or
        // under-counting — and that the data was real all along.
        let unscoped = backlinks(&fx.store, &fx.p("records/contacts/sarah.md")).unwrap();
        assert_eq!(
            paths(&unscoped),
            vec!["wiki/people/sarah"],
            "scoped and unscoped backlinks must agree on the edge set"
        );
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
    fn neighborhood_capped_bounds_traversal_not_just_output() {
        // REGRESSION (finding #16): `neighborhood` expands every reached node, and
        // each incoming-edge expansion is a full-store scan, so the per-node cost
        // is O(visited × store). The CLI's `--limit` was applied post-hoc as a
        // `.take(n)` on the RESULT, which caps printed nodes but NOT the traversal
        // — the scans still fire for every reachable node. `neighborhood_capped`
        // bounds the traversal itself: once `max_nodes` distinct nodes are
        // admitted, the BFS stops discovering (and therefore stops scanning).
        //
        // Structure proving traversal — not just output — is bounded:
        //   seed -> a, b, c   (hop 1, discovered in sorted order: a, b, c)
        //   a    -> deep      (hop 2, reachable ONLY by expanding `a`)
        // Cap at 2: admit `a` and `b`, stop before `c` and before any hop-2
        // expansion. `deep` is therefore unreachable. A post-hoc `.take(2)` would
        // have traversed the whole graph (reaching `deep`) and only then truncated
        // — so the absence of `deep` is observable proof the traversal stopped.
        let fx = Fixture::new();
        fx.write(
            "wiki/n/seed.md",
            "wiki-page",
            "Seed",
            "[[wiki/n/a]] [[wiki/n/b]] [[wiki/n/c]]",
        );
        fx.write("wiki/n/a.md", "wiki-page", "A", "[[wiki/n/deep]]");
        fx.write("wiki/n/b.md", "wiki-page", "B", "");
        fx.write("wiki/n/c.md", "wiki-page", "C", "");
        fx.write("wiki/n/deep.md", "wiki-page", "Deep", "");

        // Uncapped over 3 hops: all four reachable nodes appear (a, b, c at hop 1,
        // deep at hop 2) — the full set the cap is measured against.
        let full = neighborhood(
            &fx.store,
            &fx.p("wiki/n/seed.md"),
            3,
            &[],
            Direction::Outgoing,
        )
        .unwrap();
        assert_eq!(
            paths(
                &full
                    .nodes
                    .iter()
                    .map(|n| n.path.clone())
                    .collect::<Vec<_>>()
            ),
            vec!["wiki/n/a", "wiki/n/b", "wiki/n/c", "wiki/n/deep"],
            "uncapped traversal reaches every node within the hop budget"
        );

        // Capped at 2 over the SAME hop budget: exactly the first two hop-1 nodes,
        // and crucially NOT `deep` — the cap halted the BFS before any node was
        // expanded into hop 2, so the deep node was never traversed to.
        let capped = neighborhood_capped(
            &fx.store,
            &fx.p("wiki/n/seed.md"),
            3,
            &[],
            Direction::Outgoing,
            Some(2),
        )
        .unwrap();
        assert_eq!(
            paths(
                &capped
                    .nodes
                    .iter()
                    .map(|n| n.path.clone())
                    .collect::<Vec<_>>()
            ),
            vec!["wiki/n/a", "wiki/n/b"],
            "the cap bounds traversal: only the first 2 nodes are reached, and the \
             hop-2 `deep` node (reachable only by expanding a capped-out node) is \
             never traversed"
        );

        // `max_nodes = None` is exactly the unbounded `neighborhood` behavior.
        let uncapped = neighborhood_capped(
            &fx.store,
            &fx.p("wiki/n/seed.md"),
            3,
            &[],
            Direction::Outgoing,
            None,
        )
        .unwrap();
        assert_eq!(
            uncapped.nodes.len(),
            full.nodes.len(),
            "None cap matches the unbounded neighborhood result"
        );
    }

    #[test]
    fn neighborhood_capped_both_direction_caps_the_node_count() {
        // The CLI always passes `Direction::Both` (the per-node backlinks scan is
        // the expensive path the cap exists to bound). The cap gates discovery in
        // any direction, so a hub linked from many nodes is still bounded.
        let fx = Fixture::new();
        fx.write("wiki/h/hub.md", "wiki-page", "Hub", "");
        for n in ["a", "b", "c", "d", "e"] {
            fx.write(&format!("wiki/h/{n}.md"), "wiki-page", n, "[[wiki/h/hub]]");
        }
        fx.reindex();

        let capped = neighborhood_capped(
            &fx.store,
            &fx.p("wiki/h/hub.md"),
            1,
            &[],
            Direction::Both,
            Some(3),
        )
        .unwrap();
        assert_eq!(
            capped.nodes.len(),
            3,
            "Both-direction neighborhood is bounded to the node cap"
        );

        // Without the cap the same call returns all five backlinking nodes,
        // proving the cap (not the data) limited the set.
        let uncapped =
            neighborhood(&fx.store, &fx.p("wiki/h/hub.md"), 1, &[], Direction::Both).unwrap();
        assert_eq!(uncapped.nodes.len(), 5);
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
    fn orphans_file_with_only_broken_outgoing_link_is_orphan() {
        let fx = Fixture::new();
        // Broken targets are validation issues, not graph edges to another
        // store file. A file whose only link points nowhere is still an orphan.
        fx.write(
            "wiki/people/a.md",
            "wiki-page",
            "A",
            "[[records/contacts/ghost]]",
        );
        let got = orphans(&fx.store, None).unwrap();
        assert!(
            paths(&got).contains(&"wiki/people/a.md".to_string()),
            "broken outgoing links must not wire the graph: {got:?}"
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

    #[test]
    fn frontmatter_block_tolerates_leading_bom() {
        // Regression (finding #19 cross-module): a UTF-8 BOM before the opening
        // fence must not hide the frontmatter from the graph layer — otherwise a
        // BOM-prefixed file the catalog indexes contributes no backlinks/edges.
        // Pre-fix the `---\n` strip failed on the BOM and returned None.
        let text = "\u{feff}---\ntype: contact\nsummary: hi\n---\nbody here\n";
        assert_eq!(
            frontmatter_block(text),
            Some("type: contact\nsummary: hi\n"),
            "a leading BOM must not hide frontmatter from the graph layer"
        );
    }

    // ── shared edge notion: whitespace / fence / case / containment ──────────

    /// Padded `[[ x ]]` must be a forward edge AND (after reindex) a backward
    /// edge — the two views agreeing on the same edge in a clean store.
    #[test]
    fn padded_link_is_both_a_forward_and_backward_edge() {
        let fx = Fixture::new();
        fx.write(
            "records/contacts/sarah.md",
            "contact",
            "Sarah",
            "the contact",
        );
        fx.write(
            "wiki/people/a.md",
            "wiki-page",
            "A",
            "See [[ records/contacts/sarah ]] today.",
        );
        fx.reindex();

        assert_eq!(
            paths(&forwardlinks(&fx.store, Path::new("wiki/people/a.md")).unwrap()),
            vec!["records/contacts/sarah"],
            "padded link is a forward edge"
        );
        assert_eq!(
            paths(&backlinks(&fx.store, Path::new("records/contacts/sarah.md")).unwrap()),
            vec!["wiki/people/a"],
            "padded link is the SAME backward edge (forward and backward agree)"
        );
    }

    /// A `[[...]]` only inside a fenced code block is a documentation example,
    /// not an edge: no forward edge, no backward edge, and the source page is an
    /// orphan (no real links). Matches validate's fence-aware extractor.
    #[test]
    fn fenced_link_is_not_an_edge_and_page_is_orphan() {
        let fx = Fixture::new();
        fx.write(
            "records/contacts/sarah.md",
            "contact",
            "Sarah",
            "the contact",
        );
        fx.write(
            "wiki/topics/howto.md",
            "wiki-page",
            "Howto",
            "```markdown\n[[records/contacts/sarah]] is how you link.\n```",
        );
        fx.reindex();

        assert!(
            forwardlinks(&fx.store, Path::new("wiki/topics/howto.md"))
                .unwrap()
                .is_empty(),
            "a fenced example is not a forward edge"
        );
        assert!(
            backlinks(&fx.store, Path::new("records/contacts/sarah.md"))
                .unwrap()
                .is_empty(),
            "a fenced example is not a backward edge"
        );
        let orphan_set = paths(&orphans(&fx.store, None).unwrap());
        assert!(
            orphan_set.contains(&"wiki/topics/howto.md".to_string()),
            "a page whose only link is fenced has no real edges => orphan: {orphan_set:?}"
        );
    }

    /// `rename` must NOT rewrite a `[[...]]` inside a fenced code block (it is
    /// verbatim documentation, not an edge), while still rewriting a real link.
    #[test]
    fn rewrite_links_to_leaves_fenced_examples_untouched() {
        let input = "\
Real [[records/contacts/sarah]] link.

```markdown
Example: [[records/contacts/sarah]] inside a fence.
```

Trailing [[records/contacts/sarah]].
";
        let got = rewrite_links_to(
            input,
            Path::new("records/contacts/sarah"),
            Path::new("records/contacts/sarah-chen"),
        );
        // The two non-fenced links retarget; the fenced one is verbatim.
        assert!(
            got.contains("Real [[records/contacts/sarah-chen]] link."),
            "real link before the fence must retarget"
        );
        assert!(
            got.contains("Trailing [[records/contacts/sarah-chen]]."),
            "real link after the fence must retarget"
        );
        assert!(
            got.contains("Example: [[records/contacts/sarah]] inside a fence."),
            "fenced example must stay verbatim, got:\n{got}"
        );
    }

    /// `rewrite_links_to` matches a padded link and preserves the display.
    #[test]
    fn rewrite_links_to_matches_padded_link() {
        let got = rewrite_links_to(
            "See [[ records/contacts/sarah |Sarah]] today.",
            Path::new("records/contacts/sarah"),
            Path::new("records/contacts/sarah-chen"),
        );
        assert_eq!(got, "See [[records/contacts/sarah-chen|Sarah]] today.");
    }

    /// On a case-insensitive filesystem a case-variant link is the same edge:
    /// backlinks finds it, orphans does NOT falsely orphan the target, and
    /// rename rewrites it. On a case-sensitive FS the link is genuinely a
    /// different target, so the test is skipped.
    #[cfg(unix)]
    #[test]
    fn case_variant_link_is_one_edge_on_case_insensitive_fs() {
        // Probe the filesystem the same way the production code does
        // (`link_edge_key` is imported at module scope).
        if link_edge_key("A") != link_edge_key("a") {
            // case-sensitive filesystem: the case-variant link is a different
            // target, so this scenario doesn't apply.
            return;
        }
        let fx = Fixture::new();
        fx.write(
            "records/contacts/sarah-chen.md",
            "contact",
            "Sarah",
            "the contact",
        );
        fx.write(
            "wiki/people/bio.md",
            "wiki-page",
            "Bio",
            "See [[records/contacts/Sarah-Chen]].",
        );
        fx.reindex();

        assert_eq!(
            paths(&backlinks(&fx.store, Path::new("records/contacts/sarah-chen.md")).unwrap()),
            vec!["wiki/people/bio"],
            "case-variant incoming link must be a backward edge"
        );
        let orphan_set = paths(&orphans(&fx.store, None).unwrap());
        assert!(
            !orphan_set.contains(&"records/contacts/sarah-chen.md".to_string()),
            "a target with a live case-variant incoming link must NOT be orphaned: {orphan_set:?}"
        );

        let rewritten = rewrite_links_to(
            "See [[records/contacts/Sarah-Chen]].",
            Path::new("records/contacts/sarah-chen"),
            Path::new("records/contacts/sarah"),
        );
        assert_eq!(
            rewritten, "See [[records/contacts/sarah]].",
            "rename must rewrite the case-variant link on a case-insensitive FS"
        );
    }

    /// A `[[../outside/x]]` escaping wiki-link is never a forward edge, and a
    /// `neighborhood` from the escaping page never reads or traverses through the
    /// external file — closing the disclosure vector.
    #[cfg(unix)]
    #[test]
    fn escaping_link_is_not_an_edge_and_neighborhood_does_not_escape() {
        let fx = Fixture::new();
        // An external file OUTSIDE the store root, with its own in-store link.
        let outside_dir = fx.store.root.parent().unwrap().join("outside");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(
            outside_dir.join("secret.md"),
            "---\ntype: note\nsummary: TOPSECRET\n---\nLinks [[records/contacts/sarah]].\n",
        )
        .unwrap();
        fx.write(
            "records/contacts/sarah.md",
            "contact",
            "Sarah",
            "the contact",
        );
        fx.write(
            "wiki/topics/traversal.md",
            "wiki-page",
            "Traversal",
            "See [[../outside/secret]].",
        );
        fx.reindex();

        // The escaping target is not a forward edge.
        assert!(
            forwardlinks(&fx.store, Path::new("wiki/topics/traversal.md"))
                .unwrap()
                .is_empty(),
            "an escaping `[[../outside/secret]]` must not be a forward edge"
        );

        // Neighborhood from the escaping page reaches nothing through the
        // external file (the external file is never read/traversed).
        let slice = neighborhood(
            &fx.store,
            Path::new("wiki/topics/traversal.md"),
            2,
            &[],
            Direction::Outgoing,
        )
        .unwrap();
        assert!(
            slice
                .nodes
                .iter()
                .all(|n| !n.path.to_string_lossy().contains("outside")),
            "neighborhood must not read/traverse the external file: {:?}",
            slice.nodes
        );
    }
}
