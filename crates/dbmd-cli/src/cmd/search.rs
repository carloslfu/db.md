//! `dbmd search <query>` — embedded ripgrep over a sidecar-resolved candidate set.
//!
//! The split this command exists to enforce (plan Block 4 / SPEC § Tooling):
//!
//! - **Structured filters** (`--type` / `--where`, plus the four `--updated-*` /
//!   `--created-*` windows) resolve the *candidate set* through `dbmd_core::query`
//!   against the type-folder `index.jsonl` sidecars — a complete, sequential,
//!   cold-cache-proof read, **never** a walk-and-parse. The time windows narrow
//!   that set off the in-memory sidecar records (no extra I/O).
//! - **Link filters** (`--linked-from` / `--linked-to`) resolve through
//!   `dbmd_core::graph` (`forwardlinks` / `backlinks`, embedded ripgrep over
//!   wiki-links).
//! - **`--in <layer>`** scopes the final candidate set to one layer. On the
//!   structured path it rides along as `Query::with_layer`; on the link /
//!   all-content paths it is a path-prefix filter applied to the resolved set.
//! - The **free-text query** is then an embedded-ripgrep scan over **only the
//!   bodies of that candidate set** — the one place this crate runs `grep`
//!   directly (it is the search engine; there is no `dbmd-core` primitive for an
//!   arbitrary-regex file scan, and shelling out to `rg` is forbidden).
//!
//! When no structured/link filter is given, the candidate set is every content
//! file under `sources/` / `records/` / `wiki/` — a path-only `ignore` walk
//! (the same engine `rg` uses), not a frontmatter parse. Meta files (`DB.md`,
//! `index.md`, `index.jsonl`, `log.md`) are never content and never match.
//!
//! Output is `file:line:text` (`rg`-compatible) by default, or a structured
//! array under `--json`. The handler stays a thin orchestrator: it parses args,
//! asks `dbmd-core` for the candidate set, runs the scan, and formats — the
//! filter logic itself lives in `dbmd-core`.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use grep::regex::RegexMatcher;
use grep::searcher::sinks::UTF8;
use grep::searcher::{BinaryDetection, Searcher, SearcherBuilder};
use ignore::WalkBuilder;

use dbmd_core::{Layer, Query, Store};

use crate::cli::SearchArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// One matching line, in `rg`-compatible shape. `Serialize` drives `--json`
/// output; `Deserialize` lets the integration tests parse that output back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Match {
    /// Store-relative path of the file the match is in.
    file: String,
    /// 1-based line number of the match.
    line: u64,
    /// The full matching line, trailing newline trimmed.
    text: String,
}

/// Run `dbmd search`: open the store, collect the matches, and emit them.
///
/// The thin top: `Store::open` + [`collect_matches`] (the testable core that
/// resolves the candidate set and scans it) + [`emit`] (the formatter). All
/// three pieces are split out so the core is exercisable in-process against the
/// corpus without spawning the binary or capturing stdout.
pub fn run(ctx: &Context, args: &SearchArgs) -> CliResult {
    let store = Store::open(Path::new(&args.dir)).map_err(dbmd_core::Error::from)?;
    let matches = collect_matches(&store, args)?;
    emit(ctx, &matches)
}

/// The search core, decoupled from store-opening and output: parse the layer +
/// the query regex, resolve the candidate set from `dbmd-core` (sidecar filters
/// ∩ link filters, or every content file when neither is set), narrow it by the
/// time windows, then scan that set with embedded ripgrep — returning the
/// `rg`-shaped [`Match`]es (file order ascending, capped at `--limit`).
fn collect_matches(store: &Store, args: &SearchArgs) -> Result<Vec<Match>, CliError> {
    // The free-text query is a ripgrep regex (case-sensitive by default, the
    // `rg` default). Compile it once; an invalid pattern is a usage-class
    // runtime error, not a panic.
    let matcher = RegexMatcher::new(&args.query).map_err(|e| {
        CliError::new(
            ExitCode::Runtime,
            "BAD_QUERY_REGEX",
            format!("invalid search pattern `{}`: {e}", args.query),
        )
        .with_hint("the query is a ripgrep regex; escape regex metacharacters to search literally")
    })?;

    let layer = parse_layer(args.r#in.as_deref())?;
    let windows = TimeWindows::from_args(args)?;

    // ── Candidate-set resolution (all via dbmd-core) ─────────────────────────
    let candidates = resolve_candidates(store, args, layer, &windows)?;

    // ── Free-text scan over only the candidate set's bodies ──────────────────
    let mut matches: Vec<Match> = Vec::new();
    let mut searcher = build_searcher();
    'outer: for rel in &candidates {
        let abs = store.abs_path(rel);
        let rel_str = path_to_str(rel);
        let scan = searcher.search_path(
            &matcher,
            &abs,
            UTF8(|lineno, line| {
                matches.push(Match {
                    file: rel_str.clone(),
                    line: lineno,
                    text: line.trim_end_matches(['\n', '\r']).to_string(),
                });
                Ok(true)
            }),
        );
        if let Err(e) = scan {
            return Err(CliError::new(
                ExitCode::Runtime,
                "SEARCH_FAILED",
                format!("ripgrep scan of {} failed: {e}", abs.display()),
            ));
        }
        if let Some(limit) = args.limit {
            if matches.len() >= limit {
                matches.truncate(limit);
                break 'outer;
            }
        }
    }

    Ok(matches)
}

/// Resolve the candidate set the free-text scan runs over, entirely through
/// `dbmd-core`:
///
/// 1. **Structured** (`--type` / `--where`): `query::Query` over the sidecars.
/// 2. **Links** (`--linked-from` / `--linked-to`): `graph::forwardlinks` /
///    `graph::backlinks`.
/// 3. The active sets from (1)+(2) are **intersected** (every filter is AND).
/// 4. If neither is active, the candidate set is **every content file** (a
///    path-only walk — not a parse).
/// 5. The time windows then narrow whatever survived.
///
/// Every entry is returned as a real on-disk store-relative `.md` path, deduped
/// and sorted, so the scan output is deterministic.
fn resolve_candidates(
    store: &Store,
    args: &SearchArgs,
    layer: Option<Layer>,
    windows: &TimeWindows,
) -> Result<Vec<PathBuf>, CliError> {
    let structured = structured_candidates(store, args, layer, windows)?;
    let linked = link_candidates(store, args)?;

    let mut set: Option<BTreeSet<PathBuf>> = None;
    if let Some(s) = structured {
        set = Some(intersect(set, s));
    }
    if let Some(l) = linked {
        set = Some(intersect(set, l));
    }

    let mut resolved: Vec<PathBuf> = match set {
        // No structured filter, no link filter: scan every content file. This
        // is the `ignore` walker (paths only), not a frontmatter parse.
        None => content_files(store)?,
        Some(s) => s.into_iter().collect(),
    };

    // `--in <layer>` scopes every path. The structured path already applied it
    // via `Query::with_layer`, so re-filtering there is a no-op; the link and
    // all-content paths are NOT layer-scoped at their source, so this is where
    // their scope is enforced. Applying it uniformly here keeps the three
    // candidate sources composing correctly with `--in`.
    if let Some(l) = layer {
        resolved.retain(|rel| path_in_layer(rel, l));
    }

    // Time windows narrow whatever candidate set we ended up with. When the set
    // came from the sidecar, structured_candidates already applied them off the
    // in-memory IndexRecords (no extra I/O); for the link / all-content paths we
    // read the candidate files' timestamps here — only over the candidates, so
    // still O(candidates), never a whole-store parse.
    if windows.is_active() && args.r#type.is_none() && args.r#where.is_empty() {
        resolved.retain(|rel| windows.matches(read_file_timestamps(&store.abs_path(rel))));
    }

    Ok(resolved)
}

/// The sidecar-backed candidate set for `--type` / `--where`, with the time
/// windows applied over the returned [`IndexRecord`] timestamps (free — the
/// records are already in memory). Returns `None` when neither `--type` nor
/// `--where` is set (so the caller knows this filter is inactive); `Some(empty)`
/// is a real "nothing matched".
fn structured_candidates(
    store: &Store,
    args: &SearchArgs,
    layer: Option<Layer>,
    windows: &TimeWindows,
) -> Result<Option<BTreeSet<PathBuf>>, CliError> {
    if args.r#type.is_none() && args.r#where.is_empty() {
        return Ok(None);
    }

    let mut query = Query::new();
    if let Some(t) = &args.r#type {
        query = query.with_type(t);
    }
    if let Some(l) = layer {
        query = query.with_layer(l);
    }
    for clause in &args.r#where {
        let (key, value) = split_kv(clause)?;
        query = query.with_where(key, value);
    }

    let records = query.execute(store).map_err(dbmd_core::Error::from)?;
    let set = records
        .into_iter()
        .filter(|r| windows.matches((r.created, r.updated)))
        .map(|r| r.path)
        .collect();
    Ok(Some(set))
}

/// The graph-backed candidate set for `--linked-from` / `--linked-to`, via
/// `dbmd_core::graph`. Both flags AND together when both are given. Returns
/// `None` when neither is set.
///
/// `forwardlinks` / `backlinks` yield **bare** (no-`.md`) canonical paths; we
/// resolve each back to its real on-disk `.md` file (dropping any link target
/// that has no file on disk — a dangling edge is not a searchable body), and
/// apply the `--in` / `--type` scope so the link filter composes with them.
fn link_candidates(
    store: &Store,
    args: &SearchArgs,
) -> Result<Option<BTreeSet<PathBuf>>, CliError> {
    let mut set: Option<BTreeSet<PathBuf>> = None;

    if let Some(from) = &args.linked_from {
        let targets = dbmd_core::graph::forwardlinks(store, Path::new(from))
            .map_err(dbmd_core::Error::from)?;
        set = Some(intersect(set, resolve_link_targets(store, &targets)));
    }
    if let Some(to) = &args.linked_to {
        let linkers =
            dbmd_core::graph::backlinks(store, Path::new(to)).map_err(dbmd_core::Error::from)?;
        set = Some(intersect(set, resolve_link_targets(store, &linkers)));
    }

    Ok(set)
}

/// Resolve a list of bare wiki-link targets to the set of real `.md` files they
/// name (skipping any with no file on disk).
fn resolve_link_targets(store: &Store, targets: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    for target in targets {
        if let Some(rel) = resolve_content_file(store, target) {
            out.insert(rel);
        }
    }
    out
}

/// Every content `.md` file under the three layers, as store-relative paths.
///
/// The no-filter candidate set. Uses the `ignore` walker (the ripgrep directory
/// engine) over the layer roots only, so root meta files (`DB.md`, `log.md`,
/// `log/`) are structurally out of reach; per-folder `index.md` sidecars and
/// the `index.jsonl` twins are filtered out. A path-only walk — never a
/// frontmatter parse of the store.
fn content_files(store: &Store) -> Result<Vec<PathBuf>, CliError> {
    let mut out = BTreeSet::new();
    for layer in Layer::all() {
        let dir = store.root.join(layer.dir_name());
        if !dir.is_dir() {
            continue;
        }
        let walker = WalkBuilder::new(&dir)
            .hidden(true)
            .git_ignore(false)
            .git_global(false)
            .require_git(false)
            .build();
        for entry in walker {
            let entry = entry.map_err(|e| {
                CliError::new(
                    ExitCode::Runtime,
                    "SEARCH_FAILED",
                    format!("walk failed under {}: {e}", dir.display()),
                )
            })?;
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let abs = entry.into_path();
            if let Some(rel) = store.rel_path(&abs) {
                if is_content_file(&rel) {
                    out.insert(rel);
                }
            }
        }
    }
    Ok(out.into_iter().collect())
}

// ── Time windows ─────────────────────────────────────────────────────────────

/// The four optional `created`/`updated` bounds, parsed once. All are inclusive
/// ("at or after" / "at or before", per the CLI help). A date-only value is
/// treated as `T00:00:00Z`, matching `dbmd log since`.
struct TimeWindows {
    updated_after: Option<chrono::DateTime<chrono::FixedOffset>>,
    updated_before: Option<chrono::DateTime<chrono::FixedOffset>>,
    created_after: Option<chrono::DateTime<chrono::FixedOffset>>,
    created_before: Option<chrono::DateTime<chrono::FixedOffset>>,
}

impl TimeWindows {
    fn from_args(args: &SearchArgs) -> Result<Self, CliError> {
        Ok(Self {
            updated_after: parse_ts_opt(args.updated_after.as_deref(), "--updated-after")?,
            updated_before: parse_ts_opt(args.updated_before.as_deref(), "--updated-before")?,
            created_after: parse_ts_opt(args.created_after.as_deref(), "--created-after")?,
            created_before: parse_ts_opt(args.created_before.as_deref(), "--created-before")?,
        })
    }

    fn is_active(&self) -> bool {
        self.updated_after.is_some()
            || self.updated_before.is_some()
            || self.created_after.is_some()
            || self.created_before.is_some()
    }

    /// True if a record's `(created, updated)` instants satisfy every set bound.
    /// A bound against an absent timestamp fails (you cannot prove a missing
    /// `updated` is "after" a cutoff), so a window filter excludes undated files.
    fn matches(
        &self,
        ts: (
            Option<chrono::DateTime<chrono::FixedOffset>>,
            Option<chrono::DateTime<chrono::FixedOffset>>,
        ),
    ) -> bool {
        let (created, updated) = ts;
        if let Some(bound) = self.updated_after {
            match updated {
                Some(u) if u >= bound => {}
                _ => return false,
            }
        }
        if let Some(bound) = self.updated_before {
            match updated {
                Some(u) if u <= bound => {}
                _ => return false,
            }
        }
        if let Some(bound) = self.created_after {
            match created {
                Some(c) if c >= bound => {}
                _ => return false,
            }
        }
        if let Some(bound) = self.created_before {
            match created {
                Some(c) if c <= bound => {}
                _ => return false,
            }
        }
        true
    }
}

// ── Output ───────────────────────────────────────────────────────────────────

/// Emit the matches: an `rg`-compatible `file:line:text` line each (text mode),
/// or a JSON array of `{file, line, text}` objects (`--json`). No matches is a
/// success with empty output (exit 0), the `rg`-on-`--json`/agent-friendly
/// contract — a "not found" is data, not an error.
fn emit(ctx: &Context, matches: &[Match]) -> CliResult {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if ctx.json {
        let rendered = serde_json::to_string_pretty(matches)
            .map_err(|e| CliError::new(ExitCode::Runtime, "JSON_ENCODE_FAILED", e.to_string()))?;
        writeln!(out, "{rendered}")?;
    } else {
        for m in matches {
            writeln!(out, "{}:{}:{}", m.file, m.line, m.text)?;
        }
    }
    Ok(())
}

// ── Small helpers ─────────────────────────────────────────────────────────────

/// Parse a `--in <layer>` value into a [`Layer`]. `None` in → `None` out (no
/// scope); an unrecognized layer is a usage-class error naming the valid set.
fn parse_layer(value: Option<&str>) -> Result<Option<Layer>, CliError> {
    match value {
        None => Ok(None),
        Some(name) => Layer::from_dir_name(name).map(Some).ok_or_else(|| {
            CliError::new(
                ExitCode::Runtime,
                "BAD_LAYER",
                format!("unknown layer `{name}`"),
            )
            .with_hint("valid layers are: sources, records, wiki")
        }),
    }
}

/// Split a `key=value` clause; an entry with no `=` is a usage-class error.
fn split_kv(clause: &str) -> Result<(&str, &str), CliError> {
    clause.split_once('=').ok_or_else(|| {
        CliError::new(
            ExitCode::Runtime,
            "BAD_WHERE",
            format!("expected key=value, got `{clause}`"),
        )
        .with_hint("pass --where as key=value, e.g. --where status=active")
    })
}

/// Parse an optional RFC3339 timestamp, accepting a bare date (`2026-05-15`) as
/// midnight UTC. An unparseable value is a usage-class error naming the flag.
fn parse_ts_opt(
    value: Option<&str>,
    flag: &str,
) -> Result<Option<chrono::DateTime<chrono::FixedOffset>>, CliError> {
    match value {
        None => Ok(None),
        Some(raw) => parse_ts(raw)
            .map(Some)
            .ok_or_else(|| bad_timestamp(flag, raw)),
    }
}

/// Parse a timestamp as RFC3339, or a bare `YYYY-MM-DD` date as `T00:00:00Z`.
fn parse_ts(raw: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let raw = raw.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt);
    }
    // Date-only → midnight UTC, the same lenience `dbmd log since` documents.
    let midnight = format!("{raw}T00:00:00Z");
    chrono::DateTime::parse_from_rfc3339(&midnight).ok()
}

fn bad_timestamp(flag: &str, raw: &str) -> CliError {
    CliError::new(
        ExitCode::Runtime,
        "BAD_TIMESTAMP",
        format!("{flag} expects an RFC3339 timestamp, got `{raw}`"),
    )
    .with_hint("use e.g. 2026-05-15 or 2026-05-15T09:00:00Z")
}

/// Read just the `created` / `updated` frontmatter timestamps of one file.
///
/// Used only for the link / all-content candidate paths when a time window is
/// active — and only over the already-narrowed candidate set, so it never
/// becomes a whole-store parse. A missing file / frontmatter / field yields
/// `None`, which a window filter then excludes.
fn read_file_timestamps(
    abs: &Path,
) -> (
    Option<chrono::DateTime<chrono::FixedOffset>>,
    Option<chrono::DateTime<chrono::FixedOffset>>,
) {
    let text = match std::fs::read_to_string(abs) {
        Ok(t) => t,
        Err(_) => return (None, None),
    };
    let yaml = match frontmatter_block(&text) {
        Some(y) => y,
        None => return (None, None),
    };
    let value: serde_yml::Value = match serde_yml::from_str(yaml) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let read = |key: &str| {
        value
            .get(key)
            .and_then(yaml_scalar_string)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
    };
    (read("created"), read("updated"))
}

/// The YAML between a leading `---` fence and the next `---`, or `None` when the
/// file does not open with a fence. Local mirror of the parser's split so the
/// handler stays self-contained (matches the same helper in `dbmd-core`).
fn frontmatter_block(text: &str) -> Option<&str> {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = body
        .strip_prefix("---\n")
        .or_else(|| body.strip_prefix("---\r\n"))?;
    let mut idx = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some(&rest[..idx]);
        }
        idx += line.len();
    }
    None
}

/// Render a YAML scalar as a string (a real string verbatim; otherwise its
/// compact serialization, covering YAML-native timestamps).
fn yaml_scalar_string(value: &serde_yml::Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    match value {
        serde_yml::Value::Null | serde_yml::Value::Mapping(_) | serde_yml::Value::Sequence(_) => {
            None
        }
        other => serde_yml::to_string(other)
            .ok()
            .map(|s| s.trim().to_string()),
    }
}

/// Resolve a (possibly bare, no-`.md`) store-relative path to the real content
/// `.md` file on disk, trying the path as written and then with `.md` appended.
/// Returns the store-relative path of whichever exists, else `None`.
fn resolve_content_file(store: &Store, rel: &Path) -> Option<PathBuf> {
    let as_written = store.abs_path(rel);
    if as_written.is_file() {
        return store.rel_path(&as_written).filter(|r| is_content_file(r));
    }
    let with_md = PathBuf::from(format!("{}.md", path_to_str(rel)));
    let abs = store.abs_path(&with_md);
    if abs.is_file() {
        return store.rel_path(&abs).filter(|r| is_content_file(r));
    }
    None
}

/// True if a store-relative path lives under `layer`'s top-level folder. The
/// `--in` scope predicate for the link / all-content candidate paths (the
/// structured path scopes itself via `Query::with_layer`).
fn path_in_layer(rel: &Path, layer: Layer) -> bool {
    rel.components().next().and_then(|c| c.as_os_str().to_str()) == Some(layer.dir_name())
}

/// True if a store-relative path is a *content* file the search should scan:
/// under `sources/` / `records/` / `wiki/`, a `.md` file, and not a per-folder
/// `index.md`. (`index.jsonl` is excluded by the `.md` check; `DB.md` / `log.md`
/// live at the root, outside every layer, and never reach here.)
fn is_content_file(rel: &Path) -> bool {
    let first = rel.components().next().and_then(|c| c.as_os_str().to_str());
    if !matches!(first, Some("sources" | "records" | "wiki")) {
        return false;
    }
    if rel.extension().and_then(|e| e.to_str()) != Some("md") {
        return false;
    }
    rel.file_name().and_then(|n| n.to_str()) != Some("index.md")
}

/// A searcher configured the way `rg` scans content: skip binary files, report
/// line numbers (the [`UTF8`] sink requires them).
fn build_searcher() -> Searcher {
    SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build()
}

/// Intersect an optional running set with the next filter's set: the first
/// active filter seeds it, each subsequent one ANDs in. (Kept generic over the
/// accumulator so `resolve_candidates` and `link_candidates` share it.)
fn intersect(acc: Option<BTreeSet<PathBuf>>, next: BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    match acc {
        None => next,
        Some(prev) => prev.intersection(&next).cloned().collect(),
    }
}

/// Store-relative path → forward-slash string (never `\`), for output + the
/// `.md`-suffix resolution.
fn path_to_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    // ── Corpus-driven tests (the golden master), run in-process ──────────────
    //
    // `tests/corpora/corpus-a-canonical/EXPECTED/search.json` is the intent-
    // derived contract: each case lists the store-relative paths that MUST
    // match. We open the real corpus and call the real handler core
    // (`collect_matches`) directly — no subprocess, no stdout scraping — and
    // assert the set of matching FILES equals the golden set. (The golden
    // asserts files, not per-line counts — a file with the term on two lines
    // still contributes one path.) This is the binding Block-4 verification.

    /// Absolute path to the corpus-a store root, from this crate's manifest dir.
    fn corpus_a() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/corpora/corpus-a-canonical")
            .canonicalize()
            .expect("corpus-a-canonical must exist")
    }

    /// A [`SearchArgs`] over corpus-a with everything but `query` defaulted; the
    /// per-test flags are layered on by the callers via [`with_flags`].
    fn args(query: &str) -> SearchArgs {
        SearchArgs {
            query: query.to_string(),
            r#type: None,
            r#in: None,
            r#where: Vec::new(),
            linked_from: None,
            linked_to: None,
            updated_after: None,
            updated_before: None,
            created_after: None,
            created_before: None,
            limit: None,
            dir: corpus_a().to_string_lossy().into_owned(),
        }
    }

    /// Apply a flat `["--type", "meeting", ...]` flag list (the shape the golden
    /// master stores) onto a [`SearchArgs`]. Only the flags the corpus actually
    /// uses are recognized; an unknown flag is a test-author error.
    fn with_flags(mut a: SearchArgs, flags: &[&str]) -> SearchArgs {
        let mut i = 0;
        while i < flags.len() {
            match flags[i] {
                "--type" => a.r#type = Some(flags[i + 1].to_string()),
                "--in" => a.r#in = Some(flags[i + 1].to_string()),
                "--where" => a.r#where.push(flags[i + 1].to_string()),
                "--linked-from" => a.linked_from = Some(flags[i + 1].to_string()),
                "--linked-to" => a.linked_to = Some(flags[i + 1].to_string()),
                "--updated-after" => a.updated_after = Some(flags[i + 1].to_string()),
                "--updated-before" => a.updated_before = Some(flags[i + 1].to_string()),
                "--created-after" => a.created_after = Some(flags[i + 1].to_string()),
                "--created-before" => a.created_before = Some(flags[i + 1].to_string()),
                "--limit" => a.limit = Some(flags[i + 1].parse().expect("limit is a number")),
                other => panic!("unhandled test flag {other}"),
            }
            i += 2;
        }
        a
    }

    /// Run the real handler core over corpus-a and return the deduped, sorted
    /// set of store-relative file paths that matched.
    fn search_files(query: &str, flags: &[&str]) -> BTreeSet<String> {
        let store = Store::open(&corpus_a()).expect("open corpus-a");
        let a = with_flags(args(query), flags);
        let matches = collect_matches(&store, &a).expect("search must succeed");
        matches.into_iter().map(|m| m.file).collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn golden_plain_phrase_no_filter() {
        // Body-only phrase in the renewal call record and the project wiki page.
        assert_eq!(
            search_files("volume discount", &[]),
            set(&[
                "records/meetings/2026/05/2026-05-22-northstar-renewal-call.md",
                "wiki/projects/northstar-renewal.md",
            ])
        );
    }

    #[test]
    fn golden_distinctive_phrase_single_source() {
        assert_eq!(
            search_files("master services agreement", &[]),
            set(&["sources/docs/2026-03-15-northstar-msa.md"])
        );
    }

    #[test]
    fn golden_case_sensitive_default() {
        // `async` is present in Elena's contact record and her wiki bio.
        assert_eq!(
            search_files("async", &[]),
            set(&[
                "records/contacts/elena-rodriguez.md",
                "wiki/people/elena-rodriguez.md",
            ])
        );
    }

    #[test]
    fn golden_layer_scope_in_wiki() {
        assert_eq!(
            search_files("renewal", &["--in", "wiki"]),
            set(&[
                "wiki/people/elena-rodriguez.md",
                "wiki/people/sarah-chen.md",
                "wiki/projects/northstar-renewal.md",
                "wiki/synthesis/2026-renewal-plan.md",
            ])
        );
    }

    #[test]
    fn golden_type_scope_meeting() {
        assert_eq!(
            search_files("175", &["--type", "meeting"]),
            set(&["records/meetings/2026/05/2026-05-22-northstar-renewal-call.md"])
        );
    }

    #[test]
    fn golden_type_scope_contact_all_four() {
        assert_eq!(
            search_files("Northstar", &["--type", "contact"]),
            set(&[
                "records/contacts/david-kim.md",
                "records/contacts/elena-rodriguez.md",
                "records/contacts/marcus-okafor.md",
                "records/contacts/sarah-chen.md",
            ])
        );
    }

    #[test]
    fn golden_regex_alternation_type_invoice() {
        // Only the May AWS invoice carries `unpaid`; no invoice is `void`.
        assert_eq!(
            search_files("(unpaid|void)", &["--type", "invoice"]),
            set(&["records/invoices/2026/05/2026-05-31-aws-may.md"])
        );
    }

    #[test]
    fn golden_type_scope_company_excludes_other_layers() {
        // `Figma` also appears in emails/invoices/expenses; --type excludes them.
        assert_eq!(
            search_files("Figma", &["--type", "company"]),
            set(&["records/companies/figma.md"])
        );
    }

    #[test]
    fn golden_type_scope_expense_single_shard_date() {
        // Seven expense records dated the last day of May, all three vendors.
        assert_eq!(
            search_files("2026-05-31", &["--type", "expense"]),
            set(&[
                "records/expenses/2026/05/2026-05-31-aws-0031.md",
                "records/expenses/2026/05/2026-05-31-aws-0124.md",
                "records/expenses/2026/05/2026-05-31-aws-0217.md",
                "records/expenses/2026/05/2026-05-31-figma-0062.md",
                "records/expenses/2026/05/2026-05-31-figma-0155.md",
                "records/expenses/2026/05/2026-05-31-github-0093.md",
                "records/expenses/2026/05/2026-05-31-github-0186.md",
            ])
        );
    }

    #[test]
    fn golden_type_plus_updated_after() {
        // Of the contacts mentioning renewal, only Elena and Sarah were updated
        // after 2026-05-15 (Marcus and David predate the cutoff). This is the
        // combined sidecar-filter + time-window path.
        assert_eq!(
            search_files(
                "renewal",
                &["--type", "contact", "--updated-after", "2026-05-15"]
            ),
            set(&[
                "records/contacts/elena-rodriguez.md",
                "records/contacts/sarah-chen.md",
            ])
        );
    }

    // ── Behavior the golden master does not pin, derived from the SPEC ────────

    #[test]
    fn meta_files_are_never_matched() {
        // `db-md` is the DB.md frontmatter `type`; `index` is in every index.md.
        // Neither meta file is content, so a no-filter search for those terms
        // must never return DB.md / index.md / index.jsonl / log.md.
        for q in ["db-md", "Knowledge base index"] {
            let hits = search_files(q, &[]);
            for path in &hits {
                assert!(
                    !path.ends_with("DB.md")
                        && !path.ends_with("/index.md")
                        && !path.ends_with("index.jsonl")
                        && !path.ends_with("log.md"),
                    "search `{q}` returned meta file {path}"
                );
            }
        }
    }

    #[test]
    fn frontmatter_block_is_searched_not_just_body() {
        // The contact `status: active` lives only in the frontmatter block (no
        // body mention). A match proves the scan covers the frontmatter, per the
        // golden's "frontmatter block + body of every content file" contract.
        let hits = search_files("status: active", &["--type", "contact"]);
        assert!(
            hits.contains("records/contacts/sarah-chen.md"),
            "frontmatter line should be searchable, got {hits:?}"
        );
    }

    #[test]
    fn no_match_is_empty_success_not_error() {
        // A term in no content file yields an empty result (Ok), not an error —
        // "not found" is data for the agent.
        let store = Store::open(&corpus_a()).unwrap();
        let matches = collect_matches(&store, &args("zzz-nonexistent-term-zzz")).unwrap();
        assert!(matches.is_empty(), "expected no matches, got {matches:?}");
    }

    #[test]
    fn match_carries_file_line_and_text() {
        // The `rg`-compatible shape is per-match: a content file path, a 1-based
        // line number, and the matching line's text. The distinctive MSA phrase
        // occurs in exactly one source doc, on its body line.
        let store = Store::open(&corpus_a()).unwrap();
        let matches = collect_matches(&store, &args("master services agreement")).unwrap();
        assert!(!matches.is_empty(), "expected at least one match");
        for m in &matches {
            assert_eq!(m.file, "sources/docs/2026-03-15-northstar-msa.md");
            assert!(m.line >= 1, "line numbers are 1-based, got {}", m.line);
            assert!(
                m.text.contains("master services agreement"),
                "the match text must be the matching line, got {:?}",
                m.text
            );
            assert!(
                !m.text.ends_with('\n') && !m.text.ends_with('\r'),
                "trailing newline must be trimmed: {:?}",
                m.text
            );
        }
    }

    #[test]
    fn limit_caps_match_count() {
        // `--limit 1` over a query with many hits returns exactly one match.
        let store = Store::open(&corpus_a()).unwrap();
        let a = with_flags(args("Northstar"), &["--type", "contact", "--limit", "1"]);
        let matches = collect_matches(&store, &a).unwrap();
        assert_eq!(matches.len(), 1, "limit must cap the match count");
    }

    #[test]
    fn linked_to_filters_to_backlinkers() {
        // Files that wiki-link TO the company record. The renewal call meeting,
        // both contacts, etc. link to northstar; intersect with a query term to
        // prove the candidate set is the backlinkers, scanned for text.
        // Sarah's contact links to northstar and contains "renewal".
        let hits = search_files("renewal", &["--linked-to", "records/companies/northstar"]);
        assert!(
            hits.contains("records/contacts/sarah-chen.md"),
            "a backlinker containing the term should match, got {hits:?}"
        );
        // The company record itself does not link to itself, so even though it
        // may contain `renewal`, it is not in the --linked-to candidate set.
        assert!(
            !hits.contains("records/companies/northstar.md"),
            "the target itself is not its own backlinker: {hits:?}"
        );
    }

    #[test]
    fn linked_from_filters_to_forward_targets() {
        // Files the renewal call meeting links TO. The meeting links to the two
        // contacts + the email; scanning those for "Operations" should hit a
        // contact record (Elena/Sarah are Directors of Operations) and never the
        // meeting file itself.
        let seed = "records/meetings/2026/05/2026-05-22-northstar-renewal-call";
        let hits = search_files("Operations", &["--linked-from", seed]);
        assert!(
            hits.contains("records/contacts/sarah-chen.md")
                || hits.contains("records/contacts/elena-rodriguez.md"),
            "a forward-linked contact containing the term should match, got {hits:?}"
        );
        assert!(
            !hits
                .iter()
                .any(|p| p.ends_with("northstar-renewal-call.md")),
            "the seed file is not among its own forward targets: {hits:?}"
        );
    }

    #[test]
    fn invalid_layer_is_an_error_with_code() {
        // `--in nope` is rejected before any scan, with the BAD_LAYER code.
        let store = Store::open(&corpus_a()).unwrap();
        let a = with_flags(args("x"), &["--in", "nope"]);
        let err = collect_matches(&store, &a).expect_err("bad --in must error");
        assert_eq!(err.code, "BAD_LAYER", "got {err:?}");
    }

    #[test]
    fn invalid_query_regex_is_reported() {
        // An unbalanced group is an invalid ripgrep regex → structured error
        // (code BAD_QUERY_REGEX), not a panic.
        let store = Store::open(&corpus_a()).unwrap();
        let err =
            collect_matches(&store, &args("(unterminated")).expect_err("bad regex must error");
        assert_eq!(err.code, "BAD_QUERY_REGEX", "got {err:?}");
    }

    #[test]
    fn invalid_where_clause_is_reported() {
        // A `--where` without `=` is a usage-class BAD_WHERE error.
        let store = Store::open(&corpus_a()).unwrap();
        let a = with_flags(args("x"), &["--type", "contact", "--where", "no-equals"]);
        let err = collect_matches(&store, &a).expect_err("bad --where must error");
        assert_eq!(err.code, "BAD_WHERE", "got {err:?}");
    }

    #[test]
    fn invalid_timestamp_is_reported() {
        // A non-RFC3339 `--updated-after` is a usage-class BAD_TIMESTAMP error.
        let store = Store::open(&corpus_a()).unwrap();
        let a = with_flags(
            args("x"),
            &["--type", "contact", "--updated-after", "yesterday"],
        );
        let err = collect_matches(&store, &a).expect_err("bad timestamp must error");
        assert_eq!(err.code, "BAD_TIMESTAMP", "got {err:?}");
    }

    #[test]
    fn not_a_store_is_rejected() {
        // `run` opening a non-store (no DB.md) is the NOT_A_STORE contract.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Context {
            json: true,
            color: crate::context::ColorChoice::Never,
        };
        let a = SearchArgs {
            dir: tmp.path().to_string_lossy().into_owned(),
            ..args("x")
        };
        let err = run(&ctx, &a).expect_err("non-store must error");
        assert_eq!(err.code, "NOT_A_STORE", "got {err:?}");
    }

    // ── Pure-unit coverage of the handler's local logic ──────────────────────

    #[test]
    fn parse_layer_maps_known_and_rejects_unknown() {
        assert_eq!(parse_layer(None).unwrap(), None);
        assert_eq!(parse_layer(Some("sources")).unwrap(), Some(Layer::Sources));
        assert_eq!(parse_layer(Some("records")).unwrap(), Some(Layer::Records));
        assert_eq!(parse_layer(Some("wiki")).unwrap(), Some(Layer::Wiki));
        assert!(parse_layer(Some("Sources")).is_err(), "case-sensitive");
        assert!(parse_layer(Some("log")).is_err());
    }

    #[test]
    fn split_kv_requires_equals() {
        assert_eq!(split_kv("status=active").unwrap(), ("status", "active"));
        // A value may itself contain `=`; only the first splits.
        assert_eq!(split_kv("k=a=b").unwrap(), ("k", "a=b"));
        assert!(split_kv("no-equals").is_err());
    }

    #[test]
    fn parse_ts_accepts_rfc3339_and_bare_date() {
        assert!(parse_ts("2026-05-15T09:00:00Z").is_some());
        assert!(parse_ts("2026-05-15T09:00:00-07:00").is_some());
        // Bare date → midnight UTC.
        let d = parse_ts("2026-05-15").unwrap();
        assert_eq!(d.to_rfc3339(), "2026-05-15T00:00:00+00:00");
        assert!(parse_ts("not-a-date").is_none());
    }

    #[test]
    fn time_window_bounds_are_inclusive_and_exclude_undated() {
        let w = TimeWindows {
            updated_after: parse_ts("2026-05-15"),
            updated_before: None,
            created_after: None,
            created_before: None,
        };
        let on_cutoff = chrono::DateTime::parse_from_rfc3339("2026-05-15T00:00:00Z").unwrap();
        let after = chrono::DateTime::parse_from_rfc3339("2026-05-22T00:00:00Z").unwrap();
        let before = chrono::DateTime::parse_from_rfc3339("2026-05-01T00:00:00Z").unwrap();
        // Inclusive "at or after": the cutoff instant itself passes.
        assert!(w.matches((None, Some(on_cutoff))));
        assert!(w.matches((None, Some(after))));
        assert!(!w.matches((None, Some(before))));
        // An absent `updated` cannot satisfy an "after" bound.
        assert!(!w.matches((None, None)));
    }

    #[test]
    fn time_window_inactive_matches_everything() {
        let w = TimeWindows {
            updated_after: None,
            updated_before: None,
            created_after: None,
            created_before: None,
        };
        assert!(!w.is_active());
        assert!(w.matches((None, None)), "no bounds → no filtering");
    }

    #[test]
    fn path_in_layer_keys_off_first_component() {
        assert!(path_in_layer(Path::new("wiki/people/x.md"), Layer::Wiki));
        assert!(!path_in_layer(
            Path::new("wiki/people/x.md"),
            Layer::Records
        ));
        assert!(path_in_layer(
            Path::new("records/contacts/c.md"),
            Layer::Records
        ));
        assert!(path_in_layer(
            Path::new("sources/emails/2026/05/e.md"),
            Layer::Sources
        ));
        assert!(!path_in_layer(
            Path::new("records/contacts/c.md"),
            Layer::Wiki
        ));
    }

    #[test]
    fn is_content_file_excludes_meta_and_non_layer() {
        assert!(is_content_file(Path::new("records/contacts/sarah.md")));
        assert!(is_content_file(Path::new("sources/emails/2026/05/e.md")));
        assert!(is_content_file(Path::new("wiki/people/x.md")));
        // Meta + non-content:
        assert!(!is_content_file(Path::new("records/contacts/index.md")));
        assert!(!is_content_file(Path::new("records/contacts/index.jsonl")));
        assert!(!is_content_file(Path::new("DB.md")));
        assert!(!is_content_file(Path::new("log.md")));
        assert!(!is_content_file(Path::new("log/2026-04.md")));
    }

    #[test]
    fn intersect_seeds_then_ands() {
        let a: BTreeSet<PathBuf> = ["x", "y", "z"].iter().map(PathBuf::from).collect();
        let b: BTreeSet<PathBuf> = ["y", "z", "w"].iter().map(PathBuf::from).collect();
        // First filter seeds the accumulator verbatim.
        assert_eq!(intersect(None, a.clone()), a);
        // Second filter ANDs.
        let both = intersect(Some(a), b);
        let expected: BTreeSet<PathBuf> = ["y", "z"].iter().map(PathBuf::from).collect();
        assert_eq!(both, expected);
    }

    #[test]
    fn frontmatter_block_extracts_between_fences() {
        let text = "---\ntype: contact\nupdated: 2026-05-01T00:00:00Z\n---\nbody\n";
        let yaml = frontmatter_block(text).unwrap();
        assert!(yaml.contains("type: contact"));
        assert!(yaml.contains("updated:"));
        assert!(!yaml.contains("body"));
        // No leading fence → None.
        assert!(frontmatter_block("no frontmatter\n").is_none());
    }
}
