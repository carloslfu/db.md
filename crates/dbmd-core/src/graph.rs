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
//!   its layer (a `profile` filed under any `records/<folder>/`, not only its
//!   canonical `records/profiles/`), so the read is layer-wide, not a single
//!   canonical folder — otherwise off-canonical-folder linkers would be silently
//!   dropped.
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
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::index::IndexRecord;
use crate::store::{
    canonical_link_target, ensure_path_within_store, extract_edge_targets, fence_closes,
    fence_opens, link_edge_key, Layer, Store, StoreError,
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
///   *might* link to `path` — is read from `index.jsonl` sidecars (never a
///   content-tree walk). With a `--in <layer>` the read touches only that layer:
///   O(entities-in-layer), the sanctioned loop cost. A type-only scope (no `--in`)
///   reads store-wide sidecars and filters by `type`, exactly as
///   [`crate::query::Query::execute`] does — so a record of the type filed under a
///   non-canonical folder of its layer (a `profile` under any `records/<folder>/`)
///   *and* a **loose file** of the type filed at the *other* layer's root (a `note`
///   filed directly under `records/`, catalogued in `records/index.jsonl`) are both
///   candidates. Each candidate is then confirmed by a single-file parse.
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
    // Decode the body LOSSILY (bytes -> `from_utf8_lossy`): wiki-link syntax
    // (`[[...]]`) is ASCII, so a non-UTF8 byte elsewhere on a line cannot hide an
    // edge. This mirrors the unscoped backlink scanner
    // ([`Store::find_links_to_any`], which reads bytes + lossy by design) so
    // SCOPED backlinks (which ride `forwardlinks`) agree with unscoped backlinks
    // on a Latin-1-imported file instead of silently dropping its edges — a
    // `read_to_string` that errored on `InvalidData` returned NO edges.
    let body = match std::fs::read(&abs) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
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
/// link to the target, read from the `index.jsonl` sidecars (never a content-tree
/// walk). `types`/`layer` narrow *which* sidecars are read — the I/O scope that
/// keeps a typed/layer backlinks O(entities-in-layer) when a layer is named.
///
/// - `types` non-empty, `layer` given: read **only that layer's** sidecars
///   (O(entities-in-layer)) and keep the records whose `type` is in `types`. The
///   read is *not* short-circuited on a layer that disagrees with a type's
///   canonical layer, because a record of that type may legitimately be filed
///   there as a **loose file** (a `note` filed directly at `records/`, catalogued
///   in `records/index.jsonl`); the `type` filter on the layer read is what keeps
///   the result correct in either case.
/// - `types` non-empty, `layer` `None`: read **store-wide** sidecars and keep the
///   records whose `type` is in `types` — exactly what [`crate::query::Query::execute`]
///   does for a type-only query. This is complete across every folder *and* every
///   layer the type is filed under: its canonical-layer records (the common case)
///   plus any loose file of that type filed at the *other* layer's root.
/// - `types` empty: every sidecar record under `layer` (or store-wide when
///   `None`) via [`Store::sidecar_records`].
///
/// **Why store-wide (not the type's one canonical layer) for the type-only case.**
/// [`layer_for_type`](crate::store::layer_for_type) maps a type to exactly ONE
/// layer (`note` → Sources, `contact`
/// → Records), but a loose file (SPEC § Loose files) may legitimately be filed at
/// the *other* layer's root and catalogued in that layer's `index.jsonl`. Reading
/// only `layer_for_type(T)` would silently drop a records-loose `note` from
/// `backlinks --type note`, and early-`continue`-ing on `--in records` (because
/// `records` ≠ `layer_for_type(note)`) would return empty — diverging from the
/// unscoped scan, from `--type T --in <layer>`, and from `dbmd query --type T`.
/// Reading store-wide (or the named layer) and filtering by `type` is sidecar-backed
/// (no content-tree walk) and keeps the scoped edge set equal to the unscoped one.
/// A `type` can also span several folders within one layer — a conclusion `profile`
/// filed under any `records/<folder>/`, not only `records/profiles/` — and the
/// store-wide/layer read covers that too.
fn candidate_records(
    store: &Store,
    types: &[String],
    layer: Option<Layer>,
) -> Result<Vec<IndexRecord>, StoreError> {
    if types.is_empty() {
        return store.sidecar_records(layer);
    }
    let want: HashSet<&str> = types.iter().map(|s| s.as_str()).collect();
    // A layer scope reads only that layer's sidecars (O(entities-in-layer)); with
    // no layer, read store-wide so a loose file of the type filed at *either*
    // layer's root is covered — matching `Query::execute`'s type-only candidate
    // set. The `type` filter (not a per-type canonical-layer guess) is what makes
    // both correct, so a loose `note` under `records/` is found and a `note` under
    // `sources/` is excluded when `--in records`.
    let mut by_path: std::collections::BTreeMap<PathBuf, IndexRecord> =
        std::collections::BTreeMap::new();
    for rec in store.sidecar_records(layer)? {
        if want.contains(rec.type_.as_str()) {
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
                // Drop a neighbor that exists on disk but resolves OUTSIDE the
                // store via a symlinked path component — it is not a real in-store
                // edge, exactly as a `..` escape is dropped at edge extraction. This
                // yields no node (and no traversal through it), closing the
                // `graph neighborhood` disclosure vector at the graph boundary.
                if target_escapes_store(store, &neighbor) {
                    continue;
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

    // Every walked content file's edge KEY (NFC-folded, `.md`-stripped). A
    // wiki-link counts as a live incoming/outgoing edge when it resolves on disk
    // OR its edge key matches a walked file's. The key match is what makes a
    // cross-NORMALIZATION link a real edge on a byte-exact filesystem: an NFD
    // link to an NFC-named file (or vice versa) does NOT satisfy
    // `resolve_existing`'s `is_file` on Linux (the bytes differ), though it does
    // on macOS/APFS (which folds NFC/NFD). `link_edge_key` NFC-folds both sides,
    // so the keys agree on every platform — without this, `orphans` flagged a
    // live cross-normalization target as an orphan on Linux while macOS hid it.
    let content_keys: HashSet<String> = all
        .iter()
        .filter_map(|abs| rel_path(store, abs))
        .map(|rel| edge_key(&normalize_target(&rel)))
        .collect();

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

        // Lossy decode (see `forwardlinks`): a non-UTF8 byte must not hide a
        // `[[...]]` edge, or `orphans` would over-report BOTH endpoints of a live
        // edge as orphans (and `stats` would inflate the orphan count) on a file
        // with a stray Latin-1 byte beside a valid ASCII link line.
        let body = match std::fs::read(abs) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => return Err(StoreError::Io(e)),
        };

        let mut outgoing = false;
        for target in extract_link_targets(&body) {
            if target.is_empty() || edge_key(&target) == self_key {
                continue;
            }
            // A live edge: resolves on disk (handles raw `.eml`/`.pdf` sources and
            // store containment) OR matches a walked content file by NFC-folded
            // key (the cross-normalization case `resolve_existing` misses on a
            // byte-exact filesystem).
            if resolve_existing(store, Path::new(&target)).is_none()
                && !content_keys.contains(&edge_key(&target))
            {
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

    // Split off the leading `---`…`---` frontmatter block exactly like the read
    // side ([`Store::extract_edge_targets`] via `split_frontmatter_raw`): the
    // frontmatter is YAML, NOT markdown — it has no code fences, and a `[[…]]`
    // in any frontmatter field is a real edge. So the frontmatter region is
    // rewrite-scanned WITHOUT fence tracking, and the body is rewrite-scanned
    // with a FRESH fence state. Without this boundary reset, a stray ``` / `~~~`
    // inside a frontmatter block scalar opens a fence that persists into the
    // body, so every body `[[…]]` is treated as fenced and silently skipped —
    // leaving a dangling link after rename even though `backlinks`/`forwardlinks`
    // (which DO reset at this boundary) still report the body edge. Returns
    // byte offsets so the `---` fence lines and everything else are copied
    // byte-exact; the only mutation is a matched `[[…]]` retarget.
    let body_start = match frontmatter_body_split(text) {
        Some(body_offset) => {
            // Frontmatter prefix = `0..body_offset` (the opening `---` line, the
            // YAML, and the closing `---` line). Scan it line-by-line with
            // rewriting on and NO fence state: the literal `---` fence lines
            // never match link syntax (rewrite is a no-op on them), and any
            // real `[[…]]` in a YAML field is retargeted.
            for line in text[..body_offset].split_inclusive('\n') {
                rewrite_links_in_line(line, &old_key, &new_target, &mut out);
            }
            body_offset
        }
        // No leading frontmatter block → the whole text is body.
        None => 0,
    };

    // Body scan with a FRESH fence state. Track the fence as a `(byte, run
    // length)` exactly like validate and `extract_edge_targets` (NOT a bool
    // toggled on any ``` / ~~~ line). The naive toggle flips mid-block on a
    // nested/indented/long-run fence, so a fenced example link would be
    // rewritten — corrupting documentation and making rename disagree with
    // validate's edge notion.
    let mut fence: Option<(u8, usize)> = None;
    // `split_inclusive` keeps each line's trailing `\n`, so copying a chunk
    // verbatim preserves the original line endings exactly.
    for line in text[body_start..].split_inclusive('\n') {
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

/// Byte offset where the body begins after a leading `---`…`---` frontmatter
/// block — i.e. the first byte past the closing `---` line's `\n`. `None` when
/// the text does not open with a `---` fence or has no closing fence (the caller
/// then treats the whole text as body). Local mirror of store's
/// `split_frontmatter_raw` boundary detection (BOM- and CRLF-tolerant) — kept
/// in graph.rs so the module stays self-contained, paired with the existing
/// `frontmatter_block` mirror. Returns an offset (not slices) so
/// [`rewrite_links_to`] can copy the frontmatter and body regions byte-exact and
/// scan them with different fence policies.
fn frontmatter_body_split(text: &str) -> Option<usize> {
    // Tolerate a single leading UTF-8 BOM, matching parser/store/index/validate.
    let bom = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let after_open = if text[bom..].starts_with("---\n") {
        bom + 4
    } else if text[bom..].starts_with("---\r\n") {
        bom + 5
    } else {
        return None;
    };
    // Walk lines from just after the opening fence; the body starts right after
    // the line that is exactly `---`.
    let mut idx = after_open;
    for line in text[after_open..].split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        idx += line.len();
        if trimmed == "---" {
            return Some(idx);
        }
    }
    None
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
    if direct.is_file() && resolves_within_store(store, &direct) {
        return Some(direct);
    }
    let normalized = normalize_target(store_relative);
    let with_md = store.root.join(format!("{normalized}.md"));
    if with_md.is_file() && resolves_within_store(store, &with_md) {
        return Some(with_md);
    }
    None
}

/// True if a store-relative wiki-link target exists on disk but **resolves
/// outside the store** — i.e. some `Normal` component is a symlink redirecting to
/// an external dir/file (`records/linkdir/secret` through `records/linkdir ->
/// /external`, or a directly-symlinked `records/aliased.md -> /external/x.md`).
///
/// This is the symlink twin of the `..` escape that [`is_within_store_target`]
/// drops at edge *extraction*: a `..` target is rejected by its spelling, but a
/// symlink escape is spelled with only `Normal` components and can only be caught
/// by resolving the path. [`neighborhood_capped`] uses this to drop such a
/// neighbor from the traversal entirely, so an escaping symlink yields **no node**
/// (matching the `..` control) rather than a phantom node whose summary/type are
/// blanked — closing the `graph neighborhood` disclosure vector at the graph
/// boundary, not only at the file read.
///
/// A genuinely *dangling* in-store link (a target that exists nowhere) is **not**
/// an escape: it does not resolve on disk at all, so this returns `false` and the
/// dangling target is still surfaced as a node (existing behavior; broken-link
/// reporting is [`crate::validate`]'s job).
fn target_escapes_store(store: &Store, store_relative: &Path) -> bool {
    // Already in-store-resolvable → not an escape.
    if resolve_existing(store, store_relative).is_some() {
        return false;
    }
    // Not resolvable in-store: is it because it points OUTSIDE (a symlink escape),
    // or because it does not exist at all (a dangling link)? It escapes iff the
    // path (as written or with `.md`) exists on disk yet fails containment.
    let direct = store.root.join(store_relative);
    if direct.exists() && !resolves_within_store(store, &direct) {
        return true;
    }
    let normalized = normalize_target(store_relative);
    let with_md = store.root.join(format!("{normalized}.md"));
    with_md.exists() && !resolves_within_store(store, &with_md)
}

/// Containment check for a candidate on-disk path. Always routes through the
/// authoritative, symlink-resolving [`ensure_path_within_store`] gate — the only
/// thing that can prove an escaping or symlink-redirected path actually stays
/// inside the store.
///
/// There is deliberately **no** "all-`Normal`-components" fast path that returns
/// `true` without canonicalizing. A `Normal` component is not safe by spelling:
/// it can itself be a symlink to a directory or file outside the store
/// (`records/linkdir -> /etc`, or a directly-symlinked `records/aliased.md ->
/// ../../outside/secret.md`). `store.root.join(rel)` follows that in-store symlink,
/// `is_file()` succeeds (it follows symlinks), and without canonicalizing the
/// resolved target the out-of-store file's `summary`/`type` leak into a
/// `graph neighborhood` slice. `ensure_path_within_store` canonicalizes `abs`
/// (resolving every symlink in its chain) and confirms the result is under the
/// canonicalized root, closing that disclosure vector — the same gate the `..`
/// path already passes through.
fn resolves_within_store(store: &Store, abs: &Path) -> bool {
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
        _ => None,
    }
}

/// True if a store-relative path is a *content* file: under `sources/` or
/// `records/`, a `.md` file, and not an `index.md`. Meta files
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
/// (the ripgrep directory engine). Only the two layer roots
/// (`sources/`/`records/`) are descended, so `DB.md`, `log.md`, and
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
    // Lossy decode so a node's summary/type still resolve when the file carries
    // a stray non-UTF8 byte (consistent with the edge readers above).
    let text = match std::fs::read(&abs) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
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
            normalize_target(Path::new("./records/profiles/elena")),
            "records/profiles/elena"
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
        let body = "See [[records/profiles/sarah-chen|Sarah]] and [[records/contacts/elena.md]].";
        let got = extract_link_targets(body);
        assert_eq!(
            got,
            vec!["records/profiles/sarah-chen", "records/contacts/elena"]
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
        let input = "[[records/contacts/sarah-chen-jr]] [[sarah-chen]] [[records/concepts/x]]";
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
        fx.write("records/profiles/bio.md", "profile", "bio", body);

        // Read side: the parser sees two outgoing edges, both in canonical bare
        // form (the `.md` spelling collapsed). `sarah` is a real edge here.
        let edges = forwardlinks(&fx.store, &fx.p("records/profiles/bio.md")).unwrap();
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
        fx.write("records/profiles/bio.md", "profile", "bio", &got);
        let after = forwardlinks(&fx.store, &fx.p("records/profiles/bio.md")).unwrap();
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
        let input = "no links, [external](https://x), and [[records/concepts/y]]";
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

    #[test]
    fn rewrite_frontmatter_fence_does_not_swallow_body_link() {
        // Regression for the frontmatter/body fence-boundary data-loss bug: a
        // stray ``` inside a YAML block scalar in frontmatter used to open a code
        // fence that persisted into the body, so the rewriter treated every body
        // `[[…]]` as fenced and skipped it — leaving a dangling link after rename
        // even though `backlinks`/`forwardlinks` (which reset fence state at the
        // frontmatter boundary) still report the body edge. The write side must
        // split the frontmatter off and scan the body with a FRESH fence state,
        // exactly like the read side, so rename and the graph reads agree.
        let fx = Fixture::new();
        let text = "\
---
type: meeting
created: 2026-05-27T08:00:00-07:00
updated: 2026-05-27T08:00:00-07:00
summary: Notes
note: |
  fence with no close:
  ```
---
Met with [[records/contacts/sarah-chen]] yesterday.
";
        fx.write_raw("records/meeting.md", text);

        // Read side: despite the stray fence in frontmatter, the body edge is a
        // live forward edge (fence state resets at the frontmatter boundary).
        let edges = forwardlinks(&fx.store, &fx.p("records/meeting.md")).unwrap();
        assert_eq!(
            paths(&edges),
            vec!["records/contacts/sarah-chen"],
            "read side must report the body edge despite the frontmatter fence"
        );

        // Write side: rename must retarget that exact body edge — not skip it as
        // fenced. Output is byte-exact everywhere else (frontmatter verbatim,
        // including the stray ```).
        let got = rewrite_links_to(
            text,
            Path::new("records/contacts/sarah-chen"),
            Path::new("records/contacts/sc2"),
        );
        let expected = "\
---
type: meeting
created: 2026-05-27T08:00:00-07:00
updated: 2026-05-27T08:00:00-07:00
summary: Notes
note: |
  fence with no close:
  ```
---
Met with [[records/contacts/sc2]] yesterday.
";
        assert_eq!(
            got, expected,
            "the body link the read side reports must be rewritten; frontmatter copied verbatim"
        );

        // Cross-check through the parser: after rewrite the read side sees the new
        // target and no trace of the old — rename and the graph reads agree.
        fx.write_raw("records/meeting.md", &got);
        let after = forwardlinks(&fx.store, &fx.p("records/meeting.md")).unwrap();
        assert_eq!(
            paths(&after),
            vec!["records/contacts/sc2"],
            "after rename the read side must report only the retargeted edge"
        );
    }

    #[test]
    fn rewrite_link_genuinely_inside_a_body_fence_is_left_untouched() {
        // The boundary reset must not over-correct: a `[[…]]` truly inside a BODY
        // code fence is a documentation example, NOT an edge (matching the read
        // side), and must survive rename verbatim. This pairs with the
        // frontmatter-fence test: the body still gets a fresh, real fence state.
        let fx = Fixture::new();
        let text = "\
---
type: meeting
created: 2026-05-27T08:00:00-07:00
updated: 2026-05-27T08:00:00-07:00
summary: Notes
---
Real link: [[records/contacts/sarah-chen]].

```
Example: [[records/contacts/sarah-chen]]
```
";
        fx.write_raw("records/meeting.md", text);

        // Read side: only the unfenced body link is an edge; the fenced one is not.
        let edges = forwardlinks(&fx.store, &fx.p("records/meeting.md")).unwrap();
        assert_eq!(
            paths(&edges),
            vec!["records/contacts/sarah-chen"],
            "only the unfenced body link is a live edge"
        );

        // Write side: the real link retargets; the fenced example is byte-exact.
        let got = rewrite_links_to(
            text,
            Path::new("records/contacts/sarah-chen"),
            Path::new("records/contacts/sc2"),
        );
        let expected = "\
---
type: meeting
created: 2026-05-27T08:00:00-07:00
updated: 2026-05-27T08:00:00-07:00
summary: Notes
---
Real link: [[records/contacts/sc2]].

```
Example: [[records/contacts/sarah-chen]]
```
";
        assert_eq!(
            got, expected,
            "a link inside a body fence must survive rename; only the live edge retargets"
        );
    }

    // ── forwardlinks ─────────────────────────────────────────────────────────

    #[test]
    fn forwardlinks_returns_sorted_deduped_targets_excluding_self() {
        let fx = Fixture::new();
        fx.write(
            "records/projects/renewal.md",
            "synthesis",
            "Renewal project",
            "Links: [[records/contacts/sarah]] [[records/companies/acme]] [[records/contacts/sarah]] and itself [[records/projects/renewal]].",
        );
        // The targets need not exist on disk for forwardlinks (it reads the one
        // file only). Self-links are dropped; duplicates collapse; sorted asc.
        let got = forwardlinks(&fx.store, &fx.p("records/projects/renewal.md")).unwrap();
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
            "---\ntype: meeting\ncreated: 2026-05-01T00:00:00Z\nupdated: 2026-05-01T00:00:00Z\nsummary: Renewal sync\ncompany: [[records/companies/acme]]\nattendees:\n  - [[records/contacts/sarah]]\n  - [[records/contacts/elena]]\n---\nNotes about [[records/projects/renewal]].\n",
        );
        let got = forwardlinks(&fx.store, &fx.p("records/meetings/m1.md")).unwrap();
        assert_eq!(
            paths(&got),
            vec![
                "records/companies/acme",
                "records/contacts/elena",
                "records/contacts/sarah",
                "records/projects/renewal",
            ]
        );
    }

    #[test]
    fn forwardlinks_missing_file_is_empty_not_error() {
        let fx = Fixture::new();
        let got = forwardlinks(&fx.store, &fx.p("records/profiles/ghost.md")).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn forwardlinks_resolves_seed_given_without_md_extension() {
        let fx = Fixture::new();
        fx.write(
            "records/profiles/sarah.md",
            "profile",
            "Sarah bio",
            "Works at [[records/companies/acme]].",
        );
        // Seed passed in bare wiki-link form (no `.md`) must still resolve.
        let got = forwardlinks(&fx.store, &fx.p("records/profiles/sarah")).unwrap();
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
            "records/profiles/sarah.md",
            "profile",
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
            "records/profiles/other.md",
            "profile",
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
                "records/profiles/sarah",
                "sources/emails/e1",
            ]
        );
    }

    #[test]
    fn backlinks_and_forwardlinks_round_trip_on_same_key() {
        // If A forwardlinks to B, then B backlinks to A — both expressed in the
        // identical bare key, so neighborhood can dedup across directions.
        let fx = Fixture::new();
        fx.write(
            "records/profiles/a.md",
            "profile",
            "A",
            "Knows [[records/profiles/b]].",
        );
        fx.write("records/profiles/b.md", "profile", "B", "");
        fx.reindex();
        let fwd = forwardlinks(&fx.store, &fx.p("records/profiles/a.md")).unwrap();
        let back = backlinks(&fx.store, &fx.p("records/profiles/b.md")).unwrap();
        assert_eq!(paths(&fwd), vec!["records/profiles/b"]);
        assert_eq!(paths(&back), vec!["records/profiles/a"]);
    }

    #[test]
    fn backlinks_does_not_match_path_prefix_collisions() {
        let fx = Fixture::new();
        fx.write("records/contacts/sam.md", "contact", "Sam", "");
        // `sam-smith` shares the `sam` prefix; must NOT count as a backlink to `sam`.
        fx.write(
            "records/profiles/x.md",
            "profile",
            "x",
            "[[records/contacts/sam-smith]]",
        );
        // The genuine backlink.
        fx.write(
            "records/profiles/y.md",
            "profile",
            "y",
            "[[records/contacts/sam]]",
        );
        fx.reindex();

        let got = backlinks(&fx.store, &fx.p("records/contacts/sam")).unwrap();
        assert_eq!(paths(&got), vec!["records/profiles/y"]);
    }

    #[test]
    fn backlinks_excludes_self_reference() {
        let fx = Fixture::new();
        // A page that links to itself is not its own backlink.
        fx.write(
            "records/synthesis/overview.md",
            "synthesis",
            "Overview",
            "This page [[records/synthesis/overview]] references itself.",
        );
        fx.reindex();
        let got = backlinks(&fx.store, &fx.p("records/synthesis/overview.md")).unwrap();
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
            "records/profiles/unrelated.md",
            "profile",
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
            "records/profiles/indexed.md",
            "profile",
            "Indexed",
            "[[records/contacts/sarah]]",
        );
        fx.reindex(); // builds sidecars for sarah + the indexed linker

        // Now drop a NEW linker on disk WITHOUT reindexing — it is on disk but in
        // no sidecar.
        fx.write(
            "records/profiles/unindexed.md",
            "profile",
            "Unindexed",
            "[[records/contacts/sarah]]",
        );

        let got = backlinks(&fx.store, &fx.p("records/contacts/sarah.md")).unwrap();
        assert_eq!(
            paths(&got),
            vec!["records/profiles/indexed", "records/profiles/unindexed"],
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
            "records/profiles/indexed.md",
            "profile",
            "Indexed",
            "[[records/contacts/sarah]]",
        );
        fx.reindex(); // builds sidecars for sarah + the indexed linker

        // Drop a NEW profile linker on disk WITHOUT reindexing — on disk, in no
        // sidecar.
        fx.write(
            "records/profiles/unindexed.md",
            "profile",
            "Unindexed",
            "[[records/contacts/sarah]]",
        );

        // Scoped to the `profile` type: the candidate set is the sidecar's, so
        // only the catalogued linker is found — the unindexed one is invisible.
        let only_profiles = vec!["profile".to_string()];
        let got = backlinks_filtered(
            &fx.store,
            &fx.p("records/contacts/sarah.md"),
            &only_profiles,
            None,
        )
        .unwrap();
        assert_eq!(
            paths(&got),
            vec!["records/profiles/indexed"],
            "scoped backlinks reads the sidecar candidate set; the on-disk-but-unindexed \
             linker is not tree-walked"
        );
    }

    #[test]
    fn backlinks_filtered_type_scopes_the_candidate_set() {
        // `--type` narrows backlinks to linkers of that type. Two files link to
        // the target — one `meeting`, one `profile`; filtering to `meeting`
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
            "records/profiles/bio.md",
            "profile",
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
            "--type meeting must exclude the profile linker"
        );

        // Unfiltered, both come back — proving the filter (not the data) dropped one.
        let all = backlinks(&fx.store, &fx.p("records/contacts/sarah.md")).unwrap();
        assert_eq!(
            paths(&all),
            vec!["records/meetings/m1", "records/profiles/bio"]
        );
    }

    #[test]
    fn backlinks_filtered_layer_scopes_the_candidate_set() {
        // `--in <layer>` narrows backlinks to linkers under that layer. The two
        // linkers live in different layers (a sources email and a records
        // meeting) so the scope genuinely separates them.
        let fx = Fixture::new();
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        fx.write(
            "records/meetings/m1.md",
            "meeting",
            "Call",
            "[[records/contacts/sarah]]",
        );
        fx.write(
            "sources/emails/intro.md",
            "email",
            "Intro",
            "[[records/contacts/sarah]]",
        );
        fx.reindex();

        let got = backlinks_filtered(
            &fx.store,
            &fx.p("records/contacts/sarah.md"),
            &[],
            Some(Layer::Sources),
        )
        .unwrap();
        assert_eq!(
            paths(&got),
            vec!["sources/emails/intro"],
            "--in sources must keep only the sources-layer linker"
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
        // within one layer — a `profile` is filed under its canonical
        // `records/profiles/` folder, but an agent may also file a profile under
        // another `records/<folder>/` (the type, not the folder, is authoritative).
        // The scoped candidate set must read the whole `records/` layer and filter
        // by type, NOT just the canonical-guess folder `records/profiles/`. Before
        // the fix, `find_by_type("profile")` read ONLY `records/profiles/index.jsonl`
        // whenever that sidecar existed, silently dropping every profile linker
        // filed under any other folder — so `backlinks --type profile` under-reported
        // dependents (a wrong blast-radius check) the moment a `records/profiles/`
        // page also existed.
        //
        // The trigger needs BOTH: a populated `records/profiles/` (so its canonical
        // sidecar exists) AND a profile elsewhere in the layer that links the
        // target. The earlier
        // `backlinks_scoped_candidates_come_from_the_sidecar_not_a_tree_walk` test
        // masks this bug precisely because its fixture has no `records/profiles/`.
        let fx = Fixture::new();
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        // A profile in the CANONICAL type folder, NOT linking the target — its
        // only purpose is to make `records/profiles/index.jsonl` exist on disk.
        fx.write(
            "records/profiles/glossary.md",
            "profile",
            "Glossary",
            "No link to sarah here.",
        );
        // A profile in a NON-canonical folder that DOES link the target.
        fx.write(
            "records/people/sarah.md",
            "profile",
            "Sarah bio",
            "Profile of [[records/contacts/sarah]].",
        );
        fx.reindex(); // builds records/profiles/index.jsonl AND records/people/index.jsonl

        // Scoped to `profile`: the off-canonical linker MUST be found. Pre-fix,
        // the candidate set was only `records/profiles/`'s sidecar, so this was empty.
        let scoped = backlinks_filtered(
            &fx.store,
            &fx.p("records/contacts/sarah.md"),
            &["profile".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(
            paths(&scoped),
            vec!["records/people/sarah"],
            "a profile filed outside records/profiles/ must still be a scoped backlink"
        );

        // Cross-check: the unscoped path (ripgrep tree scan) finds the same single
        // linker, proving the scoped result is now complete — not over- or
        // under-counting — and that the data was real all along.
        let unscoped = backlinks(&fx.store, &fx.p("records/contacts/sarah.md")).unwrap();
        assert_eq!(
            paths(&unscoped),
            vec!["records/people/sarah"],
            "scoped and unscoped backlinks must agree on the edge set"
        );
    }

    #[test]
    fn backlinks_scoped_type_finds_loose_file_at_non_canonical_layer() {
        // REGRESSION (spec-conformance, SPEC § Loose files): a loose file (content
        // directly at a layer root, no type-folder) may be filed at a layer that is
        // NOT the type's canonical layer — e.g. a `note` (canonical layer
        // `sources/`) filed as `records/loose-note.md` and catalogued in
        // `records/index.jsonl`. A scoped `backlinks --type note` must still find
        // it, matching the unscoped scan and `dbmd query --type note`.
        //
        // Pre-fix, `candidate_records(--type note)` read only `layer_for_type(note)`
        // = Sources, so the records-loose note was invisible (`--type note` empty),
        // and `--type note --in records` hit the early `continue` (records ≠ the
        // note's canonical Sources layer) → also empty. Both diverged from the
        // store-wide unscoped scan. The fix reads store-wide (or the named layer)
        // sidecars and filters by `type`, never short-circuiting on the canonical
        // layer.
        let fx = Fixture::new();
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        // A loose `note` directly at the records/ layer root (no type-folder),
        // linking the target. Its canonical layer is sources/, so this exercises
        // exactly the off-canonical-layer loose-file path.
        fx.write_raw(
            "records/loose-note.md",
            "---\ntype: note\ncreated: 2026-05-01T00:00:00Z\nupdated: 2026-05-01T00:00:00Z\nsummary: Loose\n---\nMentions [[records/contacts/sarah]].\n",
        );
        fx.reindex(); // catalogs the loose note in records/index.jsonl

        let target = fx.p("records/contacts/sarah.md");
        let note_type = vec!["note".to_string()];

        // Unscoped: the loose note is a backlink (ground truth).
        let unscoped = backlinks(&fx.store, &target).unwrap();
        assert_eq!(
            paths(&unscoped),
            vec!["records/loose-note"],
            "unscoped backlinks finds the records-loose note"
        );

        // `--type note` (no layer): must agree with unscoped, NOT empty.
        let by_type = backlinks_filtered(&fx.store, &target, &note_type, None).unwrap();
        assert_eq!(
            paths(&by_type),
            vec!["records/loose-note"],
            "`--type note` must find the loose note filed at the non-canonical (records) layer"
        );

        // `--type note --in records`: the note lives in records/, so this must
        // find it too — the early `continue` on canonical-layer mismatch is gone.
        let by_type_in_records =
            backlinks_filtered(&fx.store, &target, &note_type, Some(Layer::Records)).unwrap();
        assert_eq!(
            paths(&by_type_in_records),
            vec!["records/loose-note"],
            "`--type note --in records` must find the records-loose note"
        );

        // Cross-check the same completeness via the structured query path the SPEC
        // ties graph reads to: `query --type note` (store-wide) sees the loose note,
        // proving the data was real and the scoped graph result now agrees with it.
        let q_records: Vec<String> = paths(
            &crate::query::Query::new()
                .with_type("note")
                .execute(&fx.store)
                .unwrap()
                .into_iter()
                .map(|r| r.path)
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            q_records,
            vec!["records/loose-note.md"],
            "query --type note sees the loose note store-wide; scoped backlinks must agree"
        );
    }

    // ── neighborhood ─────────────────────────────────────────────────────────

    #[test]
    fn neighborhood_hops_zero_is_empty() {
        let fx = Fixture::new();
        fx.write(
            "records/profiles/a.md",
            "profile",
            "A",
            "[[records/profiles/b]]",
        );
        fx.write("records/profiles/b.md", "profile", "B", "");
        let slice = neighborhood(
            &fx.store,
            &fx.p("records/profiles/a.md"),
            0,
            &[],
            Direction::Both,
        )
        .unwrap();
        assert_eq!(slice.seed, fx.p("records/profiles/a"));
        assert!(slice.nodes.is_empty());
    }

    #[test]
    fn neighborhood_outgoing_one_hop_reads_summary_and_type() {
        let fx = Fixture::new();
        fx.write(
            "records/profiles/a.md",
            "profile",
            "Person A",
            "Knows [[records/contacts/b]].",
        );
        fx.write("records/contacts/b.md", "contact", "Contact B summary", "");
        let slice = neighborhood(
            &fx.store,
            &fx.p("records/profiles/a.md"),
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
        assert_eq!(
            n.via,
            Some((fx.p("records/profiles/a"), Direction::Outgoing))
        );
    }

    #[test]
    fn neighborhood_incoming_only_walks_backlinks() {
        let fx = Fixture::new();
        // a -> seed (incoming to seed). seed -> c (outgoing from seed).
        fx.write(
            "records/profiles/seed.md",
            "profile",
            "Seed",
            "Out to [[records/profiles/c]].",
        );
        fx.write(
            "records/profiles/a.md",
            "profile",
            "A",
            "In to [[records/profiles/seed]].",
        );
        fx.write("records/profiles/c.md", "profile", "C", "");
        fx.reindex();
        let slice = neighborhood(
            &fx.store,
            &fx.p("records/profiles/seed.md"),
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
            vec!["records/profiles/a"]
        );
        assert_eq!(
            slice.nodes[0].via,
            Some((fx.p("records/profiles/seed"), Direction::Incoming))
        );
    }

    #[test]
    fn neighborhood_bounded_bfs_respects_hop_limit_and_min_distance() {
        let fx = Fixture::new();
        // Chain a -> b -> c -> d, all outgoing.
        fx.write("records/c/a.md", "concept", "A", "[[records/c/b]]");
        fx.write("records/c/b.md", "concept", "B", "[[records/c/c]]");
        fx.write("records/c/c.md", "concept", "C", "[[records/c/d]]");
        fx.write("records/c/d.md", "concept", "D", "");
        let slice = neighborhood(
            &fx.store,
            &fx.p("records/c/a.md"),
            2,
            &[],
            Direction::Outgoing,
        )
        .unwrap();
        // 2 hops reaches b (1) and c (2), not d (3).
        let by_path: HashMap<String, u32> = slice
            .nodes
            .iter()
            .map(|n| (n.path.to_string_lossy().to_string(), n.hops))
            .collect();
        assert_eq!(by_path.get("records/c/b").copied(), Some(1));
        assert_eq!(by_path.get("records/c/c").copied(), Some(2));
        assert_eq!(by_path.get("records/c/d"), None);
        assert_eq!(slice.nodes.len(), 2);
    }

    #[test]
    fn neighborhood_records_min_hops_on_diamond() {
        let fx = Fixture::new();
        // Diamond: a -> b, a -> c, b -> d, c -> d. d is reachable at hop 2 from
        // either branch; it must be recorded once, at hop 2.
        fx.write(
            "records/d/a.md",
            "concept",
            "A",
            "[[records/d/b]] [[records/d/c]]",
        );
        fx.write("records/d/b.md", "concept", "B", "[[records/d/d]]");
        fx.write("records/d/c.md", "concept", "C", "[[records/d/d]]");
        fx.write("records/d/d.md", "concept", "D", "");
        let slice = neighborhood(
            &fx.store,
            &fx.p("records/d/a.md"),
            3,
            &[],
            Direction::Outgoing,
        )
        .unwrap();
        let d_nodes: Vec<&ContextNode> = slice
            .nodes
            .iter()
            .filter(|n| n.path == fx.p("records/d/d"))
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
            "records/profiles/seed.md",
            "profile",
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
            &fx.p("records/profiles/seed.md"),
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
            "records/n/seed.md",
            "concept",
            "Seed",
            "[[records/n/a]] [[records/n/b]] [[records/n/c]]",
        );
        fx.write("records/n/a.md", "concept", "A", "[[records/n/deep]]");
        fx.write("records/n/b.md", "concept", "B", "");
        fx.write("records/n/c.md", "concept", "C", "");
        fx.write("records/n/deep.md", "concept", "Deep", "");

        // Uncapped over 3 hops: all four reachable nodes appear (a, b, c at hop 1,
        // deep at hop 2) — the full set the cap is measured against.
        let full = neighborhood(
            &fx.store,
            &fx.p("records/n/seed.md"),
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
            vec![
                "records/n/a",
                "records/n/b",
                "records/n/c",
                "records/n/deep"
            ],
            "uncapped traversal reaches every node within the hop budget"
        );

        // Capped at 2 over the SAME hop budget: exactly the first two hop-1 nodes,
        // and crucially NOT `deep` — the cap halted the BFS before any node was
        // expanded into hop 2, so the deep node was never traversed to.
        let capped = neighborhood_capped(
            &fx.store,
            &fx.p("records/n/seed.md"),
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
            vec!["records/n/a", "records/n/b"],
            "the cap bounds traversal: only the first 2 nodes are reached, and the \
             hop-2 `deep` node (reachable only by expanding a capped-out node) is \
             never traversed"
        );

        // `max_nodes = None` is exactly the unbounded `neighborhood` behavior.
        let uncapped = neighborhood_capped(
            &fx.store,
            &fx.p("records/n/seed.md"),
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
        fx.write("records/profiles/hub.md", "profile", "Hub", "");
        for n in ["a", "b", "c", "d", "e"] {
            fx.write(
                &format!("records/profiles/{n}.md"),
                "profile",
                n,
                "[[records/profiles/hub]]",
            );
        }
        fx.reindex();

        let capped = neighborhood_capped(
            &fx.store,
            &fx.p("records/profiles/hub.md"),
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
        let uncapped = neighborhood(
            &fx.store,
            &fx.p("records/profiles/hub.md"),
            1,
            &[],
            Direction::Both,
        )
        .unwrap();
        assert_eq!(uncapped.nodes.len(), 5);
    }

    #[test]
    fn neighborhood_cycle_terminates() {
        let fx = Fixture::new();
        // a <-> b cycle. Must not loop forever; each appears once.
        fx.write("records/g/a.md", "concept", "A", "[[records/g/b]]");
        fx.write("records/g/b.md", "concept", "B", "[[records/g/a]]");
        fx.reindex();
        let slice =
            neighborhood(&fx.store, &fx.p("records/g/a.md"), 10, &[], Direction::Both).unwrap();
        // From a: b is the only other node (a is the seed, excluded).
        assert_eq!(
            paths(
                &slice
                    .nodes
                    .iter()
                    .map(|n| n.path.clone())
                    .collect::<Vec<_>>()
            ),
            vec!["records/g/b"]
        );
    }

    // ── orphans ──────────────────────────────────────────────────────────────

    #[test]
    fn orphans_finds_files_with_no_edges_either_direction() {
        let fx = Fixture::new();
        // Wired pair: a links to b (a has outgoing, b has incoming).
        fx.write(
            "records/profiles/a.md",
            "profile",
            "A",
            "[[records/profiles/b]]",
        );
        fx.write("records/profiles/b.md", "profile", "B", "");
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
            "records/profiles/a.md",
            "profile",
            "A",
            "[[records/contacts/ghost]]",
        );
        let got = orphans(&fx.store, None).unwrap();
        assert!(
            paths(&got).contains(&"records/profiles/a.md".to_string()),
            "broken outgoing links must not wire the graph: {got:?}"
        );
    }

    #[test]
    fn orphans_file_with_only_incoming_is_not_orphan() {
        let fx = Fixture::new();
        // `target` has no outgoing links but IS linked to by `linker` — not an orphan.
        fx.write("records/contacts/target.md", "contact", "Target", "");
        fx.write(
            "records/profiles/linker.md",
            "profile",
            "Linker",
            "[[records/contacts/target]]",
        );
        let got = orphans(&fx.store, None).unwrap();
        assert!(
            !paths(&got).contains(&"records/contacts/target.md".to_string()),
            "incoming-only is not an orphan: {got:?}"
        );
        // `linker` has outgoing, so also not an orphan.
        assert!(!paths(&got).contains(&"records/profiles/linker.md".to_string()));
    }

    #[test]
    fn orphans_incoming_link_from_other_layer_unorphans() {
        let fx = Fixture::new();
        // Candidate in records/, only incoming edge comes from sources/ — a
        // cross-layer link must still un-orphan it even when scoped to records.
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "");
        fx.write(
            "sources/emails/sarah.md",
            "email",
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
        // Orphans across both layers: one source, and two records (an atomic
        // contact + a conclusion `profile`, the former wiki-page).
        fx.write("sources/emails/s.md", "email", "S", "no links");
        fx.write("records/contacts/r.md", "contact", "R", "");
        fx.write("records/profiles/w.md", "profile", "W", "");
        // The records scope keeps only the two records-layer orphans.
        let only_records = orphans(&fx.store, Some(Layer::Records)).unwrap();
        assert_eq!(
            paths(&only_records),
            vec!["records/contacts/r.md", "records/profiles/w.md"]
        );
        let only_sources = orphans(&fx.store, Some(Layer::Sources)).unwrap();
        assert_eq!(paths(&only_sources), vec!["sources/emails/s.md"]);
        // No scope: all three, sorted (records, records, sources).
        let all = orphans(&fx.store, None).unwrap();
        assert_eq!(
            paths(&all),
            vec![
                "records/contacts/r.md",
                "records/profiles/w.md",
                "sources/emails/s.md",
            ]
        );
    }

    #[test]
    fn orphans_self_link_does_not_count_as_an_edge() {
        let fx = Fixture::new();
        // A page that only links to itself has no real edges => still an orphan.
        fx.write(
            "records/synthesis/solo.md",
            "synthesis",
            "Solo",
            "I reference [[records/synthesis/solo]] only.",
        );
        let got = orphans(&fx.store, None).unwrap();
        assert_eq!(paths(&got), vec!["records/synthesis/solo.md"]);
    }

    #[test]
    fn orphans_excludes_index_and_db_files() {
        let fx = Fixture::new();
        // A lone index.md / DB.md must never be reported as an orphan content file.
        fx.write_raw(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# records\n",
        );
        fx.write(
            "records/profiles/real-orphan.md",
            "profile",
            "Real",
            "no links",
        );
        let got = orphans(&fx.store, None).unwrap();
        assert_eq!(paths(&got), vec!["records/profiles/real-orphan.md"]);
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
            "records/profiles/a.md",
            "profile",
            "A",
            "See [[ records/contacts/sarah ]] today.",
        );
        fx.reindex();

        assert_eq!(
            paths(&forwardlinks(&fx.store, Path::new("records/profiles/a.md")).unwrap()),
            vec!["records/contacts/sarah"],
            "padded link is a forward edge"
        );
        assert_eq!(
            paths(&backlinks(&fx.store, Path::new("records/contacts/sarah.md")).unwrap()),
            vec!["records/profiles/a"],
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
            "records/synthesis/howto.md",
            "synthesis",
            "Howto",
            "```markdown\n[[records/contacts/sarah]] is how you link.\n```",
        );
        fx.reindex();

        assert!(
            forwardlinks(&fx.store, Path::new("records/synthesis/howto.md"))
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
            orphan_set.contains(&"records/synthesis/howto.md".to_string()),
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
            "records/profiles/bio.md",
            "profile",
            "Bio",
            "See [[records/contacts/Sarah-Chen]].",
        );
        fx.reindex();

        assert_eq!(
            paths(&backlinks(&fx.store, Path::new("records/contacts/sarah-chen.md")).unwrap()),
            vec!["records/profiles/bio"],
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

    /// REGRESSION (Unicode encoding / silent graph break): a file whose name is
    /// written in one Unicode normalization form and an incoming link written in
    /// the OTHER form must be ONE edge — on macOS/APFS both name the same file
    /// (the FS folds NFC/NFD), so the string-keyed graph must agree. Before the
    /// fix, `link_edge_key` only case-folded (no NFC), so `backlinks` returned
    /// empty and `orphans` flagged the linked-to file as an orphan while
    /// `validate` saw the link as live. NFC-keying both sides unifies them.
    ///
    /// Runs on every platform: the file is written NFC and linked NFD (both
    /// representable in any filename), and `link_edge_key` normalizes
    /// unconditionally, so the assertion holds regardless of host FS folding.
    #[test]
    fn nfc_nfd_cross_normalization_link_is_one_edge() {
        let fx = Fixture::new();
        // File on disk: NFC `josé` (é = U+00E9).
        fx.write(
            "records/contacts/jos\u{00e9}.md",
            "contact",
            "Jose",
            "the contact",
        );
        // Incoming link: NFD `josé` (e + U+0301) — byte-different, same name.
        fx.write(
            "records/profiles/bio.md",
            "profile",
            "Bio",
            "Knows [[records/contacts/jose\u{0301}]].",
        );
        fx.reindex();

        // backlinks: the NFD link must resolve to the NFC file.
        assert_eq!(
            paths(&backlinks(&fx.store, Path::new("records/contacts/jos\u{00e9}.md")).unwrap()),
            vec!["records/profiles/bio"],
            "an NFD incoming link must be a backward edge of the NFC-named file"
        );

        // orphans: the linked-to file must NOT be flagged as an orphan.
        let orphan_set = paths(&orphans(&fx.store, None).unwrap());
        assert!(
            !orphan_set.contains(&"records/contacts/jos\u{00e9}.md".to_string()),
            "a target with a live cross-normalization incoming link must NOT be orphaned: \
             {orphan_set:?}"
        );

        // forwardlinks: the body link is a real forward edge. Its emitted target
        // is the canonical (normalization-PRESERVING) form — i.e. the NFD bytes
        // as written, NOT re-normalized to NFC — because `forwardlinks` output
        // feeds byte-faithful rewrites; only the comparison KEY is NFC-folded.
        let fwd = paths(&forwardlinks(&fx.store, &fx.p("records/profiles/bio.md")).unwrap());
        assert_eq!(
            fwd,
            vec!["records/contacts/jose\u{0301}"],
            "forwardlinks must emit the body link's canonical (NFD-preserving) target"
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
            "records/concepts/traversal.md",
            "concept",
            "Traversal",
            "See [[../outside/secret]].",
        );
        fx.reindex();

        // The escaping target is not a forward edge.
        assert!(
            forwardlinks(&fx.store, Path::new("records/concepts/traversal.md"))
                .unwrap()
                .is_empty(),
            "an escaping `[[../outside/secret]]` must not be a forward edge"
        );

        // Neighborhood from the escaping page reaches nothing through the
        // external file (the external file is never read/traversed).
        let slice = neighborhood(
            &fx.store,
            Path::new("records/concepts/traversal.md"),
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

    /// REGRESSION (path-safety / info-disclosure): a wiki-link target whose path
    /// is made entirely of `Normal` components but routes through a **symlink**
    /// pointing outside the store must NOT leak the out-of-store file's
    /// `summary`/`type` into a `neighborhood` slice. Two shapes:
    ///   (a) a symlinked DIRECTORY component (`records/linkdir -> /external/dir`,
    ///       link `[[records/linkdir/secret]]`), and
    ///   (b) a directly-symlinked `.md` (`records/aliased.md -> /external/secret.md`,
    ///       link `[[records/aliased]]`).
    /// Both used to slip past the all-`Normal`-components fast path in
    /// `resolves_within_store` (which returned `true` without canonicalizing), so
    /// `store.root.join(rel)` followed the in-store symlink, `is_file()` succeeded,
    /// and the external file was read. The fix routes every candidate through the
    /// symlink-resolving `ensure_path_within_store`, so these resolve to NO
    /// out-of-store node — exactly like the `..` escape control above. A legitimate
    /// in-store link still resolves, proving the gate did not over-block.
    #[cfg(unix)]
    #[test]
    fn symlinked_normal_component_does_not_disclose_out_of_store_file() {
        use std::os::unix::fs::symlink;

        let fx = Fixture::new();
        // The secret lives OUTSIDE the store root, as a sibling of it.
        let outside_dir = fx.store.root.parent().unwrap().join("secret");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(
            outside_dir.join("secret.md"),
            "---\ntype: contact\nsummary: TOP SECRET\n---\n# x\n",
        )
        .unwrap();

        // A legitimate in-store target, to prove the gate does not over-block.
        fx.write("records/contacts/real.md", "contact", "Real Contact", "");

        // (a) symlinked DIRECTORY component: records/linkdir -> <outside>/secret
        symlink(&outside_dir, fx.store.root.join("records/linkdir")).unwrap();
        fx.write(
            "records/contacts/seed.md",
            "contact",
            "Seed",
            "[[records/linkdir/secret]] and the in-store [[records/contacts/real]].",
        );

        // (b) directly-symlinked .md: records/aliased.md -> <outside>/secret.md
        symlink(
            outside_dir.join("secret.md"),
            fx.store.root.join("records/aliased.md"),
        )
        .unwrap();
        fx.write(
            "records/contacts/seed2.md",
            "contact",
            "Seed2",
            "[[records/aliased]]",
        );
        fx.reindex();

        // (a): the symlinked-dir target must NOT appear; the in-store link must.
        let slice = neighborhood(
            &fx.store,
            &fx.p("records/contacts/seed.md"),
            1,
            &[],
            Direction::Outgoing,
        )
        .unwrap();
        assert!(
            !slice.nodes.iter().any(|n| n.summary == "TOP SECRET"),
            "a symlinked-dir component must not disclose the out-of-store summary: {:?}",
            slice.nodes
        );
        assert!(
            !slice
                .nodes
                .iter()
                .any(|n| n.path.to_string_lossy().contains("linkdir")),
            "the symlinked-out-of-store target must not be a node: {:?}",
            slice.nodes
        );
        assert!(
            slice
                .nodes
                .iter()
                .any(|n| n.path == fx.p("records/contacts/real")),
            "the legitimate in-store link must still resolve (gate did not over-block): {:?}",
            slice.nodes
        );

        // (b): the directly-symlinked .md target must NOT disclose anything.
        let slice2 = neighborhood(
            &fx.store,
            &fx.p("records/contacts/seed2.md"),
            1,
            &[],
            Direction::Outgoing,
        )
        .unwrap();
        assert!(
            slice2.nodes.is_empty(),
            "a directly-symlinked .md pointing outside the store must yield no node: {:?}",
            slice2.nodes
        );
    }

    #[test]
    fn regression_non_utf8_linker_edges_survive_scoped_backlinks_and_orphans() {
        // Adversarial review #10: a content file with a stray non-UTF8 byte beside
        // a valid ASCII `[[...]]` line must still expose its edges. The unscoped
        // backlink scanner reads bytes lossily, but `forwardlinks`/`orphans` used
        // `read_to_string` and dropped EVERY edge on `InvalidData` — so scoped
        // backlinks under-reported vs unscoped, and `orphans` flagged BOTH
        // endpoints of a live edge.
        let fx = Fixture::new();
        fx.write("records/contacts/sarah.md", "contact", "Sarah", "# Sarah");
        // bio.md: valid UTF-8 frontmatter, but a BODY line with a 0xE9 byte
        // (Latin-1 'é', invalid as standalone UTF-8) beside the link to sarah.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(
            b"---\ntype: profile\nmeta-type: conclusion\ncreated: 2026-05-01T00:00:00Z\nupdated: 2026-05-01T00:00:00Z\nsummary: Bio\n---\n",
        );
        bytes.extend_from_slice(b"See [[records/contacts/sarah]] caf");
        bytes.push(0xE9);
        bytes.extend_from_slice(b"\n");
        let bio_abs = fx.store.root.join("records/profiles/bio.md");
        fs::create_dir_all(bio_abs.parent().unwrap()).unwrap();
        fs::write(&bio_abs, &bytes).unwrap();
        fx.reindex();

        let sarah = fx.p("records/contacts/sarah");

        // forwardlinks reads the non-UTF8 file and still finds the edge.
        let fwd = paths(&forwardlinks(&fx.store, &fx.p("records/profiles/bio")).unwrap());
        assert!(
            fwd.iter().any(|p| p.contains("sarah")),
            "forwardlinks must extract the edge from a non-UTF8 file: {fwd:?}"
        );

        // Scoped backlinks (rides `forwardlinks`) must AGREE with unscoped.
        let unscoped = paths(&backlinks(&fx.store, &sarah).unwrap());
        let scoped =
            paths(&backlinks_filtered(&fx.store, &sarah, &["profile".to_string()], None).unwrap());
        assert!(
            unscoped.iter().any(|p| p.contains("bio")),
            "unscoped backlinks must include bio: {unscoped:?}"
        );
        assert!(
            scoped.iter().any(|p| p.contains("bio")),
            "scoped backlinks must agree with unscoped on the non-UTF8 linker: {scoped:?}"
        );

        // Neither endpoint of the live edge may be reported as an orphan.
        let orph = paths(&orphans(&fx.store, None).unwrap());
        assert!(
            !orph
                .iter()
                .any(|p| p.contains("bio") || p.contains("sarah")),
            "neither endpoint of a live edge may be an orphan: {orph:?}"
        );
    }
}
