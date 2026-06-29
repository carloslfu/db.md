//! `validate` — the validation engine.
//!
//! The canonical issue-code vocabulary is **SPEC.md § Validation** (that table
//! is the single source of truth). This module implements exactly those codes
//! — no more, no fewer. If a code is added here it must be added to the SPEC
//! table in the same change. The codes are exposed as the [`codes`] constants
//! so call sites never spell a code as a bare string literal.
//!
//! **Two scopes.** [`validate_working_set`] is the loop default: content files
//! changed since `since`, plus any file whose wiki-links target a changed path.
//! The changed set and the per-file checks are O(changed); the incoming linkers
//! are found by a *single* embedded-ripgrep pass over the store for the whole
//! changed set at once ([`Store::find_links_to_any`], one scan — not a full read
//! per changed object, and not the parse-the-tree walk `--all` does). On this
//! changed-set path it never builds the global cross-file state.
//!
//! The **one** exception is the vacuous-pass guard: when the change log records
//! no objects since the cutoff and no explicit `--since` was given (a fresh
//! store, a missing/empty `log.md`, or external edits never logged), the default
//! call falls back to a single per-file content sweep ([`Store::walk`]) so an
//! externally edited or freshly copied store cannot pass validation vacuously.
//! That fallback is O(store) by design; the O(changed) guarantee is about the
//! normal post-write path, not this safety net.
//!
//! [`validate_all`] is the full SWEEP: it adds the checks that need the global
//! cross-file state — entity-dedup `DUP_*`, every-index sync, and `log.md`
//! ordering.
//!
//! ## Why this module is self-contained
//!
//! Validation does its own frontmatter split, YAML parse, wiki-link scan,
//! log-header parse, and file walk here, reading only the two public,
//! caller-populated fields of a [`Store`]: [`Store::root`] and
//! [`Store::config`] — rather than routing through the sibling modules
//! ([`crate::parser`], [`crate::store`], [`crate::log`], [`crate::index`]).
//! Keeping the checks local lets the validator report precise, per-issue
//! diagnostics (exact codes, file, and context) without coupling its output to
//! incidental behavior of the shared readers; the public surface and the
//! emitted issue vocabulary are the contract.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, FixedOffset, NaiveDateTime};
use serde_norway::Value;

use crate::parser::{Schema, Shape};
use crate::store::Store;

/// Severity of a validation [`Issue`]. Any [`Severity::Error`] fails validation
/// (non-zero exit); warnings and info do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Blocks: a hard violation of the format or doctrine.
    Error,
    /// A decision point the agent resolves at its discretion.
    Warning,
    /// Visibility only; never affects exit status.
    Info,
}

/// A single structured validation finding. Agent-primary and machine-parseable
/// via `--json`; `suggestion` is a deterministic remediation hint the agent
/// applies without guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// The severity; only [`Severity::Error`] fails validation.
    pub severity: Severity,
    /// The structured code, e.g. `"WIKI_LINK_SHORT_FORM"` — one of [`codes`].
    pub code: &'static str,
    /// The file the issue is about.
    pub file: PathBuf,
    /// The 1-based line, when applicable.
    pub line: Option<u32>,
    /// The frontmatter key, when the issue is about a specific field.
    pub key: Option<String>,
    /// A human-readable message.
    pub message: String,
    /// A deterministic remediation hint, when one exists.
    pub suggestion: Option<String>,
    /// Other files involved (e.g. the duplicate partner in a collision).
    pub related: Vec<PathBuf>,
}

impl Issue {
    /// True if this issue fails validation (i.e. its severity is
    /// [`Severity::Error`]).
    pub fn is_error(&self) -> bool {
        matches!(self.severity, Severity::Error)
    }
}

/// The canonical validation issue codes — one constant per row of the SPEC.md
/// § Validation table. Call sites reference these instead of bare strings so
/// the code and the SPEC table can never silently drift.
pub mod codes {
    /// path has no `DB.md`; not a db.md store.
    pub const NOT_A_STORE: &str = "NOT_A_STORE";
    /// the store's `DB.md` is not `type: db-md`.
    pub const DB_MD_BAD_TYPE: &str = "DB_MD_BAD_TYPE";
    /// the store's `DB.md` frontmatter lacks `scope` or `owner`.
    pub const DB_MD_MISSING_FIELD: &str = "DB_MD_MISSING_FIELD";
    /// `DB.md` has an `##` section other than the three recognized ones.
    pub const DB_MD_UNKNOWN_SECTION: &str = "DB_MD_UNKNOWN_SECTION";
    /// a `DB.md ## Schemas` field declaration is malformed (empty or duplicate
    /// field name) or carries an unrecognized modifier.
    pub const DB_MD_SCHEMA_FIELD: &str = "DB_MD_SCHEMA_FIELD";
    /// content file has no `type:`.
    pub const FM_MISSING_TYPE: &str = "FM_MISSING_TYPE";
    /// content file has no `created:`.
    pub const FM_MISSING_CREATED: &str = "FM_MISSING_CREATED";
    /// content file has no `updated:`.
    pub const FM_MISSING_UPDATED: &str = "FM_MISSING_UPDATED";
    /// content file can't be read (not valid UTF-8, or an I/O error).
    pub const FM_UNREADABLE: &str = "FM_UNREADABLE";
    /// frontmatter block isn't valid YAML.
    pub const FM_MALFORMED_YAML: &str = "FM_MALFORMED_YAML";
    /// `created` or `updated` isn't ISO-8601.
    pub const FM_BAD_TIMESTAMP: &str = "FM_BAD_TIMESTAMP";
    /// `meta-type` is present but not one of fact / operational / conclusion.
    pub const FM_BAD_META_TYPE: &str = "FM_BAD_META_TYPE";
    /// content file has no `summary`.
    pub const SUMMARY_MISSING: &str = "SUMMARY_MISSING";
    /// `summary` present but empty.
    pub const SUMMARY_EMPTY: &str = "SUMMARY_EMPTY";
    /// `summary` contains newlines.
    pub const SUMMARY_MULTILINE: &str = "SUMMARY_MULTILINE";
    /// `summary` > 200 chars.
    pub const SUMMARY_TOO_LONG: &str = "SUMMARY_TOO_LONG";
    /// wiki-link target isn't a full store-relative path.
    pub const WIKI_LINK_SHORT_FORM: &str = "WIKI_LINK_SHORT_FORM";
    /// wiki-link target file doesn't exist.
    pub const WIKI_LINK_BROKEN: &str = "WIKI_LINK_BROKEN";
    /// wiki-link target matches multiple files (defensive).
    pub const WIKI_LINK_AMBIGUOUS: &str = "WIKI_LINK_AMBIGUOUS";
    /// wiki-link target carries a `.md` extension — drop it.
    pub const WIKI_LINK_HAS_EXTENSION: &str = "WIKI_LINK_HAS_EXTENSION";
    /// frontmatter list uses inline `[[[a]], [[b]]]` — use block form.
    pub const WIKI_LINK_FLOW_FORM_LIST: &str = "WIKI_LINK_FLOW_FORM_LIST";
    /// two files declare the same explicit `id`.
    pub const DUP_ID: &str = "DUP_ID";
    /// two records of a type collide on a `DB.md ## Schemas` `unique:` key.
    pub const DUP_UNIQUE_KEY: &str = "DUP_UNIQUE_KEY";
    /// a `DB.md` schema requires a field that's absent.
    pub const SCHEMA_MISSING_REQUIRED: &str = "SCHEMA_MISSING_REQUIRED";
    /// a value doesn't match the schema's shape modifier.
    pub const SCHEMA_SHAPE_MISMATCH: &str = "SCHEMA_SHAPE_MISMATCH";
    /// a `link to <prefix>/` field has a plain or wrong-prefix value.
    pub const SCHEMA_LINK_PREFIX_MISMATCH: &str = "SCHEMA_LINK_PREFIX_MISMATCH";
    /// a value isn't in the schema's `enum`.
    pub const SCHEMA_ENUM_VIOLATION: &str = "SCHEMA_ENUM_VIOLATION";
    /// a write was attempted on a `### Frozen pages` path (write-time).
    pub const POLICY_FROZEN_PAGE: &str = "POLICY_FROZEN_PAGE";
    /// a file with an `### Ignored types` type exists.
    pub const POLICY_IGNORED_TYPE_PRESENT: &str = "POLICY_IGNORED_TYPE_PRESENT";
    /// a `meta-type: conclusion` record derives from an ignored-type record.
    pub const POLICY_IGNORED_TYPE_DERIVED: &str = "POLICY_IGNORED_TYPE_DERIVED";
    /// a `log.md` entry header timestamp is unparseable.
    pub const LOG_BAD_TIMESTAMP: &str = "LOG_BAD_TIMESTAMP";
    /// a `log.md` entry kind isn't recognized.
    pub const LOG_UNKNOWN_KIND: &str = "LOG_UNKNOWN_KIND";
    /// `log.md` entries aren't in non-decreasing time order (possible rewrite).
    pub const LOG_OUT_OF_ORDER: &str = "LOG_OUT_OF_ORDER";
    /// a non-empty canonical folder lacks `index.md`.
    pub const INDEX_MISSING: &str = "INDEX_MISSING";
    /// an `index.md` lists a file that no longer exists.
    pub const INDEX_STALE_ENTRY: &str = "INDEX_STALE_ENTRY";
    /// a file isn't listed in its folder's `index.md`.
    pub const INDEX_MISSING_ENTRY: &str = "INDEX_MISSING_ENTRY";
    /// an `index.md` sits in an empty / non-canonical folder.
    pub const INDEX_ORPHAN: &str = "INDEX_ORPHAN";
    /// an index's `scope:` doesn't match its filesystem location.
    pub const INDEX_WRONG_SCOPE: &str = "INDEX_WRONG_SCOPE";
    /// an index entry's text doesn't match the target file's `summary`.
    pub const INDEX_SUMMARY_MISMATCH: &str = "INDEX_SUMMARY_MISMATCH";
    /// a type-folder's `index.jsonl` twin is missing.
    pub const INDEX_JSONL_MISSING: &str = "INDEX_JSONL_MISSING";
    /// a file isn't in the `index.jsonl`, or a jsonl record points at a missing
    /// file.
    pub const INDEX_JSONL_DESYNC: &str = "INDEX_JSONL_DESYNC";
    /// a `index.jsonl` record's fields don't match the file's frontmatter.
    pub const INDEX_JSONL_STALE: &str = "INDEX_JSONL_STALE";
    /// `tags` isn't a flat YAML list of short scalar labels.
    pub const TAGS_MALFORMED: &str = "TAGS_MALFORMED";
    /// a line in `assets.jsonl` is not a valid asset record.
    pub const ASSET_MANIFEST_MALFORMED: &str = "ASSET_MANIFEST_MALFORMED";
    /// a content file references an `asset`/`assets` path with no record in
    /// `assets.jsonl` (run `dbmd assets scan`).
    pub const ASSET_UNDECLARED: &str = "ASSET_UNDECLARED";
    /// an `assets.jsonl` record names a wrapper file that does not exist.
    pub const ASSET_WRAPPER_BROKEN: &str = "ASSET_WRAPPER_BROKEN";
    /// an `assets.jsonl` record's path is referenced by no wrapper.
    pub const ASSET_MANIFEST_ORPHAN: &str = "ASSET_MANIFEST_ORPHAN";
    /// an `asset`/`assets` path points at a tracked markdown content file.
    pub const ASSET_PATH_IS_CONTENT: &str = "ASSET_PATH_IS_CONTENT";
}

/// The SPEC's `summary` length bound (chars). Over it → `SUMMARY_TOO_LONG`.
const MAX_SUMMARY_LEN: usize = 200;

/// Recognized `log.md` entry kinds (SPEC § `log.md`). Anything else →
/// `LOG_UNKNOWN_KIND` (warning, not error).
const RECOGNIZED_LOG_KINDS: &[&str] = &[
    "ingest",
    "create",
    "update",
    "delete",
    "rename",
    "link",
    "validate",
    "index-rebuild",
    "contradiction",
];

// ─────────────────────────────────────────────────────────────────────────────
//  Public entrypoints
// ─────────────────────────────────────────────────────────────────────────────

/// **Loop default.** Validate the working set: content files changed since
/// `since` (default: the last `validate` entry in `log.md`), plus any file whose
/// wiki-links target a changed/renamed/removed path. Per-file *checks* only —
/// none of the cross-file global passes (entity-dedup, every-index sync,
/// `log.md` ordering) that `--all` adds. If the default call finds no logged
/// changed objects, it falls back to a per-file content sweep so an externally
/// edited or freshly copied store cannot pass vacuously.
///
/// **Cost.** The changed set is read from `log.md` — O(changed): every
/// `create`/`update`/`ingest`/`rename`/`delete`/`link` entry newer than the
/// cutoff names an object. Per-file frontmatter + link-doctrine checks then run
/// over that set plus its incoming linkers — also O(changed). The one part that
/// is *not* O(changed) is discovering those incoming linkers: a link to a
/// changed path can live in the body or a typed frontmatter field of any file,
/// so it is found by a **single** embedded-ripgrep pass over the store
/// ([`Store::find_links_to_any`]) for the whole changed set at once — one store
/// scan, flat in the changed-set size. (It was previously a full store read
/// *per* changed object — `O(changed × store)`; that is the blow-up this path
/// no longer pays.) The unavoidable single content scan is the same shape as
/// free-text `dbmd search`; the sidecar `links` projection can't replace it
/// because it omits body/typed-field edges.
pub fn validate_working_set(
    store: &Store,
    since: Option<DateTime<FixedOffset>>,
) -> crate::Result<Vec<Issue>> {
    if !store_marker_present(store) {
        return Ok(vec![not_a_store_issue(store)]);
    }

    let cutoff = match since {
        Some(ts) => Some(ts),
        None => last_validate_at(store),
    };

    // 1. Changed objects, straight from the log (O(changed) — never a walk).
    let changed = changed_objects_since(store, cutoff);
    if changed.is_empty() && since.is_none() {
        return validate_content_sweep(store);
    }

    // 2. Add every file with an incoming wiki-link to a changed/renamed/removed
    //    path (the linker may now be stale even though it didn't change). The
    //    incoming-linker scan is `Store::find_links_to_any` — ONE embedded-ripgrep
    //    pass over the store for the WHOLE changed set (one `.md` walk, one
    //    presence-only/early-exit scan per file), not one walk per object. This
    //    is the fix for the `O(changed × store)` blow-up that calling
    //    `find_links_to` in a loop produced (a full store read per changed
    //    object); the cost is now a single store scan regardless of how many
    //    objects changed. A returned self-link is harmlessly deduped by the set
    //    (the object is already inserted below).
    let changed_targets: Vec<PathBuf> = changed.iter().cloned().collect();
    let mut working: BTreeSet<PathBuf> = changed;
    for linker in store.find_links_to_any(&changed_targets)? {
        working.insert(linker);
    }

    let mut issues = Vec::new();
    for rel in &working {
        let abs = store.root.join(rel);
        // A changed path can be a *deletion* — skip files that no longer exist;
        // the incoming-linker scan above already flagged links into them.
        if !abs.is_file() {
            continue;
        }
        // `None` basename index: the working-set pass does not build the
        // store-wide basename map (that is a `--all`-only structure), so a bare
        // short-form target is reported as plain `WIKI_LINK_SHORT_FORM` and the
        // `--all` sweep does the ambiguity upgrade.
        check_content_file(store, rel, &abs, None, &mut issues);
    }
    issues.sort_by(issue_order);
    Ok(issues)
}

fn validate_content_sweep(store: &Store) -> crate::Result<Vec<Issue>> {
    let mut issues = Vec::new();
    for rel in store.walk()? {
        let abs = store.root.join(&rel);
        check_content_file(store, &rel, &abs, None, &mut issues);
    }
    issues.sort_by(issue_order);
    Ok(issues)
}

/// **Full SWEEP (O(store)).** Validate every file, every link, and every index,
/// adding the cross-file checks that need global state: entity-dedup `DUP_*`,
/// every-index sync (md + jsonl), and `log.md` ordering. CI / recovery, not the
/// loop.
pub fn validate_all(store: &Store) -> crate::Result<Vec<Issue>> {
    if !store_marker_present(store) {
        return Ok(vec![not_a_store_issue(store)]);
    }

    let mut issues = Vec::new();

    // Store-identity file: `DB.md` shape (type / required fields / section
    // headers). A single root file, checked once in the sweep — not a content
    // file (it carries no `summary`), so it is not part of `walk_content_files`.
    check_db_md(store, &mut issues);

    let files = walk_content_files(&store.root);

    // The basename index makes the short-form wiki-link check able to upgrade a
    // bare-basename target to `WIKI_LINK_AMBIGUOUS` when it matches ≥2 files.
    // Built once from the already-gathered sweep list (no extra walk); only the
    // `--all` path has it (the working-set path stays O(changed)).
    let basenames = build_basename_index(&files);

    // Per-file checks over the whole store.
    let mut parsed: Vec<(PathBuf, Parsed)> = Vec::new();
    for rel in &files {
        let abs = store.root.join(rel);
        if let Some(p) = check_content_file(store, rel, &abs, Some(&basenames), &mut issues) {
            parsed.push((rel.clone(), p));
        }
    }

    // Cross-file: hard `id` + soft schema-declared `unique:` dedup collisions.
    check_duplicates(store, &parsed, &mut issues);

    // Cross-file: hierarchical index.md + index.jsonl sync.
    check_indexes(store, &files, &mut issues);

    // Cross-file: log.md well-formedness + ordering.
    check_log(store, &mut issues);

    // Cross-file: asset manifest (assets.jsonl) integrity against wrapper
    // declarations. Text-only, no hashing, no byte reads — a SWEEP check like
    // dedup. Byte presence/correctness is `dbmd assets verify`, not validate, so
    // a fresh clone with no restored bytes still passes here.
    check_assets(store, &parsed, &mut issues);

    issues.sort_by(issue_order);
    Ok(issues)
}

// ─────────────────────────────────────────────────────────────────────────────
//  Per-file content checks (shared by both scopes)
// ─────────────────────────────────────────────────────────────────────────────

/// What `validate_all`'s cross-file pass needs from a per-file parse: the
/// parsed YAML mapping (for dedup keys) and the raw frontmatter text (for
/// text-based wiki-link extraction). The body and fence-line are consumed
/// inline during the per-file pass and not carried here.
struct Parsed {
    /// The parsed top-level YAML mapping, keyed by string. `None` ⇒ malformed
    /// YAML (a `FM_MALFORMED_YAML` was already emitted).
    fm: Option<BTreeMap<String, Value>>,
    /// The raw frontmatter YAML text (between the fences) — the source for
    /// text-based wiki-link extraction in dedup.
    fm_yaml: String,
}

/// Run every per-file check on one content file, pushing issues. Returns the
/// parsed file so `validate_all` can reuse it for cross-file checks. Returns
/// `None` only when the file is unreadable or has no frontmatter block at all
/// (which for a content file is itself reported).
fn check_content_file(
    store: &Store,
    rel: &Path,
    abs: &Path,
    basenames: Option<&BasenameIndex>,
    issues: &mut Vec<Issue>,
) -> Option<Parsed> {
    let text = match std::fs::read_to_string(abs) {
        Ok(t) => t,
        Err(e) => {
            // The file exists in the walk but can't be read as UTF-8 text
            // (invalid bytes) or hit an I/O error. Returning `None` silently
            // here let a store whose only content file was binary garbage pass
            // `dbmd validate` with exit 0 — the exact vacuous-pass the fallback
            // sweep exists to prevent. Report it so the agent gets an actionable
            // diagnostic naming the unreadable file (and `index rebuild`, which
            // hard-fails on the same file, isn't the only signal).
            let detail = if e.kind() == std::io::ErrorKind::InvalidData {
                "file is not valid UTF-8 text".to_string()
            } else {
                format!("file could not be read: {e}")
            };
            push(
                issues,
                Severity::Error,
                codes::FM_UNREADABLE,
                rel,
                None,
                None,
                format!("content file is unreadable: {detail}"),
                Some(
                    "save the file as UTF-8 text, or remove it if it isn't a db.md content file"
                        .into(),
                ),
                vec![],
            );
            return None;
        }
    };

    let is_content = is_content_file(rel);

    let (fm_yaml, body, fm_end_line) = match split_frontmatter(&text) {
        Some(split) => split,
        None => {
            // No frontmatter at all. For a content file that means there's no
            // `type:` and no `summary:` — report both the way a parsed-but-empty
            // file would, so the agent gets the same actionable codes.
            if is_content {
                push(
                    issues,
                    Severity::Error,
                    codes::FM_MISSING_TYPE,
                    rel,
                    None,
                    Some("type".into()),
                    "content file has no frontmatter `type:`".into(),
                    Some("add a YAML frontmatter block with `type:`".into()),
                    vec![],
                );
                push(
                    issues,
                    Severity::Error,
                    codes::SUMMARY_MISSING,
                    rel,
                    None,
                    Some("summary".into()),
                    "content file has no `summary`".into(),
                    Some("run `dbmd fm init`".into()),
                    vec![],
                );
            }
            return None;
        }
    };

    // Parse the YAML block.
    let fm: Option<BTreeMap<String, Value>> = match serde_norway::from_str::<Value>(&fm_yaml) {
        Ok(Value::Mapping(map)) => Some(yaml_map_to_btree(&map)),
        // An empty frontmatter block parses as Null; treat as an empty mapping.
        Ok(Value::Null) => Some(BTreeMap::new()),
        Ok(_) => {
            // A scalar / sequence at the top level isn't a frontmatter mapping.
            // Anchor to line 1 — the frontmatter block's opening `---`; the whole
            // block is opaque, so there is no single offending field line.
            push(
                issues,
                Severity::Error,
                codes::FM_MALFORMED_YAML,
                rel,
                Some(1),
                None,
                "frontmatter is not a YAML mapping".into(),
                Some("repair the frontmatter YAML mapping, then rerun `dbmd validate`".into()),
                vec![],
            );
            None
        }
        Err(e) => {
            // Anchor to line 1 (the opening `---`): an unparseable block has no
            // single offending field line; the agent re-reads the whole block.
            push(
                issues,
                Severity::Error,
                codes::FM_MALFORMED_YAML,
                rel,
                Some(1),
                None,
                format!("frontmatter block isn't valid YAML: {e}"),
                Some("repair the frontmatter YAML block, then rerun `dbmd validate`".into()),
                vec![],
            );
            None
        }
    };

    if let Some(map) = &fm {
        // The detailed frontmatter checks only run when the YAML parsed.
        check_frontmatter(store, rel, map, &fm_yaml, basenames, issues, is_content);
    }

    // Wiki-link doctrine checks run on the body of content files. They are NOT
    // run on:
    //   - the root append-only meta files `log.md`/`DB.md` — they reach this
    //     function only via the working-set incoming-linker scan (`walk_all_md`
    //     includes them), and `validate --all` never link-checks their bodies. A
    //     historical `[[deleted-page]]` mention in a `log.md` note, or a `[[…]]`
    //     in DB.md's `## Agent instructions`, must not be `WIKI_LINK_BROKEN`; the
    //     log is append-only, so "fix the link" can't even be applied.
    //   - the derived catalogs `index.md`/`index.jsonl` — their "links" are
    //     GENERATED catalog entries, not authored body wiki-links. A folder's
    //     `index.md` is pulled into the working set as an incoming linker (an
    //     entry `[[records/contacts/a]]` IS a wiki-link to a member, so touching
    //     or deleting any member drags its folder `index.md` in). Its integrity
    //     is the job of `check_indexes` under `--all`, which reports a dangling
    //     entry as `INDEX_STALE_ENTRY` ("run `dbmd index rebuild`"). Body-link-
    //     checking it here instead emitted `WIKI_LINK_BROKEN` ("create the
    //     target") for the SAME condition — a different code with the OPPOSITE
    //     remedy across the loop default vs the sweep, steering an agent to
    //     recreate deleted data. `walk_content_files` skips `index.md` under
    //     `--all` for exactly this reason; the working-set scope must match.
    // Without these guards the two scopes disagree on the same store.
    if !is_root_meta_file(rel) && !is_index_catalog_file(rel) {
        check_body_wiki_links(store, rel, &body, fm_end_line, basenames, issues);
    }

    Some(Parsed { fm, fm_yaml })
}

/// All frontmatter-level checks for a content file with valid YAML.
fn check_frontmatter(
    store: &Store,
    rel: &Path,
    fm: &BTreeMap<String, Value>,
    fm_yaml: &str,
    basenames: Option<&BasenameIndex>,
    issues: &mut Vec<Issue>,
    is_content: bool,
) {
    let type_ = fm.get("type").and_then(scalar_string);

    // ── type ────────────────────────────────────────────────────────────────
    if is_content && type_.is_none() {
        push(
            issues,
            Severity::Error,
            codes::FM_MISSING_TYPE,
            rel,
            fm_key_line_or_top(fm_yaml, "type"),
            Some("type".into()),
            "content file has no `type:`".into(),
            Some("add a `type:` field (e.g. `type: contact`)".into()),
            vec![],
        );
    }

    // ── meta-type (records-only epistemic class; closed enum) ─────────────────
    // Present-but-out-of-enum is an error; absent is fine (effective default
    // `fact`). Sources don't normally carry one, but validating the value when
    // present is layer-agnostic and harmless.
    if is_content {
        // Branch on the raw value, NOT `and_then(scalar_string)`. Pre-filtering
        // through `scalar_string` made a list/mapping value (which returns `None`)
        // short-circuit the whole check, so a structurally-wrong `meta-type`
        // slipped through clean AND was silently reclassified as the default
        // `fact` by the rest of the toolkit. Absent or explicit-`null` is fine
        // (effective default `fact`); a present non-null value must be a scalar in
        // the closed enum. This mirrors the sibling timestamp check below, which
        // was already hardened against the same non-scalar escape.
        if let Some(v) = fm.get("meta-type").filter(|v| !v.is_null()) {
            match scalar_string(v) {
                Some(mt) if matches!(mt.as_str(), "fact" | "operational" | "conclusion") => {}
                Some(mt) => push(
                    issues,
                    Severity::Error,
                    codes::FM_BAD_META_TYPE,
                    rel,
                    fm_key_line_or_top(fm_yaml, "meta-type"),
                    Some("meta-type".into()),
                    format!("`meta-type: {mt}` is not one of fact / operational / conclusion"),
                    Some(
                        "use one of: fact, operational, conclusion (or omit for the default `fact`)"
                            .into(),
                    ),
                    vec![],
                ),
                None => push(
                    issues,
                    Severity::Error,
                    codes::FM_BAD_META_TYPE,
                    rel,
                    fm_key_line_or_top(fm_yaml, "meta-type"),
                    Some("meta-type".into()),
                    "`meta-type` is not one of fact / operational / conclusion: expected a scalar \
                     string, found a list or mapping"
                        .to_string(),
                    Some(
                        "use one of: fact, operational, conclusion (or omit for the default `fact`)"
                            .into(),
                    ),
                    vec![],
                ),
            }
        }
    }

    // ── summary (universal on content files) ──────────────────────────────────
    if is_content {
        check_summary(rel, fm, fm_yaml, issues);
    }

    // ── timestamps: created / updated ─────────────────────────────────────────
    // The `created`/`updated` contract is content-file-only; meta files
    // (`DB.md`, `log.md`, index twins) legitimately carry no such timestamps.
    if is_content {
        for (key, missing_code) in [
            ("created", codes::FM_MISSING_CREATED),
            ("updated", codes::FM_MISSING_UPDATED),
        ] {
            // A key that is absent, or present-but-`null`, has *no* timestamp →
            // `FM_MISSING_*`. The toolkit's parser also treats a null value as
            // "no timestamp", so a null `created:` must read as missing, not
            // silently pass.
            let value = fm.get(key);
            let missing = value.is_none() || value.is_some_and(Value::is_null);
            if missing {
                push(
                    issues,
                    Severity::Error,
                    missing_code,
                    rel,
                    fm_key_line_or_top(fm_yaml, key),
                    Some(key.into()),
                    format!("content file has no `{key}:` timestamp"),
                    Some(format!(
                        "set `{key}` to an RFC3339 timestamp, e.g. 2026-05-27T08:00:00-07:00"
                    )),
                    vec![],
                );
            } else if let Some(v) = value {
                // Present and non-null. A scalar is checked for ISO-8601; a
                // sequence/mapping is not a timestamp string at all and so
                // cannot be ISO-8601 → `FM_BAD_TIMESTAMP` (it must not slip
                // through the way it did when `scalar_string` returned `None`
                // and the branch silently no-oped).
                match scalar_string(v) {
                    Some(s) if is_iso8601(&s) => {}
                    Some(s) => push(
                        issues,
                        Severity::Error,
                        codes::FM_BAD_TIMESTAMP,
                        rel,
                        fm_key_line(fm_yaml, key),
                        Some(key.into()),
                        format!("`{key}` is not ISO-8601: {s:?}"),
                        Some("use RFC3339, e.g. 2026-05-27T08:00:00-07:00".into()),
                        vec![],
                    ),
                    None => push(
                        issues,
                        Severity::Error,
                        codes::FM_BAD_TIMESTAMP,
                        rel,
                        fm_key_line(fm_yaml, key),
                        Some(key.into()),
                        format!(
                            "`{key}` is not ISO-8601: expected a timestamp string, found a list or mapping"
                        ),
                        Some("use RFC3339, e.g. 2026-05-27T08:00:00-07:00".into()),
                        vec![],
                    ),
                }
            }
        }
    }
    // ── tags shape ────────────────────────────────────────────────────────────
    if let Some(tags) = fm.get("tags") {
        if !is_flat_scalar_list(tags) {
            push(
                issues,
                Severity::Warning,
                codes::TAGS_MALFORMED,
                rel,
                fm_key_line(fm_yaml, "tags"),
                Some("tags".into()),
                "`tags` must be a flat YAML list of short scalar labels".into(),
                Some("use block form: one `- <tag>` per line".into()),
                vec![],
            );
        }
    }

    // ── inline flow-form wiki-link lists in frontmatter ──────────────────────
    for key in detect_flow_form_link_lists(fm_yaml) {
        push(
            issues,
            Severity::Error,
            codes::WIKI_LINK_FLOW_FORM_LIST,
            rel,
            fm_key_line(fm_yaml, &key),
            Some(key.clone()),
            format!("`{key}` uses inline flow form `[[[a]], [[b]]]`"),
            Some("use YAML block-sequence form: one `- [[...]]` per line".into()),
            vec![],
        );
    }

    // ── frontmatter wiki-link fields: doctrine + integrity ───────────────────
    // Skip keys that have an explicit `link to` schema spec — those are checked
    // (with prefix enforcement) in `check_schema`, and double-reporting the same
    // link via two paths would be noise.
    let schema_link_keys: BTreeSet<String> =
        effective_schema(store, type_.as_deref().unwrap_or(""))
            .map(|s| {
                s.fields
                    .iter()
                    .filter(|f| f.link_prefix.is_some())
                    .map(|f| f.name.clone())
                    .collect()
            })
            .unwrap_or_default();
    for (key, link) in frontmatter_link_fields_text(fm_yaml, 2) {
        if schema_link_keys.contains(&key) {
            continue;
        }
        check_wiki_link(
            store,
            rel,
            &link,
            Some(link.line),
            Some(&key),
            basenames,
            issues,
        );
    }

    // ── policies: ignored types ──────────────────────────────────────────────
    if let Some(t) = &type_ {
        if store.config.ignored_types.iter().any(|it| it == t) {
            push(
                issues,
                Severity::Info,
                codes::POLICY_IGNORED_TYPE_PRESENT,
                rel,
                fm_key_line(fm_yaml, "type"),
                Some("type".into()),
                format!("file has ignored type `{t}` (per DB.md ## Policies)"),
                Some(
                    "change the `type`, or remove it from DB.md `### Ignored types` if it should be managed"
                        .into(),
                ),
                // The policy source: `DB.md` declares the ignored type.
                vec![PathBuf::from("DB.md")],
            );
        }
        // A conclusion record (`meta-type: conclusion`) deriving from an
        // ignored-type record → warning. The decision lives in the shared
        // `derived_from_ignored_type` entry point; this side only supplies the
        // `derived_from` targets (with their line, which the issue carries) and
        // renders the finding.
        let meta_type = fm
            .get("meta-type")
            .and_then(scalar_string)
            .unwrap_or_else(|| "fact".to_string());
        for link in frontmatter_links_for_key(fm_yaml, "derived_from", 2) {
            if let Some(hit) =
                derived_from_ignored_type(store, &meta_type, std::iter::once(link.target.as_str()))
            {
                push(
                    issues,
                    Severity::Warning,
                    codes::POLICY_IGNORED_TYPE_DERIVED,
                    rel,
                    Some(link.line),
                    Some("derived_from".into()),
                    format!(
                        "conclusion record derives from ignored-type record `{}` (type `{}`)",
                        hit.target, hit.target_type
                    ),
                    Some(
                        "drop this `derived_from` link, or remove the target type from DB.md `### Ignored types`"
                            .into(),
                    ),
                    // The ignored-type source record, plus `DB.md` (the policy
                    // source that lists the ignored type).
                    vec![
                        PathBuf::from(format!("{}.md", hit.target)),
                        PathBuf::from("DB.md"),
                    ],
                );
            }
        }
    }

    // ── schema enforcement: DB.md ## Schemas (the only schema source) ─────────
    if let Some(t) = &type_ {
        if let Some(schema) = effective_schema(store, t) {
            check_schema(store, rel, fm, fm_yaml, &schema, issues);
        }
    }
}

/// `summary` rules: required, non-empty, single-line, ≤ 200 chars.
fn check_summary(rel: &Path, fm: &BTreeMap<String, Value>, fm_yaml: &str, issues: &mut Vec<Issue>) {
    let line = fm_key_line(fm_yaml, "summary");
    match fm.get("summary") {
        None => push(
            issues,
            Severity::Error,
            codes::SUMMARY_MISSING,
            rel,
            // A missing `summary` key has no line of its own → anchor to the
            // frontmatter block top (line 1), the EXPECTED field-absence rule.
            fm_key_line_or_top(fm_yaml, "summary"),
            Some("summary".into()),
            "content file has no `summary`".into(),
            Some("run `dbmd fm init`".into()),
            vec![],
        ),
        Some(v) => {
            let s = scalar_string(v).unwrap_or_default();
            if s.trim().is_empty() {
                push(
                    issues,
                    Severity::Error,
                    codes::SUMMARY_EMPTY,
                    rel,
                    line,
                    Some("summary".into()),
                    "`summary` is present but empty".into(),
                    Some("write a one-line summary, or run `dbmd fm init`".into()),
                    vec![],
                );
            } else if s.contains('\n') {
                push(
                    issues,
                    Severity::Error,
                    codes::SUMMARY_MULTILINE,
                    rel,
                    line,
                    Some("summary".into()),
                    "`summary` must be one line (contains a newline)".into(),
                    Some("collapse the summary to a single line".into()),
                    vec![],
                );
            } else if s.chars().count() > MAX_SUMMARY_LEN {
                push(
                    issues,
                    Severity::Warning,
                    codes::SUMMARY_TOO_LONG,
                    rel,
                    line,
                    Some("summary".into()),
                    format!(
                        "`summary` is {} chars (> {MAX_SUMMARY_LEN})",
                        s.chars().count()
                    ),
                    Some(format!("trim the summary to ≤ {MAX_SUMMARY_LEN} chars")),
                    vec![],
                );
            }
        }
    }
}

/// Wiki-link checks for a body. Per-link doctrine (`WIKI_LINK_*`).
fn check_body_wiki_links(
    store: &Store,
    rel: &Path,
    body: &str,
    fm_end_line: u32,
    basenames: Option<&BasenameIndex>,
    issues: &mut Vec<Issue>,
) {
    for link in extract_wiki_links(body) {
        // Body lines are offset past the frontmatter block. `link.line` is
        // 1-based within `body`; the body starts at `fm_end_line + 1`.
        let abs_line = fm_end_line + link.line;
        check_wiki_link(store, rel, &link, Some(abs_line), None, basenames, issues);
    }
}

/// A store-wide map from a file's bare basename (its stem, no `.md`) to every
/// store-relative path carrying that basename. Built once per `validate --all`
/// sweep so the short-form wiki-link check can distinguish a merely short-form
/// target (`WIKI_LINK_SHORT_FORM`) from one that is *ambiguous* because the bare
/// basename matches two or more files (`WIKI_LINK_AMBIGUOUS`, the defensive
/// code). `None` in the working-set path — that loop is O(changed) and never
/// walks the store, so it reports the plain short-form error without the scan.
type BasenameIndex = HashMap<String, Vec<PathBuf>>;

/// Build the [`BasenameIndex`] from the swept file list (already gathered by
/// `validate_all`; no extra walk).
fn build_basename_index(files: &[PathBuf]) -> BasenameIndex {
    let mut idx: BasenameIndex = HashMap::new();
    for rel in files {
        if let Some(stem) = rel.file_stem().and_then(|s| s.to_str()) {
            idx.entry(stem.to_string()).or_default().push(rel.clone());
        }
    }
    idx
}

/// The shared per-wiki-link doctrine + integrity check used by both body links
/// and frontmatter link-fields. `basenames` is `Some` only in the `--all`
/// sweep, where a no-slash short-form target is upgraded to `WIKI_LINK_AMBIGUOUS`
/// when its bare basename matches ≥2 files.
fn check_wiki_link(
    store: &Store,
    rel: &Path,
    link: &Link,
    line: Option<u32>,
    key: Option<&str>,
    basenames: Option<&BasenameIndex>,
    issues: &mut Vec<Issue>,
) {
    let bare = link.target.trim_end_matches(".md");

    // Short-form: not a full store-relative path (no `/`, or first segment isn't
    // a known layer).
    if !is_full_store_path(bare) {
        // Ambiguous (defensive) takes precedence over plain short-form when the
        // target is a bare basename (no `/`) that matches ≥2 files in the store.
        // Only computable in the sweep (where `basenames` is populated); the
        // working-set path falls through to the plain short-form error.
        if !bare.contains('/') {
            if let Some(idx) = basenames {
                if let Some(matches) = idx.get(bare) {
                    if matches.len() >= 2 {
                        let mut related = matches.clone();
                        related.sort();
                        push(
                            issues,
                            Severity::Error,
                            codes::WIKI_LINK_AMBIGUOUS,
                            rel,
                            line,
                            key.map(str::to_string),
                            format!(
                                "short-form wiki-link `[[{}]]` matches multiple files",
                                link.target
                            ),
                            Some("use the full store-relative path to disambiguate".into()),
                            related,
                        );
                        return;
                    }
                }
            }
        }
        push(
            issues,
            Severity::Error,
            codes::WIKI_LINK_SHORT_FORM,
            rel,
            line,
            key.map(str::to_string),
            format!(
                "wiki-link `[[{}]]` is not a full store-relative path",
                link.target
            ),
            short_form_suggestion(bare),
            vec![],
        );
        // Don't also report broken; the agent must fix the form first.
        return;
    }

    // `.md` extension → warning, then still check existence.
    if link.target.ends_with(".md") {
        push(
            issues,
            Severity::Warning,
            codes::WIKI_LINK_HAS_EXTENSION,
            rel,
            line,
            key.map(str::to_string),
            format!("wiki-link `[[{}]]` carries a `.md` extension", link.target),
            Some(format!("drop the extension: [[{bare}]]")),
            vec![],
        );
    }

    // Broken: target file doesn't exist (O(1) stat). Resolve the target the
    // same way the graph engine does — the literal path first (so a link to a
    // raw `.eml`/`.pdf` source kept verbatim under `sources/` resolves), then
    // the `.md`-appended path.
    match resolve_wiki_target(store, bare) {
        TargetResolution::Exists => {}
        TargetResolution::Missing => push(
            issues,
            Severity::Error,
            codes::WIKI_LINK_BROKEN,
            rel,
            line,
            key.map(str::to_string),
            format!("wiki-link target `{bare}` doesn't exist"),
            Some(format!(
                "create `{bare}.md`, or point the link at an existing file"
            )),
            vec![],
        ),
        TargetResolution::Unsafe => push(
            issues,
            Severity::Error,
            codes::WIKI_LINK_BROKEN,
            rel,
            line,
            key.map(str::to_string),
            format!("wiki-link target `{bare}` is not a safe store-relative path"),
            Some("use a full store-relative path under sources/ or records/".into()),
            vec![],
        ),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Schema enforcement (user-declared DB.md ## Schemas — the only source)
// ─────────────────────────────────────────────────────────────────────────────

/// The effective schema for a type: the store's explicit `DB.md ## Schemas`
/// block, or `None`. This is the **only** source of schema enforcement — the
/// toolkit ships no implicit or built-in per-type schema (SPEC § Schemas). A
/// store that wants its `contact` / `expense` / etc. fields enforced declares
/// them in `## Schemas`; the example schema pack in SPEC § Example types is a
/// copy-in starting point.
fn effective_schema(store: &Store, type_: &str) -> Option<Schema> {
    store.config.schemas.get(type_).cloned()
}

/// Validate a file's frontmatter against a schema's [`FieldSpec`]s.
fn check_schema(
    store: &Store,
    rel: &Path,
    fm: &BTreeMap<String, Value>,
    fm_yaml: &str,
    schema: &Schema,
    issues: &mut Vec<Issue>,
) {
    for spec in &schema.fields {
        let present = fm.get(&spec.name);
        let line = fm_key_line(fm_yaml, &spec.name);

        // Required. "Empty" means: the key is absent, or its value carries no
        // content — a YAML `null` (`name:`), an empty list (`name: []`), an
        // empty mapping (`name: {}`), or a blank/whitespace-only scalar
        // (`name: ""`). `scalar_string` returns `None` for null/list/mapping, so
        // a bare `.unwrap_or(false)` wrongly treated those as non-empty and let
        // a required field with a null or empty-collection value pass silently;
        // route them through `is_empty_value` instead.
        let is_empty = match present {
            None => true,
            Some(v) => is_empty_value(v),
        };
        if spec.required && is_empty {
            push(
                issues,
                Severity::Error,
                codes::SCHEMA_MISSING_REQUIRED,
                rel,
                // Absent key → anchor to the frontmatter top (line 1); a
                // present-but-empty value keeps its own line.
                fm_key_line_or_top(fm_yaml, &spec.name),
                Some(spec.name.clone()),
                format!("required field `{}` is absent or empty", spec.name),
                Some(format!("set `{}` to a non-empty value", spec.name)),
                vec![],
            );
            continue;
        }
        let Some(value) = present else { continue };

        // An OPTIONAL field that is `null` or empty is simply unset — there is
        // no value to shape/enum/link-check. (The required+empty case already
        // returned above as `SCHEMA_MISSING_REQUIRED`.) Without this, an
        // `paid_at: null` on an `invoice` whose schema marks `paid_at (date)`
        // would wrongly fire `SCHEMA_SHAPE_MISMATCH` against the empty string.
        let value_empty = value.is_null()
            || scalar_string(value)
                .map(|s| s.trim().is_empty())
                .unwrap_or(false);
        if !spec.required && value_empty {
            continue;
        }

        // link to <prefix>/ — extract the link target(s) from the raw frontmatter
        // text (unquoted `[[...]]` is a YAML nested-sequence, not a string).
        if let Some(prefix) = &spec.link_prefix {
            check_schema_link(store, rel, &spec.name, fm_yaml, prefix, line, issues);
            continue; // a link field is never also shape/enum-checked
        }

        // A shape- or enum-constrained field expects a SCALAR. A YAML sequence
        // or mapping satisfies neither, and would otherwise slip through both
        // checks (`scalar_string` returns `None` for non-scalars, so the enum
        // and shape bodies silently no-op). Flag it as a shape mismatch rather
        // than let a structurally-wrong value validate clean. (Link fields,
        // which legitimately take block-form sequences, already `continue`d.)
        if (spec.shape.is_some() || spec.enum_values.is_some()) && scalar_string(value).is_none() {
            push(
                issues,
                Severity::Error,
                codes::SCHEMA_SHAPE_MISMATCH,
                rel,
                line,
                Some(spec.name.clone()),
                format!(
                    "`{}` must be a scalar value, found a list or mapping",
                    spec.name
                ),
                Some(format!("set `{}` to a single scalar value", spec.name)),
                vec![],
            );
            continue;
        }

        // enum
        if let Some(allowed) = &spec.enum_values {
            if let Some(s) = scalar_string(value) {
                if !allowed.iter().any(|a| a == &s) {
                    push(
                        issues,
                        Severity::Error,
                        codes::SCHEMA_ENUM_VIOLATION,
                        rel,
                        line,
                        Some(spec.name.clone()),
                        format!("`{}` value {s:?} not in enum {allowed:?}", spec.name),
                        Some(format!("use one of: {}", allowed.join(", "))),
                        vec![],
                    );
                }
            }
            continue;
        }

        // shape
        if let Some(shape) = spec.shape {
            check_schema_shape(rel, &spec.name, value, shape, line, issues);
        }
    }
}

/// `link to <prefix>/` enforcement: the value must be a wiki-link whose target
/// starts with `<prefix>`. Reads the link target(s) from the raw frontmatter
/// text so unquoted `field: [[...]]` (a YAML nested-sequence, not a string) is
/// recognized exactly like the quoted form.
fn check_schema_link(
    store: &Store,
    rel: &Path,
    field: &str,
    fm_yaml: &str,
    prefix: &Path,
    line: Option<u32>,
    issues: &mut Vec<Issue>,
) {
    let prefix_str = prefix.to_string_lossy();
    let prefix_str = prefix_str.trim_end_matches('/');
    let suggestion = |target_leaf: &str| {
        Some(format!(
            "expected `link to {prefix_str}/`; replace with [[{prefix_str}/{target_leaf}]]"
        ))
    };

    let links = frontmatter_links_for_key(fm_yaml, field, 2);
    if links.is_empty() {
        // No wiki-link in the field's value → it's a plain string.
        let raw = frontmatter_raw_value_for_key(fm_yaml, field, 2).unwrap_or_default();
        let raw = raw.trim().trim_matches('"').trim_matches('\'').trim();
        let leaf = slugish(raw);
        push(
            issues,
            Severity::Error,
            codes::SCHEMA_LINK_PREFIX_MISMATCH,
            rel,
            line,
            Some(field.to_string()),
            format!(
                "`{field}` is a plain string {raw:?}, expected a wiki-link under `{prefix_str}/`"
            ),
            suggestion(&leaf),
            vec![],
        );
        return;
    }

    for link in links {
        if link.target.ends_with(".md") {
            let bare = link.target.trim_end_matches(".md");
            push(
                issues,
                Severity::Warning,
                codes::WIKI_LINK_HAS_EXTENSION,
                rel,
                Some(link.line),
                Some(field.to_string()),
                format!("wiki-link `[[{}]]` carries a `.md` extension", link.target),
                Some(format!("drop the extension: [[{bare}]]")),
                vec![],
            );
        }
        let bare = link.target.trim_end_matches(".md");
        if !path_under_prefix(bare, prefix_str) {
            let leaf = bare.rsplit('/').next().unwrap_or(bare);
            push(
                issues,
                Severity::Error,
                codes::SCHEMA_LINK_PREFIX_MISMATCH,
                rel,
                line,
                Some(field.to_string()),
                format!("`{field}` target `{bare}` is not under `{prefix_str}/`"),
                suggestion(leaf),
                vec![],
            );
        } else {
            // Correct prefix — still surface a broken target so the agent sees
            // one consistent vocabulary. Resolve like the graph engine (literal
            // path first, then `.md`) so a `link to sources/` field pointing at a
            // raw `.eml`/`.pdf` source isn't wrongly flagged broken.
            match resolve_wiki_target(store, bare) {
                TargetResolution::Exists => {}
                TargetResolution::Missing => push(
                    issues,
                    Severity::Error,
                    codes::WIKI_LINK_BROKEN,
                    rel,
                    line,
                    Some(field.to_string()),
                    format!("wiki-link target `{bare}` doesn't exist"),
                    Some(format!(
                        "create `{bare}.md`, or point the link at an existing file"
                    )),
                    vec![],
                ),
                TargetResolution::Unsafe => push(
                    issues,
                    Severity::Error,
                    codes::WIKI_LINK_BROKEN,
                    rel,
                    line,
                    Some(field.to_string()),
                    format!("wiki-link target `{bare}` is not a safe store-relative path"),
                    Some("use a full store-relative path under sources/ or records/".into()),
                    vec![],
                ),
            }
        }
    }
}

/// Shape enforcement for a non-link, non-enum schema field.
fn check_schema_shape(
    rel: &Path,
    field: &str,
    value: &Value,
    shape: Shape,
    line: Option<u32>,
    issues: &mut Vec<Issue>,
) {
    let s = scalar_string(value).unwrap_or_default();
    let ok = match shape {
        Shape::String => true, // any scalar string
        Shape::Int => value.is_i64() || value.is_u64() || s.trim().parse::<i64>().is_ok(),
        Shape::Bool => value.is_bool() || matches!(s.trim(), "true" | "false"),
        Shape::Date => is_iso8601_date_or_datetime(&s),
        Shape::Email => is_email(&s),
        Shape::Currency => is_currency(&s),
        Shape::Url => is_url(&s),
    };
    if !ok {
        push(
            issues,
            Severity::Error,
            codes::SCHEMA_SHAPE_MISMATCH,
            rel,
            line,
            Some(field.to_string()),
            format!("`{field}` value {s:?} doesn't match shape {shape:?}"),
            Some(shape_suggestion(shape)),
            vec![],
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Cross-file: entity-dedup collisions (validate_all only)
// ─────────────────────────────────────────────────────────────────────────────

/// Hard `DUP_ID` + the soft, schema-declared `DUP_UNIQUE_KEY` collisions.
///
/// `DUP_ID` is universal (two files with the same explicit `id`).
/// `DUP_UNIQUE_KEY` is driven entirely by the store's `DB.md ## Schemas`: each
/// `- unique: <field>[, <field> …]` directive on a `### <type>` declares a
/// uniqueness constraint, and two records of that type whose declared values
/// collide warn. No type carries a built-in dedup key — the store opts in.
///
/// **Reporting precedence (rule #1 in `corpus-b-edges/EXPECTED/README.md`):** a
/// collision group of N files yields exactly ONE issue, not N. Its `file` is the
/// lexicographically smallest store-relative path in the group (a total order →
/// deterministic); `related` is the rest, sorted. A single-field key anchors to
/// that field's line on the reported file and carries it as `key`; a multi-field
/// key anchors to line 1 with a null key.
fn check_duplicates(store: &Store, parsed: &[(PathBuf, Parsed)], issues: &mut Vec<Issue>) {
    // Path → frontmatter YAML, for resolving the anchor field's line on the
    // reported (smallest-path) member.
    let fm_yaml_of: HashMap<&PathBuf, &str> = parsed
        .iter()
        .map(|(rel, p)| (rel, p.fm_yaml.as_str()))
        .collect();

    // ── DUP_ID (hard error): two files with the same explicit `id`. ──────────
    let mut by_id: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (rel, p) in parsed {
        if let Some(map) = &p.fm {
            if let Some(id) = map.get("id").and_then(scalar_string) {
                if !id.trim().is_empty() {
                    by_id.entry(id).or_default().push(rel.clone());
                }
            }
        }
    }
    for (id, files) in &by_id {
        if files.len() > 1 {
            let (reported, related) = canonical_and_related(files);
            let line = fm_yaml_of.get(&reported).and_then(|y| fm_key_line(y, "id"));
            push(
                issues,
                Severity::Error,
                codes::DUP_ID,
                &reported,
                line,
                Some("id".into()),
                format!("id {id:?} is declared by more than one file"),
                Some("give each file a unique `id` (or drop it to derive from the path)".into()),
                related,
            );
        }
    }

    // ── DUP_UNIQUE_KEY (warning): schema-declared `unique:` collisions. ───────
    // Every constraint comes from the store's `## Schemas`; a type with no
    // `unique:` directive is never dedup-checked. Iteration over the BTreeMap is
    // key-ordered, so emitted issues are deterministic across runs.
    for (type_name, schema) in &store.config.schemas {
        for key_fields in &schema.unique_keys {
            soft_dup(parsed, issues, type_name, key_fields, &fm_yaml_of);
        }
    }
}

/// Emit ONE `DUP_UNIQUE_KEY` warning per group of ≥2 files of `type_` whose
/// declared `key_fields` render to the same token tuple. Files missing any key
/// field are skipped — an incomplete key is never a collision.
///
/// Per reporting rule #1 the issue is keyed on the lexicographically smallest
/// store-relative path; `related` is the rest. A single-field key anchors to
/// that field's line on the reported file and carries it as `key`; a multi-field
/// key anchors to line 1 with a null key. `fm_yaml_of` resolves the field line.
fn soft_dup(
    parsed: &[(PathBuf, Parsed)],
    issues: &mut Vec<Issue>,
    type_: &str,
    key_fields: &[String],
    fm_yaml_of: &HashMap<&PathBuf, &str>,
) {
    if key_fields.is_empty() {
        return;
    }
    let mut groups: HashMap<Vec<String>, Vec<PathBuf>> = HashMap::new();
    for (rel, p) in parsed {
        let is_type =
            p.fm.as_ref()
                .and_then(|m| m.get("type"))
                .and_then(scalar_string)
                .map(|t| t == type_)
                .unwrap_or(false);
        if !is_type {
            continue;
        }
        if let Some(key) = dedup_key(p, key_fields) {
            groups.entry(key).or_default().push(rel.clone());
        }
    }
    // HashMap iteration is nondeterministic; sort by reported member so the
    // emitted issue order is stable across runs.
    let mut collisions: Vec<(PathBuf, Vec<PathBuf>)> = groups
        .values()
        .filter(|files| files.len() > 1)
        .map(|files| canonical_and_related(files))
        .collect();
    collisions.sort_by(|a, b| a.0.cmp(&b.0));

    let fields_disp = key_fields.join(", ");
    for (reported, related) in collisions {
        // Single-field keys anchor to the field's line + carry the key; multi-
        // field keys anchor to line 1 with a null key.
        let (line, key) = if key_fields.len() == 1 {
            (
                fm_yaml_of
                    .get(&reported)
                    .and_then(|y| fm_key_line(y, &key_fields[0])),
                Some(key_fields[0].clone()),
            )
        } else {
            (Some(1), None)
        };
        let n = related.len();
        push(
            issues,
            Severity::Warning,
            codes::DUP_UNIQUE_KEY,
            &reported,
            line,
            key,
            format!("`{type_}` unique key ({fields_disp}) collides with {n} other record(s)"),
            Some("merge with `dbmd rename`, or cross-link with `dbmd link`".into()),
            related,
        );
    }
}

/// Render a type's `unique:` key for one file: each field's dedup token in
/// order, or `None` if any field is absent/empty (an incomplete key never
/// collides).
fn dedup_key(p: &Parsed, key_fields: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(key_fields.len());
    for f in key_fields {
        out.push(dedup_token(p, f)?);
    }
    Some(out)
}

/// One field's normalized dedup token, or `None` when absent/empty. Wiki-link
/// values (single or block-sequence list) reduce to their lower-cased target
/// path(s); a list collapses to a sorted, de-duplicated set so item order never
/// matters. Plain scalars (and YAML scalar lists) lower-case and trim.
fn dedup_token(p: &Parsed, field: &str) -> Option<String> {
    // Wiki-links first — read from the raw frontmatter text so the unquoted
    // `field: [[...]]` (a YAML nested-sequence, not a string) is handled.
    let links = frontmatter_links_for_key(&p.fm_yaml, field, 2);
    if !links.is_empty() {
        let set: BTreeSet<String> = links
            .into_iter()
            .map(|l| l.target.trim_end_matches(".md").to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        return if set.is_empty() {
            None
        } else {
            Some(set.into_iter().collect::<Vec<_>>().join(","))
        };
    }
    match p.fm.as_ref()?.get(field) {
        Some(Value::Sequence(items)) => {
            let set: BTreeSet<String> = items
                .iter()
                .filter_map(scalar_string)
                .map(|s| s.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .collect();
            if set.is_empty() {
                None
            } else {
                Some(set.into_iter().collect::<Vec<_>>().join(","))
            }
        }
        Some(v) => {
            let s = scalar_string(v)?.trim().to_lowercase();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        None => None,
    }
}

/// Split a non-empty collision group into `(reported, related)`: the
/// lexicographically smallest store-relative path is the reported member; the
/// rest, sorted ascending, are `related`. Deterministic because store-relative
/// path is a total order — the property reporting rule #1 relies on.
fn canonical_and_related(files: &[PathBuf]) -> (PathBuf, Vec<PathBuf>) {
    let mut sorted = files.to_vec();
    sorted.sort();
    let reported = sorted[0].clone();
    let related = sorted[1..].to_vec();
    (reported, related)
}

// ─────────────────────────────────────────────────────────────────────────────
//  Cross-file: hierarchical index.md + index.jsonl sync (validate_all only)
// ─────────────────────────────────────────────────────────────────────────────

/// All `INDEX_*` and `INDEX_JSONL_*` checks across the three canonical levels.
fn check_indexes(store: &Store, files: &[PathBuf], issues: &mut Vec<Issue>) {
    // Group content files by their immediate parent folder (the type-folder,
    // *across date shards* — a sharded file's "type folder" is the folder right
    // under the layer). We key on the type-folder so shards roll up correctly.
    let mut type_folders: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    let mut layers_present: BTreeSet<&'static str> = BTreeSet::new();
    for rel in files {
        // The layer is the first path component — recorded independently of the
        // type-folder so a layer containing only loose files still requires an
        // `index.md`.
        if let Some(layer) = rel.iter().next().and_then(|s| s.to_str()) {
            match layer {
                "sources" => layers_present.insert("sources"),
                "records" => layers_present.insert("records"),
                _ => false,
            };
        }
        if let Some(tf) = type_folder_of(rel) {
            type_folders.entry(tf).or_default().push(rel.clone());
        }
    }

    // ── Root index.md ─────────────────────────────────────────────────────────
    // The root `index.md` is a TYPE-FOLDER rollup, so it is required only when
    // the store has type-folder content. A store whose only content is loose
    // files (directly at a layer root) is catalogued by its layer `index.jsonl`
    // and has nothing to roll up, so the absence of a root `index.md` is not a
    // defect — but if one exists, scope-check it.
    {
        let root_index = store.root.join("index.md");
        if root_index.is_file() {
            check_index_scope(store, Path::new("index.md"), "root", None, issues);
        } else if !type_folders.is_empty() {
            push(
                issues,
                Severity::Error,
                codes::INDEX_MISSING,
                Path::new("index.md"),
                None,
                None,
                "store has files but no root `index.md`".into(),
                Some("run `dbmd index rebuild`".into()),
                vec![],
            );
        }
    }

    // ── Layer index.md ────────────────────────────────────────────────────────
    // A layer `index.md` is the rollup of that layer's type-folders, so it is
    // required only when the layer HAS type-folders. A layer whose only content
    // is loose files is catalogued by its own `index.jsonl` (checked below) and
    // needs no rollup; demanding one there was a false `INDEX_MISSING`.
    for layer in &layers_present {
        let layer_index_rel = PathBuf::from(layer).join("index.md");
        let abs = store.root.join(&layer_index_rel);
        let layer_has_type_folders = type_folders.keys().any(|tf| tf.starts_with(layer));
        if abs.is_file() {
            check_index_scope(store, &layer_index_rel, "layer", Some(layer), issues);
        } else if layer_has_type_folders {
            push(
                issues,
                Severity::Error,
                codes::INDEX_MISSING,
                &layer_index_rel,
                None,
                None,
                format!("layer `{layer}/` has files but no `index.md`"),
                Some("run `dbmd index rebuild`".into()),
                vec![],
            );
        }
    }

    // ── Type-folder index.md + index.jsonl ───────────────────────────────────
    for (tf, members) in &type_folders {
        let index_md_rel = tf.join("index.md");
        let index_md_abs = store.root.join(&index_md_rel);
        let index_md_present = index_md_abs.is_file();
        if !index_md_present {
            // The whole folder index is absent → a single `INDEX_MISSING` keyed
            // on the FOLDER (not the would-be `index.md` path). When the index is
            // entirely missing we do NOT additionally evaluate per-entry
            // completeness or the `index.jsonl` twin: one `INDEX_MISSING` covers
            // the folder (precedence rule #4 in `corpus-b-edges/EXPECTED`).
            push(
                issues,
                Severity::Error,
                codes::INDEX_MISSING,
                tf,
                None,
                None,
                format!("non-empty folder `{}` has no index.md", tf.display()),
                Some(format!(
                    "run `dbmd index rebuild --folder {}`",
                    tf.display()
                )),
                vec![],
            );
            continue;
        }

        check_index_scope(store, &index_md_rel, "type-folder", tf.to_str(), issues);
        check_type_folder_index_md(store, tf, &index_md_rel, members, issues);

        // index.jsonl twin — must exist and be complete (uncapped). Only checked
        // when the `index.md` is present (above): a folder whose entire index is
        // missing is one `INDEX_MISSING`, not also an `INDEX_JSONL_MISSING`.
        let jsonl_rel = tf.join("index.jsonl");
        let jsonl_abs = store.root.join(&jsonl_rel);
        if !jsonl_abs.is_file() {
            push(
                issues,
                Severity::Error,
                codes::INDEX_JSONL_MISSING,
                &jsonl_rel,
                None,
                None,
                format!("type-folder `{}/` has no `index.jsonl` twin", tf.display()),
                Some("run `dbmd index rebuild`".into()),
                vec![],
            );
        } else {
            check_type_folder_index_jsonl(store, tf, &jsonl_rel, members, issues);
        }
    }

    // ── Loose files: content directly at a layer root (no type-folder). ──────
    // They are catalogued in the layer's own `index.jsonl` (the layer `index.md`
    // stays a type-folder rollup), so structured reads — `query`, dedup, `graph`
    // — see them the same way they see canonical files. Require that sidecar and
    // sync-check it, so a loose file is never silently absent from the catalog.
    // Only genuinely-loose files land here: `type_folder_of` already grouped
    // every file two-or-more levels under a layer into its type-folder above.
    let mut loose_by_layer: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for rel in files {
        if !is_content_file(rel) || type_folder_of(rel).is_some() {
            continue;
        }
        if let Some(layer_dir) = loose_layer_dir(rel) {
            loose_by_layer
                .entry(layer_dir)
                .or_default()
                .push(rel.clone());
        }
    }
    for (layer_dir, members) in &loose_by_layer {
        let jsonl_rel = layer_dir.join("index.jsonl");
        if !store.root.join(&jsonl_rel).is_file() {
            push(
                issues,
                Severity::Error,
                codes::INDEX_JSONL_MISSING,
                &jsonl_rel,
                None,
                None,
                format!(
                    "loose files at `{}/` are not catalogued — the layer has no `index.jsonl`",
                    layer_dir.display()
                ),
                Some("run `dbmd index rebuild`".into()),
                members.clone(),
            );
        } else {
            // `check_type_folder_index_jsonl` ignores its `tf` arg (`let _ = tf`)
            // and only checks jsonl-vs-files-vs-frontmatter — exactly the layer
            // sidecar's contract, so it is reused verbatim.
            check_type_folder_index_jsonl(store, layer_dir, &jsonl_rel, members, issues);
        }
    }

    // ── Orphan index.md: an index file in a folder with no content. ──────────
    for rel in walk_index_files(&store.root) {
        let parent = rel.parent().unwrap_or(Path::new("")).to_path_buf();
        let parent_str = parent.to_string_lossy().to_string();
        let is_canonical = parent_str.is_empty() // root
            || matches!(parent_str.as_str(), "sources" | "records")
            || type_folders.contains_key(&parent);
        if !is_canonical {
            push(
                issues,
                Severity::Warning,
                codes::INDEX_ORPHAN,
                &rel,
                None,
                None,
                format!(
                    "`{}` sits in an empty or non-canonical folder",
                    rel.display()
                ),
                Some("remove it, or run `dbmd index rebuild`".into()),
                vec![],
            );
        }
    }
}

/// Check a type-folder `index.md`'s entries against the folder's actual files:
/// stale entries (target gone), missing entries (file not listed), and
/// summary mismatches.
fn check_type_folder_index_md(
    store: &Store,
    tf: &Path,
    index_rel: &Path,
    members: &[PathBuf],
    issues: &mut Vec<Issue>,
) {
    let abs = store.root.join(index_rel);
    let Ok(text) = std::fs::read_to_string(&abs) else {
        return;
    };
    let entries = parse_index_entries(&text);

    let listed: BTreeSet<PathBuf> = entries
        .iter()
        .map(|e| PathBuf::from(e.target.trim_end_matches(".md")))
        .collect();

    // Stale entries + summary mismatch.
    for entry in &entries {
        let bare = entry.target.trim_end_matches(".md");
        // Resolve like the graph engine (literal path first, then `.md`) so an
        // index entry naming a raw `.eml`/`.pdf` source isn't reported stale.
        let target_abs = match resolved_target_abs(store, bare) {
            Some(abs) => abs,
            None => {
                if matches!(resolve_wiki_target(store, bare), TargetResolution::Unsafe) {
                    push(
                        issues,
                        Severity::Error,
                        codes::INDEX_STALE_ENTRY,
                        index_rel,
                        Some(entry.line),
                        None,
                        format!("index entry `[[{bare}]]` is not a safe store-relative path"),
                        Some("run `dbmd index rebuild`".into()),
                        vec![],
                    );
                } else {
                    push(
                        issues,
                        Severity::Error,
                        codes::INDEX_STALE_ENTRY,
                        index_rel,
                        Some(entry.line),
                        None,
                        format!("index entry `[[{bare}]]` points at a missing file"),
                        Some("run `dbmd index rebuild`".into()),
                        // The stale target the entry names (the file that no
                        // longer exists) — so the agent can locate the dangling
                        // reference.
                        vec![PathBuf::from(format!("{bare}.md"))],
                    );
                }
                continue;
            }
        };
        // Summary mismatch: the entry text must equal the file's `summary`. A
        // bare `- [[path]]` entry (no `— <text>`) when the file HAS a non-empty
        // summary is also a mismatch — the SPEC requires every type-folder index
        // entry to quote the file's `summary` (`- [[path]] — <summary>`), so a
        // missing quote can't validate clean just because there's nothing to
        // compare.
        if let Some(expected) = read_summary(&target_abs) {
            match &entry.summary_text {
                // Compare with the SAME whitespace normalization the renderer
                // applies when it writes the `index.md` browse line
                // (`format_md_entry` -> `collapse_whitespace`). `text_part` is the
                // already-collapsed text parsed back out of `index.md`; `expected`
                // is the RAW file summary. Comparing a collapsed value against a
                // raw one falsely flagged any valid one-line summary that carries
                // internal whitespace (a double space, a tab) — a permanent,
                // rebuild-immune INDEX_SUMMARY_MISMATCH that wedged the store, since
                // `index rebuild` regenerates the byte-identical collapsed line.
                // Normalizing both sides makes the check compare like with like.
                Some(text_part)
                    if crate::summary::collapse_whitespace(text_part)
                        != crate::summary::collapse_whitespace(&expected) =>
                {
                    push(
                        issues,
                        Severity::Error,
                        codes::INDEX_SUMMARY_MISMATCH,
                        index_rel,
                        Some(entry.line),
                        None,
                        format!("index entry for `{bare}` text doesn't match the file's `summary`"),
                        Some("run `dbmd index rebuild`".into()),
                        vec![PathBuf::from(format!("{bare}.md"))],
                    );
                }
                None if !expected.trim().is_empty() => {
                    push(
                        issues,
                        Severity::Error,
                        codes::INDEX_SUMMARY_MISMATCH,
                        index_rel,
                        Some(entry.line),
                        None,
                        format!("index entry for `{bare}` is missing its summary text (the file has a `summary`)"),
                        Some("run `dbmd index rebuild`".into()),
                        vec![PathBuf::from(format!("{bare}.md"))],
                    );
                }
                _ => {}
            }
        }
    }

    // Missing entries: a member file not listed. Skip the index/log meta files.
    // The browse view caps at 500; only flag a missing entry when the folder is
    // under the cap (a capped folder legitimately omits older files).
    let content_members: Vec<&PathBuf> = members.iter().filter(|m| is_content_file(m)).collect();
    if content_members.len() <= 500 {
        for m in content_members {
            let bare = PathBuf::from(m.to_string_lossy().trim_end_matches(".md").to_string());
            if !listed.contains(&bare) {
                push(
                    issues,
                    Severity::Error,
                    codes::INDEX_MISSING_ENTRY,
                    index_rel,
                    None,
                    None,
                    format!(
                        "file `{}` is not listed in its folder's `index.md`",
                        m.display()
                    ),
                    Some("run `dbmd index rebuild`".into()),
                    vec![(*m).clone()],
                );
            }
        }
    }
    let _ = tf;
}

/// Check a type-folder `index.jsonl` twin: it must list **every** file in the
/// folder (uncapped), every record must point at a real file, and each record's
/// fields must match the file's frontmatter.
fn check_type_folder_index_jsonl(
    store: &Store,
    tf: &Path,
    jsonl_rel: &Path,
    members: &[PathBuf],
    issues: &mut Vec<Issue>,
) {
    let abs = store.root.join(jsonl_rel);
    let Ok(text) = std::fs::read_to_string(&abs) else {
        return;
    };

    // Parse records (last-write-wins by path), tolerating tombstones/blank lines.
    let mut records: BTreeMap<PathBuf, serde_json::Value> = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                push(
                    issues,
                    Severity::Error,
                    codes::INDEX_JSONL_DESYNC,
                    jsonl_rel,
                    Some((i + 1) as u32),
                    None,
                    format!("`index.jsonl` line {} is not valid JSON: {e}", i + 1),
                    Some("run `dbmd index rebuild`".into()),
                    vec![],
                );
                continue;
            }
        };
        if let Some(path) = rec.get("path").and_then(|v| v.as_str()) {
            if !is_safe_store_relative_path(Path::new(path)) {
                push(
                    issues,
                    Severity::Error,
                    codes::INDEX_JSONL_DESYNC,
                    jsonl_rel,
                    Some((i + 1) as u32),
                    None,
                    format!("`index.jsonl` record path `{path}` is not a safe store-relative path"),
                    Some("run `dbmd index rebuild`".into()),
                    vec![],
                );
                continue;
            }
            records.insert(PathBuf::from(path), rec);
        }
    }

    let member_set: BTreeSet<PathBuf> = members
        .iter()
        .filter(|m| is_content_file(m))
        .cloned()
        .collect();

    // jsonl record → missing file = desync.
    for path in records.keys() {
        let target_abs = store.root.join(path);
        if !target_abs.is_file() {
            push(
                issues,
                Severity::Error,
                codes::INDEX_JSONL_DESYNC,
                jsonl_rel,
                None,
                None,
                format!(
                    "`index.jsonl` record points at missing file `{}`",
                    path.display()
                ),
                Some("run `dbmd index rebuild`".into()),
                vec![],
            );
        }
    }

    // file not in jsonl = desync (the jsonl is the complete twin — no cap).
    for m in &member_set {
        if !records.contains_key(m) {
            push(
                issues,
                Severity::Error,
                codes::INDEX_JSONL_DESYNC,
                jsonl_rel,
                None,
                None,
                format!(
                    "file `{}` is missing from the complete `index.jsonl`",
                    m.display()
                ),
                Some("run `dbmd index rebuild`".into()),
                vec![m.clone()],
            );
        }
    }

    // Record fields stale vs. frontmatter. SPEC § Validation defines
    // `INDEX_JSONL_STALE` as "an `index.jsonl` record's fields don't match the
    // file's frontmatter" — ANY field, not just `summary`/`type`. The query and
    // search paths read every field straight from these sidecars (`tags`,
    // `links`, `created`, `updated`, plus type-specific `email` / `domain` /
    // `company` / `amount` / `vendor` …), so a single field left unchecked lets
    // a stale value answer queries with data that exists in no `.md` file.
    //
    // Rather than re-list (and drift from) every projected key, rebuild the
    // record the canonical projection would write for this file
    // ([`IndexRecord::expected_from_file`], the same path `index rebuild` uses)
    // and diff the two as flat JSON maps. Every key the projection emits is
    // covered automatically; `path` is the join key and is skipped.
    for (path, rec) in &records {
        let target_abs = store.root.join(path);
        if !target_abs.is_file() {
            continue;
        }
        let Ok(expected) = crate::index::IndexRecord::expected_from_file(&target_abs, path.clone())
        else {
            continue; // unreadable / unparseable frontmatter is reported elsewhere
        };
        let Ok(expected_json) = serde_json::to_value(&expected) else {
            continue;
        };
        let (Some(have), Some(want)) = (rec.as_object(), expected_json.as_object()) else {
            continue;
        };

        // Compare the union of keys present on either side; a key the file
        // projects but the sidecar omits is just as stale as a wrong value.
        let mut mismatched_keys: BTreeSet<&str> = BTreeSet::new();
        for key in have.keys().chain(want.keys()) {
            if key == "path" {
                continue;
            }
            if have.get(key) != want.get(key) {
                mismatched_keys.insert(key);
            }
        }

        if !mismatched_keys.is_empty() {
            let keys: Vec<&str> = mismatched_keys.into_iter().collect();
            push(
                issues,
                Severity::Error,
                codes::INDEX_JSONL_STALE,
                jsonl_rel,
                None,
                Some(keys.join(",")),
                format!(
                    "`index.jsonl` record for `{}` is stale ({})",
                    path.display(),
                    keys.join(", ")
                ),
                Some("run `dbmd index rebuild`".into()),
                vec![path.clone()],
            );
        }
    }
    let _ = tf;
}

/// Check an index's `scope:` frontmatter against its filesystem location.
fn check_index_scope(
    store: &Store,
    index_rel: &Path,
    expected_scope: &str,
    expected_folder: Option<&str>,
    issues: &mut Vec<Issue>,
) {
    let abs = store.root.join(index_rel);
    let Ok(text) = std::fs::read_to_string(&abs) else {
        return;
    };
    let Some((yaml, _, _)) = split_frontmatter(&text) else {
        return;
    };
    let Ok(Value::Mapping(map)) = serde_norway::from_str::<Value>(&yaml) else {
        return;
    };
    let fm = yaml_map_to_btree(&map);

    if let Some(scope) = fm.get("scope").and_then(scalar_string) {
        // Accept "type-folder" and the SPEC example's looser "folder" alias.
        let scope_ok =
            scope == expected_scope || (expected_scope == "type-folder" && scope == "folder");
        if !scope_ok {
            push(
                issues,
                Severity::Warning,
                codes::INDEX_WRONG_SCOPE,
                index_rel,
                fm_key_line(&yaml, "scope"),
                Some("scope".into()),
                format!(
                    "index `scope: {scope}` doesn't match location (expected `{expected_scope}`)"
                ),
                Some(format!("set `scope: {expected_scope}`")),
                vec![],
            );
        }
    }
    // folder: must match for layer/type-folder indexes.
    if let Some(expected) = expected_folder {
        if let Some(folder) = fm.get("folder").and_then(scalar_string) {
            if folder.trim_end_matches('/') != expected.trim_end_matches('/') {
                push(
                    issues,
                    Severity::Warning,
                    codes::INDEX_WRONG_SCOPE,
                    index_rel,
                    fm_key_line(&yaml, "folder"),
                    Some("folder".into()),
                    format!("index `folder: {folder}` doesn't match location `{expected}`"),
                    Some(format!("set `folder: {expected}`")),
                    vec![],
                );
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Cross-file: log.md well-formedness + ordering (validate_all only)
// ─────────────────────────────────────────────────────────────────────────────

/// `LOG_*` checks: bad timestamps, unknown kinds, out-of-order entries — across
/// the active `log.md` AND the rotated `log/<YYYY-MM>.md` archives.
///
/// [`Log::append`] rolls strictly-prior-month entries into `log/<YYYY-MM>.md`,
/// and `Log::tail`/`Log::since` deliberately read those archives back. If the
/// LOG_* checks read only the active file, an entry `validate --all` flagged
/// while it lived in `log.md` would stop being flagged the moment a newer-month
/// append rotated it into an archive — even though the log readers still surface
/// that exact entry to the curator. Scanning the archives too keeps validate and
/// the readers in agreement after a rotation.
///
/// Order: archives oldest-month first, then the active `log.md` last — the true
/// chronological timeline — so the out-of-order check threads `prev` across the
/// rotation boundary the same way it does within a single file.
fn check_log(store: &Store, issues: &mut Vec<Issue>) {
    let mut prev: Option<DateTime<FixedOffset>> = None;
    for rel in log_files_chronological(store) {
        check_log_file(store, &rel, &mut prev, issues);
    }
}

/// The log files to scan, in chronological order: every `log/<YYYY-MM>.md`
/// archive oldest-month first, then the active `log.md` last. Missing files are
/// simply absent from the list.
fn log_files_chronological(store: &Store) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    let archive_dir = store.root.join("log");
    if let Ok(entries) = std::fs::read_dir(&archive_dir) {
        let mut archives: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .and_then(|s| s.to_str())
                        .and_then(|n| n.strip_suffix(".md"))
                        .is_some_and(is_year_month_archive)
            })
            .filter_map(|p| p.strip_prefix(&store.root).ok().map(Path::to_path_buf))
            .collect();
        // `YYYY-MM` stems sort lexically == chronologically; oldest first.
        archives.sort();
        files.extend(archives);
    }
    // The active file holds the current month — newest, so it comes last.
    if store.root.join("log.md").is_file() {
        files.push(PathBuf::from("log.md"));
    }
    files
}

/// Scan one log file's entry headers, threading the running `prev` timestamp so
/// the out-of-order check spans file (rotation) boundaries. Issues anchor to the
/// given store-relative path so an archived entry points at its archive file.
fn check_log_file(
    store: &Store,
    log_rel: &Path,
    prev: &mut Option<DateTime<FixedOffset>>,
    issues: &mut Vec<Issue>,
) {
    let abs = store.root.join(log_rel);
    let Ok(text) = std::fs::read_to_string(&abs) else {
        return;
    };

    for (i, line) in text.lines().enumerate() {
        if !line.starts_with("## [") {
            continue;
        }
        let line_no = (i + 1) as u32;
        match parse_log_header(line) {
            None => push(
                issues,
                Severity::Error,
                codes::LOG_BAD_TIMESTAMP,
                log_rel,
                Some(line_no),
                None,
                format!("log entry header has an unparseable timestamp: {line:?}"),
                Some("use `## [YYYY-MM-DD HH:MM] <kind> | <object>`".into()),
                vec![],
            ),
            Some((ts, kind, _object)) => {
                if !RECOGNIZED_LOG_KINDS.contains(&kind.as_str()) {
                    push(
                        issues,
                        Severity::Warning,
                        codes::LOG_UNKNOWN_KIND,
                        log_rel,
                        Some(line_no),
                        None,
                        format!("log entry kind `{kind}` is not recognized"),
                        Some(format!("use one of: {}", RECOGNIZED_LOG_KINDS.join(", "))),
                        vec![],
                    );
                }
                if let Some(p) = *prev {
                    if ts < p {
                        push(
                            issues,
                            Severity::Warning,
                            codes::LOG_OUT_OF_ORDER,
                            log_rel,
                            Some(line_no),
                            None,
                            "log entry is older than the entry above it (possible rewrite)".into(),
                            Some("append corrective entries; never reorder past ones".into()),
                            vec![],
                        );
                    }
                }
                *prev = Some(ts);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Self-contained primitives (collapse onto sibling modules once they land)
// ─────────────────────────────────────────────────────────────────────────────

/// A minimal wiki-link found in a body: target, optional display, 1-based line.
#[derive(Debug)]
struct Link {
    target: String,
    line: u32,
}

/// True if the store marker (`DB.md`, uppercase) is present at the root. On a
/// case-insensitive filesystem `db.md` would also match `DB.md`; we require the
/// exact-cased directory entry to be present.
fn store_marker_present(store: &Store) -> bool {
    let want = store.root.join("DB.md");
    if !want.is_file() {
        return false;
    }
    // Reject a case-folded match (`db.md`) on case-insensitive filesystems.
    match std::fs::read_dir(&store.root) {
        Ok(entries) => entries
            .flatten()
            .any(|e| e.file_name().to_str() == Some("DB.md")),
        Err(_) => true, // can't enumerate; trust the is_file() above
    }
}

/// Validate the store's identity file, `DB.md`: its frontmatter `type:` must be
/// `db-md`, it must carry both `scope` and `owner`, and its body may contain
/// only the three recognized `##` sections (`Agent instructions`, `Policies`,
/// `Schemas`).
///
/// `DB.md` is not a content file (no `summary`), so it is checked here rather
/// than through `check_content_file`. The marker presence is established by the
/// caller (`store_marker_present`); a malformed-frontmatter `DB.md` still counts
/// as a store (the marker is the filename), so we report its shape rather than
/// `NOT_A_STORE`. Issues anchor to `DB.md` as the store-relative path.
fn check_db_md(store: &Store, issues: &mut Vec<Issue>) {
    let rel = Path::new("DB.md");
    let abs = store.root.join("DB.md");
    let Ok(text) = std::fs::read_to_string(&abs) else {
        return; // marker present but unreadable: nothing more to say.
    };

    let Some((fm_yaml, body, fm_end_line)) = split_frontmatter(&text) else {
        // No frontmatter block at all → it cannot declare `type: db-md` and has
        // neither required field. Report the type and both missing fields,
        // anchored to line 1 (the would-be opening fence).
        push(
            issues,
            Severity::Error,
            codes::DB_MD_BAD_TYPE,
            rel,
            Some(1),
            Some("type".into()),
            "DB.md has no frontmatter; it must declare `type: db-md`".into(),
            Some("add a `---` frontmatter block with `type: db-md`".into()),
            vec![],
        );
        for field in ["scope", "owner"] {
            push(
                issues,
                Severity::Error,
                codes::DB_MD_MISSING_FIELD,
                rel,
                Some(1),
                Some(field.into()),
                format!("DB.md frontmatter is missing required field `{field}`"),
                Some(format!("add `{field}:` to the DB.md frontmatter")),
                vec![],
            );
        }
        return;
    };

    // Parse the frontmatter mapping. If it doesn't parse, we can still say the
    // identity contract is unmet (no provable `type: db-md`, no provable fields).
    let fm: Option<BTreeMap<String, Value>> = match serde_norway::from_str::<Value>(&fm_yaml) {
        Ok(Value::Mapping(map)) => Some(yaml_map_to_btree(&map)),
        Ok(Value::Null) => Some(BTreeMap::new()),
        _ => None,
    };

    match &fm {
        Some(map) => {
            // ── type: db-md ──────────────────────────────────────────────────
            let type_ = map.get("type").and_then(scalar_string);
            if type_.as_deref() != Some("db-md") {
                let (line, msg) = match &type_ {
                    Some(t) => (
                        fm_key_line(&fm_yaml, "type"),
                        format!("DB.md has `type: {t}`; a store's DB.md must be `type: db-md`"),
                    ),
                    None => (
                        Some(1),
                        "DB.md frontmatter has no `type:`; it must be `type: db-md`".to_string(),
                    ),
                };
                push(
                    issues,
                    Severity::Error,
                    codes::DB_MD_BAD_TYPE,
                    rel,
                    line,
                    Some("type".into()),
                    msg,
                    Some("set `type: db-md` in the DB.md frontmatter".into()),
                    vec![],
                );
            }

            // ── required fields: scope + owner ───────────────────────────────
            for field in ["scope", "owner"] {
                let present = map
                    .get(field)
                    .and_then(scalar_string)
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
                if !present {
                    push(
                        issues,
                        Severity::Error,
                        codes::DB_MD_MISSING_FIELD,
                        rel,
                        // A present-but-empty field anchors to its line; a fully
                        // absent one to the block top.
                        fm_key_line_or_top(&fm_yaml, field),
                        Some(field.into()),
                        format!("DB.md frontmatter is missing required field `{field}`"),
                        Some(format!("add `{field}:` to the DB.md frontmatter")),
                        vec![],
                    );
                }
            }
        }
        None => {
            // Unparseable frontmatter: the identity contract is unprovable. Emit
            // the type error and both field errors, anchored to the block top.
            push(
                issues,
                Severity::Error,
                codes::DB_MD_BAD_TYPE,
                rel,
                Some(1),
                Some("type".into()),
                "DB.md frontmatter isn't valid YAML; it must declare `type: db-md`".into(),
                Some("fix the DB.md frontmatter and set `type: db-md`".into()),
                vec![],
            );
            for field in ["scope", "owner"] {
                push(
                    issues,
                    Severity::Error,
                    codes::DB_MD_MISSING_FIELD,
                    rel,
                    Some(1),
                    Some(field.into()),
                    format!("DB.md frontmatter is missing required field `{field}`"),
                    Some(format!("add `{field}:` to the DB.md frontmatter")),
                    vec![],
                );
            }
        }
    }

    // ── recognized `##` section headers only ─────────────────────────────────
    // The body's H2 headings must be one of the four the toolkit reads; any
    // other is a likely typo / misplacement (warning — the parser ignores it,
    // so the config is not corrupted, but the operator wrote a section that will
    // never be read). H3 sub-headings (Frozen pages, Ignored types, `### <type>`
    // schema blocks) live under their H2 and are not flagged here.
    //
    // `## Folders` is recognized: `parse_db_md` reads it into `Config.folders`
    // (parser.rs) and the index renders folder display names + descriptions from
    // it (index.rs `render_*_md_from_stats`). Flagging it `DB_MD_UNKNOWN_SECTION`
    // with "remove this heading" told the operator to delete a working,
    // round-tripped config block — destroying curator-authored rollup names. It
    // is a real, shipped section; SPEC.md documents it alongside the other three.
    for section in crate::parser::extract_sections(&body) {
        if section.level != 2 {
            continue;
        }
        let name = section.heading.trim().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "agent instructions" | "policies" | "schemas" | "folders"
        ) {
            continue;
        }
        // `Section::line` is 1-based within the body; the body begins at file
        // line `fm_end_line + 1`.
        let file_line = fm_end_line + section.line;
        push(
            issues,
            Severity::Warning,
            codes::DB_MD_UNKNOWN_SECTION,
            rel,
            Some(file_line),
            None,
            format!(
                "DB.md has an unrecognized `## {}` section",
                section.heading.trim()
            ),
            Some(
                "DB.md sections are `## Agent instructions`, `## Policies`, `## Schemas`, \
                 `## Folders` — remove or rename this heading"
                    .into(),
            ),
            vec![],
        );
    }

    // ── `## Schemas` field-declaration lint ──────────────────────────────────
    // Without this, every schema misparse is silent: the operator/agent gets no
    // signal that DB.md is interpreting their schema differently from what they
    // wrote, and downstream records are validated against the degraded schema.
    check_db_md_schemas(store, rel, &body, fm_end_line, issues);
}

/// Lint the parsed `## Schemas` field declarations: an empty field name, a
/// duplicate field name within a type, or an unrecognized modifier all parse
/// "successfully" into a degraded [`Schema`] today, so a bad declaration never
/// surfaces. The parsed schemas live in `store.config.schemas` (directives
/// already separated out); this pass reports the suspicious *field* shapes,
/// anchored to the `### <type>` heading line so the agent can find the block.
fn check_db_md_schemas(
    store: &Store,
    rel: &Path,
    body: &str,
    fm_end_line: u32,
    issues: &mut Vec<Issue>,
) {
    if store.config.schemas.is_empty() {
        return;
    }

    // Map each `### <type>` heading (under `## Schemas`) to its file line, so a
    // per-type issue can anchor to the declaration block. `extract_sections`
    // returns a flat list with 1-based body lines; the body starts at file line
    // `fm_end_line + 1`.
    let mut type_line: BTreeMap<String, u32> = BTreeMap::new();
    let mut current_h2: Option<String> = None;
    for section in crate::parser::extract_sections(body) {
        match section.level {
            2 => current_h2 = Some(section.heading.trim().to_ascii_lowercase()),
            3 if current_h2.as_deref() == Some("schemas") => {
                // The H3 heading text (as written) is the type name — the same
                // key `parse_db_md` inserts into `config.schemas`.
                type_line
                    .entry(section.heading.trim().to_string())
                    .or_insert(fm_end_line + section.line);
            }
            _ => {}
        }
    }

    for (type_name, schema) in &store.config.schemas {
        let line = type_line.get(type_name).copied();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for field in &schema.fields {
            let name = field.name.trim();

            // Empty field name: a `- (string)` / bare `- ` bullet parses to a
            // nameless field that can never match a frontmatter key, so its
            // required/shape/enum constraints silently never apply.
            if name.is_empty() {
                push(
                    issues,
                    Severity::Warning,
                    codes::DB_MD_SCHEMA_FIELD,
                    rel,
                    line,
                    None,
                    format!("`### {type_name}` has a schema field bullet with no field name"),
                    Some(
                        "write each field as `- <name> (<modifiers>)`, e.g. `- email (required, email)`"
                            .into(),
                    ),
                    vec![],
                );
                continue;
            }

            // Duplicate field name within a type: the second declaration's
            // constraints are interpreted independently of the first, so the
            // author's intent is ambiguous and likely wrong.
            if !seen.insert(name.to_string()) {
                push(
                    issues,
                    Severity::Warning,
                    codes::DB_MD_SCHEMA_FIELD,
                    rel,
                    line,
                    Some(name.to_string()),
                    format!("`### {type_name}` declares field `{name}` more than once"),
                    Some(
                        "remove the duplicate field bullet, or merge the modifiers onto one".into(),
                    ),
                    vec![],
                );
            }

            // Unrecognized modifiers: the parser stashes anything outside the
            // known vocabulary (`required` / a shape / `link to …` / `default …`
            // / `enum: …`) in `unknown_modifiers`. Surface them as Info so a
            // typo'd modifier (`requierd`, `unqiue`) doesn't silently do nothing.
            for modifier in &field.unknown_modifiers {
                let modifier = modifier.trim();
                if modifier.is_empty() {
                    continue;
                }
                push(
                    issues,
                    Severity::Info,
                    codes::DB_MD_SCHEMA_FIELD,
                    rel,
                    line,
                    Some(name.to_string()),
                    format!(
                        "`### {type_name}` field `{name}` has an unrecognized modifier `{modifier}`"
                    ),
                    Some(
                        "recognized modifiers are `required`, a shape (`string`/`int`/`bool`/`date`/`email`/`currency`/`url`), `link to <prefix>/`, `default <value>`, `enum: <v1>, <v2>, …`"
                            .into(),
                    ),
                    vec![],
                );
            }
        }
    }
}

/// The `NOT_A_STORE` issue for a root with no `DB.md`.
fn not_a_store_issue(store: &Store) -> Issue {
    Issue {
        severity: Severity::Error,
        code: codes::NOT_A_STORE,
        file: store.root.clone(),
        line: None,
        key: None,
        message: format!("{} has no DB.md; not a db.md store", store.root.display()),
        suggestion: Some("create a `DB.md` at the store root".into()),
        related: vec![],
    }
}

/// True if a store-relative path is a content file: under `sources/` or
/// `records/` and not an `index.md`/`index.jsonl`/`log.md`.
fn is_content_file(rel: &Path) -> bool {
    // Defense in depth: a real content file is always a forward (Normal-only)
    // store-relative path. Reject any `..`/absolute/prefix component so a
    // malformed object slot judged only by its FIRST component (`records/../..`)
    // can never turn a per-file read into a store escape, even if a future caller
    // forgets the path-safety gate `changed_objects_since` now applies.
    if !is_safe_store_relative_path(rel) {
        return false;
    }
    let Some(first) = rel.iter().next().and_then(|s| s.to_str()) else {
        return false;
    };
    if !matches!(first, "sources" | "records") {
        return false;
    }
    let name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("");
    // Only the derived catalog twins are meta INSIDE a layer. `DB.md` / `log.md`
    // are reserved meta only at the store ROOT, which the `first` layer check
    // above already excludes — so a content file named `log.md` / `DB.md` inside
    // a layer (e.g. `records/docs/log.md`) is real content, consistent with
    // `Store::walk`.
    if matches!(name, "index.md" | "index.jsonl") {
        return false;
    }
    name.ends_with(".md")
}

/// True for the store's ROOT append-only meta files (`DB.md` / `log.md`): a
/// single-component store-relative path whose name is one of those two. An
/// in-layer `records/docs/log.md` is real content (multiple components), not a
/// root meta file. These reach `check_content_file` only via the working-set
/// incoming-linker scan; their bodies are deliberately not link-checked there
/// because `validate --all` doesn't link-check them either.
fn is_root_meta_file(rel: &Path) -> bool {
    let mut comps = rel.components();
    let Some(Component::Normal(only)) = comps.next() else {
        return false;
    };
    if comps.next().is_some() {
        return false; // has a parent dir → not a root file
    }
    matches!(only.to_str(), Some("DB.md") | Some("log.md"))
}

/// True for a derived index-catalog file (`index.md` / `index.jsonl`) at any
/// depth. Its entries are GENERATED wiki-links to type-folder members, not
/// authored body links: in the working-set scope it is pulled in as an incoming
/// linker, but its integrity belongs to `check_indexes` under `--all` (which
/// reports a dangling entry as `INDEX_STALE_ENTRY`, not `WIKI_LINK_BROKEN`). So
/// `check_content_file` never body-link-checks it, matching `walk_content_files`
/// (which skips `index.md` under `--all`).
fn is_index_catalog_file(rel: &Path) -> bool {
    matches!(
        rel.file_name().and_then(|n| n.to_str()),
        Some("index.md") | Some("index.jsonl")
    )
}

/// Split a file into `(frontmatter_yaml, body, closing_fence_line)`. The block
/// must start at the very first line with `---` and end at the next `---`.
/// Returns `None` if there's no leading frontmatter block.
fn split_frontmatter(text: &str) -> Option<(String, String, u32)> {
    // Tolerate a single leading UTF-8 BOM, matching parser/store/index (which
    // already strip it). Without this, a BOM-prefixed file is read as having no
    // frontmatter here while the catalog still indexes it — so validate would
    // silently skip frontmatter checks on a file the rest of the toolkit sees.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut yaml = String::new();
    let mut close_line: Option<u32> = None;
    // line 1 is the opening fence; YAML starts at line 2.
    let mut current = 1u32;
    for line in lines {
        current += 1;
        if line.trim_end() == "---" {
            close_line = Some(current);
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    let close_line = close_line?;
    // Body = everything after the closing fence.
    let body: String = text
        .lines()
        .skip(close_line as usize)
        .collect::<Vec<_>>()
        .join("\n");
    Some((yaml, body, close_line))
}

/// Read just the `summary` field of a file, or `None` if absent/unparseable.
fn read_summary(abs: &Path) -> Option<String> {
    let text = std::fs::read_to_string(abs).ok()?;
    let (yaml, _, _) = split_frontmatter(&text)?;
    let value: Value = serde_norway::from_str(&yaml).ok()?;
    if let Value::Mapping(m) = value {
        m.get(Value::String("summary".into()))
            .and_then(scalar_string)
    } else {
        None
    }
}

/// Convert a `serde_norway` mapping into a string-keyed [`BTreeMap`], dropping
/// non-string keys (frontmatter keys are always strings).
fn yaml_map_to_btree(map: &serde_norway::Mapping) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for (k, v) in map {
        if let Value::String(s) = k {
            out.insert(s.clone(), v.clone());
        }
    }
    out
}

/// A scalar YAML value as a string (`String`/`Number`/`Bool`); `None` for
/// sequences/mappings/null.
fn scalar_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// True if a frontmatter value carries no content for a *required*-field check:
/// a YAML `null` (`name:`), an empty sequence (`name: []`), an empty mapping
/// (`name: {}`), or a blank/whitespace-only scalar (`name: ""`). A non-empty
/// list or mapping is NOT treated as empty here — a structurally-wrong value on
/// a shape/enum field is caught by the later non-scalar shape check, not by the
/// required-presence check.
fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Sequence(items) => items.is_empty(),
        Value::Mapping(map) => map.is_empty(),
        other => scalar_string(other)
            .map(|s| s.trim().is_empty())
            .unwrap_or(true),
    }
}

/// True if `tags` is a flat YAML sequence of scalars. A mapping, a scalar, or a
/// sequence containing a nested sequence/mapping → false (`TAGS_MALFORMED`).
fn is_flat_scalar_list(v: &Value) -> bool {
    match v {
        Value::Sequence(items) => items.iter().all(|it| scalar_string(it).is_some()),
        _ => false,
    }
}

/// Extract every frontmatter wiki-link, returning `(key, Link)` pairs with the
/// link's 1-based file line. **Text-based, by necessity:** an unquoted
/// `company: [[records/companies/x]]` parses in YAML as a nested *sequence*, not
/// a string (because `[[x]]` is YAML flow-list-in-a-list); a quoted
/// `"[[...]]"` parses as a string. Scanning the raw frontmatter text catches
/// both forms uniformly, the way the link textually appears — the doctrine view.
///
/// `fm_start_line` is the file line of the first YAML line (file line 2, since
/// line 1 is the opening `---`), so the returned `Link::line` is absolute.
fn frontmatter_link_fields_text(fm_yaml: &str, fm_start_line: u32) -> Vec<(String, Link)> {
    let mut out = Vec::new();
    for (key, _value_text, links) in frontmatter_key_blocks(fm_yaml, fm_start_line) {
        for link in links {
            out.push((key.clone(), link));
        }
    }
    out
}

/// The wiki-link targets declared under a single top-level frontmatter key
/// (text-based; handles quoted + unquoted forms). Empty if the key is absent or
/// carries no `[[...]]`.
fn frontmatter_links_for_key(fm_yaml: &str, key: &str, fm_start_line: u32) -> Vec<Link> {
    for (k, _value_text, links) in frontmatter_key_blocks(fm_yaml, fm_start_line) {
        if k == key {
            return links;
        }
    }
    Vec::new()
}

/// The raw value text under a single top-level frontmatter key (the remainder of
/// the key line plus any indented continuation/sequence lines), trimmed. Used to
/// decide whether a `link to` field holds a plain string vs. a wiki-link.
fn frontmatter_raw_value_for_key(fm_yaml: &str, key: &str, fm_start_line: u32) -> Option<String> {
    for (k, value_text, _links) in frontmatter_key_blocks(fm_yaml, fm_start_line) {
        if k == key {
            return Some(value_text);
        }
    }
    None
}

/// Split a frontmatter YAML block into `(key, raw_value_text, wiki_links)` for
/// each top-level key. A top-level key is a line with no leading indentation in
/// `name:` form; its value spans the rest of that line plus any deeper-indented
/// continuation lines (block scalars, block sequences) until the next top-level
/// key. Wiki-links are every `[[...]]` found anywhere in that span, with their
/// absolute file line.
fn frontmatter_key_blocks(fm_yaml: &str, fm_start_line: u32) -> Vec<(String, String, Vec<Link>)> {
    let mut blocks: Vec<(String, String, Vec<Link>)> = Vec::new();
    let mut current: Option<(String, String, Vec<Link>)> = None;

    for (idx, raw_line) in fm_yaml.lines().enumerate() {
        let file_line = fm_start_line + idx as u32;
        let indented = raw_line.starts_with(' ') || raw_line.starts_with('\t');
        let trimmed = raw_line.trim();

        // A new top-level key: no indentation, `name:` prefix, not a list dash or
        // comment. (Indented or dash lines belong to the current key's value.)
        let new_key = if !indented && !trimmed.starts_with('#') && !trimmed.starts_with('-') {
            top_level_key(raw_line)
        } else {
            None
        };

        if let Some((key, after)) = new_key {
            if let Some(done) = current.take() {
                blocks.push(done);
            }
            let mut links = Vec::new();
            collect_line_links(after, file_line, &mut links);
            current = Some((key, after.trim().to_string(), links));
        } else if let Some((_k, value_text, links)) = current.as_mut() {
            // Continuation of the current key's value (indented or dash line).
            if !value_text.is_empty() {
                value_text.push('\n');
            }
            value_text.push_str(trimmed);
            collect_line_links(raw_line, file_line, links);
        }
    }
    if let Some(done) = current.take() {
        blocks.push(done);
    }
    blocks
}

/// Parse a top-level frontmatter key line into `(key, value_after_colon)`.
/// `None` if the line isn't a `name:` mapping entry.
fn top_level_key(line: &str) -> Option<(String, &str)> {
    let (key, rest) = line.split_once(':')?;
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some((key.to_string(), rest))
}

/// Append every `[[target]]` / `[[target|display]]` found in `s` to `links`,
/// each tagged with `file_line`.
fn collect_line_links(s: &str, file_line: u32, links: &mut Vec<Link>) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if let Some(close) = s[i + 2..].find("]]") {
                let inner = &s[i + 2..i + 2 + close];
                // Guard against `[[[` (nested) double-counting: the inner must
                // not itself open another `[[`.
                let target = inner
                    .trim_start_matches('[')
                    .split('|')
                    .next()
                    .unwrap_or(inner)
                    .trim()
                    .to_string();
                if !target.is_empty() {
                    links.push(Link {
                        target,
                        line: file_line,
                    });
                }
                i = i + 2 + close + 2;
                continue;
            }
        }
        i += 1;
    }
}

/// Extract every `[[...]]` wiki-link from a body, with 1-based line numbers.
/// Skips fenced code blocks, so example links in docs don't trip the validator.
///
/// Fence tracking matches the toolkit's parser ([`crate::parser`]'s
/// `extract_sections`): an open fence is `(fence char, run length)` and closes
/// only on a line that is the **same** fence character with a run **at least as
/// long**. A naive "toggle a bool on any ``` or ~~~ line" inverts the state when
/// a `~~~` block legally contains a ```` ``` ```` line (the standard way to
/// document a backtick fence) — the inner backtick line would flip `in_fence`
/// off and the demo `[[…]]` inside the code block would be checked as a live
/// link, falsely flagging a legal store.
fn extract_wiki_links(body: &str) -> Vec<Link> {
    let mut out = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    for (idx, line) in body.lines().enumerate() {
        let content = line.trim_end_matches('\r');
        if let Some(f) = fence {
            // Inside a fence: the only thing that matters is whether THIS line
            // closes it (matching char, run ≥ the opening run). Everything else
            // is opaque code — no link extraction.
            if fence_closes(content, f) {
                fence = None;
            }
            continue;
        }
        if let Some(opened) = fence_opens(content) {
            fence = Some(opened);
            continue;
        }
        let line_no = (idx + 1) as u32;
        let bytes = line.as_bytes();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'[' && bytes[i + 1] == b'[' {
                if let Some(close) = line[i + 2..].find("]]") {
                    let inner = &line[i + 2..i + 2 + close];
                    let target = inner.split('|').next().unwrap_or(inner).trim().to_string();
                    // Skip a triple-bracket `[[[…` opening: the inner content
                    // starts with `[`, so this is the rejected flow-form list
                    // mis-encoding (`[[[a]], [[b]]]`), not a real wiki-link. A
                    // legitimate target never starts with `[`. The frontmatter
                    // `WIKI_LINK_FLOW_FORM_LIST` check already owns that error;
                    // extracting a bogus body link here would double-report it as
                    // a spurious `WIKI_LINK_SHORT_FORM`.
                    if !target.is_empty() && !target.starts_with('[') {
                        out.push(Link {
                            target,
                            line: line_no,
                        });
                    }
                    i = i + 2 + close + 2;
                    continue;
                }
            }
            i += 1;
        }
    }
    out
}

/// If `line` opens a fenced code block, return `(fence byte, run length)`. A
/// local mirror of the parser's `opening_fence` so the validator's fence
/// tracking matches the rest of the toolkit: a fence is ``` ``` ``` or `~~~`
/// (run ≥ 3) at ≤ 3 spaces of indent, and a backtick fence's info string may
/// not itself contain a backtick.
fn fence_opens(line: &str) -> Option<(u8, usize)> {
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

/// True if `line` closes the currently open `fence`: same char, run at least as
/// long, nothing but trailing whitespace after. Local mirror of the parser's
/// `is_closing_fence` — so an inner fence of the *other* character (a ``` ``` ```
/// line inside a `~~~` block) does NOT close the outer fence.
fn fence_closes(line: &str, fence: (u8, usize)) -> bool {
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

/// Detect the frontmatter INLINE flow-form wiki-link-list mis-encoding —
/// `attendees: [[[a]], [[b]]]` — and return the offending keys.
///
/// **Scoped to the inline value on the key line.** The SPEC's canonical
/// list-of-links form is the *unquoted YAML block sequence* (`- [[a]]` per
/// indented line), which is explicitly correct (SPEC § Linking) and MUST NOT be
/// flagged — even though, parsed whole, it nests the same way the rejected
/// inline flow form does. So this check looks only at the value written *inline*
/// after the colon: if it opens a flow sequence (`[…]`) whose parsed shape is a
/// nested sequence (a list whose items are themselves lists — the wiki-link-list
/// mis-encoding), it is flagged. A key with no inline value (the block form,
/// whose items live on continuation lines) is never inspected here.
///
/// Parsing the inline value (rather than a literal `starts_with("[[[")` text
/// test) is what catches the whitespace variant `attendees: [ [[a]] ]`, which
/// encodes the identical nested sequence but evaded the old prefix match.
fn detect_flow_form_link_lists(fm_yaml: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in fm_yaml.lines() {
        // Top-level key lines only (no indentation, not a comment or list dash).
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || key.starts_with('#')
            || key.starts_with('-')
            || !key
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            continue;
        }
        let rest = rest.trim();
        // Only an inline flow sequence (`[…]`) on the key line is a candidate;
        // the unquoted block form has an empty inline value and is never flagged.
        if !rest.starts_with('[') {
            continue;
        }
        // Parse just the inline value and test its shape: a list whose items are
        // themselves lists is the wiki-link-list mis-encoding (`[[[a]]]` parses
        // to `Seq[Seq[Seq[String]]]`; the scalar inline link `[[a]]` is only
        // `Seq[Seq[String]]` and is NOT flagged).
        if let Ok(Value::Sequence(items)) = serde_norway::from_str::<Value>(rest) {
            let nested = items.iter().any(|item| match item {
                Value::Sequence(inner) => inner.iter().any(|x| matches!(x, Value::Sequence(_))),
                _ => false,
            });
            if nested {
                out.push(key.to_string());
            }
        }
    }
    out
}

/// True if a bare target (no `.md`) is a full store-relative path: it contains a
/// `/` and its first segment is a known layer.
fn is_full_store_path(bare: &str) -> bool {
    let mut parts = bare.splitn(2, '/');
    let first = parts.next().unwrap_or("");
    let has_rest = parts.next().map(|r| !r.is_empty()).unwrap_or(false);
    matches!(first, "sources" | "records") && has_rest
}

/// True if a path contains only normal relative components. Validator inputs
/// come from user-authored markdown/JSON sidecars; never let absolute paths,
/// platform prefixes, or `..` turn a validation probe into a filesystem escape.
fn is_safe_store_relative_path(path: &Path) -> bool {
    let mut saw_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => saw_component = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    saw_component
}

fn safe_md_target_rel(bare: &str) -> Option<PathBuf> {
    let path = Path::new(bare);
    if !is_safe_store_relative_path(path) {
        return None;
    }
    Some(PathBuf::from(format!("{bare}.md")))
}

/// How a wiki-link / index-entry target resolves on disk.
enum TargetResolution {
    /// The target exists (either as the literal path or with a `.md` suffix).
    Exists,
    /// The target is a safe store-relative path but no file exists for it.
    Missing,
    /// The target escapes the store (absolute, `..`, prefix) — never probe it.
    Unsafe,
}

/// Resolve a bare wiki-link / index-entry target the way the graph engine does
/// ([`crate::graph`]'s `resolve_existing`): try the path **as written** first
/// (so a link to a raw non-`.md` source file kept verbatim under `sources/` —
/// `[[sources/emails/x.eml]]`, `[[sources/contracts/y.pdf]]` — resolves to the
/// real file), then the `.md`-appended path (the common case for content
/// pages). Without trying the literal path first, a legal link to a raw source
/// file is wrongly flagged `WIKI_LINK_BROKEN` even though `graph backlinks`
/// resolves it.
fn resolve_wiki_target(store: &Store, bare: &str) -> TargetResolution {
    // The literal path and the `.md`-appended path share the same safety check
    // (`safe_md_target_rel` only differs by appending `.md`), so an unsafe bare
    // target is unsafe in both forms.
    if !is_safe_store_relative_path(Path::new(bare)) {
        return TargetResolution::Unsafe;
    }
    match resolved_target_abs(store, bare) {
        Some(_) => TargetResolution::Exists,
        None => TargetResolution::Missing,
    }
}

/// The absolute on-disk path a bare wiki-link / index-entry target resolves to,
/// trying the literal path first, then `.md`-appended — mirroring the graph
/// engine. `None` when neither exists, or when the bare target escapes the store
/// (callers that need to distinguish unsafe from merely-missing use
/// [`resolve_wiki_target`]).
fn resolved_target_abs(store: &Store, bare: &str) -> Option<PathBuf> {
    if !is_safe_store_relative_path(Path::new(bare)) {
        return None;
    }
    // The literal path, as written (e.g. an `.eml`/`.pdf` source file kept
    // verbatim under `sources/`).
    let literal = store.root.join(bare);
    if literal.is_file() {
        return Some(literal);
    }
    // The `.md`-appended path (a content page referenced without its extension).
    let with_md = store.root.join(format!("{bare}.md"));
    if with_md.is_file() {
        return Some(with_md);
    }
    None
}

/// True if a bare target path is under `prefix` (both `.md`-stripped).
fn path_under_prefix(bare: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    bare == prefix || bare.starts_with(&format!("{prefix}/"))
}

/// The type-folder for a store-relative content path: `<layer>/<type-folder>`
/// (the folder directly under the layer; date-shards roll up to it). `None` for
/// files directly in a layer folder or outside the two layers.
fn type_folder_of(rel: &Path) -> Option<PathBuf> {
    let comps: Vec<&str> = rel.iter().filter_map(|s| s.to_str()).collect();
    if comps.len() < 3 {
        return None; // need layer/type-folder/file at minimum
    }
    if !matches!(comps[0], "sources" | "records") {
        return None;
    }
    Some(PathBuf::from(comps[0]).join(comps[1]))
}

/// The layer dir a *loose* content file sits directly in (`records`/`sources`):
/// exactly two path components, the first a known layer. `None` for a file
/// inside a type-folder or outside any layer. Counterpart to the index crate's
/// `loose_layer_of`, kept local so `validate` needs no index internals.
fn loose_layer_dir(rel: &Path) -> Option<PathBuf> {
    let comps: Vec<&str> = rel.iter().filter_map(|s| s.to_str()).collect();
    if comps.len() != 2 || !matches!(comps[0], "sources" | "records") {
        return None;
    }
    Some(PathBuf::from(comps[0]))
}

/// **SWEEP.** Walk every `.md` content file under `sources/`/`records/`,
/// returning store-relative paths to be parsed in full. Skips hidden dirs and
/// the index twin (`index.jsonl`). Used only by `validate_all`; the working-set
/// incoming-linker scan rides the embedded-ripgrep `Store::find_links_to_any`
/// (a single presence-only pass), so the loop default never walks-and-*parses*
/// the whole content tree.
///
/// **`log/` is NOT pruned here.** Only the *root-level* `log/` rotation archive
/// is reserved (`Store::is_in_log_dir` checks only the first path component);
/// the walk roots are the two layers, so the root archive is already out of
/// scope. A `log`-named folder *inside* a layer (e.g. `records/log/` — a
/// decision log) is real content (see `is_content_file`), so pruning every
/// `name == "log"` made `--all` silently skip those files — reporting fewer
/// errors than the default working-set scope on the same store.
fn walk_content_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for layer in ["sources", "records"] {
        let base = root.join(layer);
        if !base.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&base)
            // Follow symlinks, matching the loop-default `md_walker`
            // (store.rs `follow_links(true)`): a content file that is a symlink
            // into the store, or that lives in a symlinked-in type-folder, is
            // checked by `dbmd validate` (the loop default rides `Store::walk` /
            // `walk_all_md`, both following symlinks). Without this the `--all`
            // sweep silently SKIPPED such files, so the authoritative superset
            // reported FEWER issues than the loop scope on the same store —
            // inverting the `--all`-is-the-superset contract. walkdir's loop
            // detection drops a symlink cycle (yields an Err that `.flatten()`
            // discards), so this cannot hang.
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_str().unwrap_or("");
                !name.starts_with('.')
            })
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_str().unwrap_or("");
            if name.ends_with(".md") && name != "index.md" {
                if let Ok(rel) = entry.path().strip_prefix(root) {
                    out.push(rel.to_path_buf());
                }
            }
        }
    }
    out.sort();
    out
}

/// Every `index.md` under the store (root + layers + type-folders), as
/// store-relative paths. Used to detect orphan indexes. Like
/// [`walk_content_files`], a `log`-named folder *inside* a layer is real content
/// and its `index.md` is not pruned (only the root-level `log/` archive is
/// reserved, and the walk roots are the two layers, so it is already
/// out of scope).
fn walk_index_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if root.join("index.md").is_file() {
        out.push(PathBuf::from("index.md"));
    }
    for layer in ["sources", "records"] {
        let base = root.join(layer);
        if !base.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&base)
            // Follow symlinks, matching the loop-default `md_walker`
            // (store.rs `follow_links(true)`): a content file that is a symlink
            // into the store, or that lives in a symlinked-in type-folder, is
            // checked by `dbmd validate` (the loop default rides `Store::walk` /
            // `walk_all_md`, both following symlinks). Without this the `--all`
            // sweep silently SKIPPED such files, so the authoritative superset
            // reported FEWER issues than the loop scope on the same store —
            // inverting the `--all`-is-the-superset contract. walkdir's loop
            // detection drops a symlink cycle (yields an Err that `.flatten()`
            // discards), so this cannot hang.
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_str().unwrap_or("");
                !name.starts_with('.')
            })
            .flatten()
        {
            if entry.file_type().is_file() && entry.file_name().to_str() == Some("index.md") {
                if let Ok(rel) = entry.path().strip_prefix(root) {
                    out.push(rel.to_path_buf());
                }
            }
        }
    }
    out.sort();
    out
}

/// A parsed `index.md` entry line: the wiki-link target, the optional summary
/// text after the `—`, and the 1-based line number.
struct IndexEntry {
    target: String,
    summary_text: Option<String>,
    line: u32,
}

/// Parse the `- [[<path>]] — <summary>` entry lines of an `index.md`. Stops at a
/// `## More` footer (those lines aren't file entries). Root/layer entries with a
/// `|display` segment and a `(N)` count are parsed too — the target is the bare
/// path, the summary text is whatever follows the em dash.
fn parse_index_entries(text: &str) -> Vec<IndexEntry> {
    let mut out = Vec::new();
    let mut in_more = false;
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("## More") {
            in_more = true;
            continue;
        }
        if in_more {
            continue;
        }
        if !trimmed.starts_with("- ") {
            continue;
        }
        // Find the first `[[...]]`.
        let Some(open) = trimmed.find("[[") else {
            continue;
        };
        let Some(close_rel) = trimmed[open + 2..].find("]]") else {
            continue;
        };
        let inner = &trimmed[open + 2..open + 2 + close_rel];
        let target = inner.split('|').next().unwrap_or(inner).trim().to_string();

        // Summary text: whatever follows the first em dash (`—`) or ` - `.
        let after = &trimmed[open + 2 + close_rel + 2..];
        let summary_text = extract_index_entry_summary(after);

        out.push(IndexEntry {
            target,
            summary_text,
            line: (idx + 1) as u32,
        });
    }
    out
}

/// Pull the summary portion out of the text trailing an index entry's
/// wiki-link: drop a leading `(N files)` count, then the `—`/`-` separator, then
/// strip a trailing `  ·  #tag` suffix **only when it is a genuine tag block**
/// (so a literal `·` inside the summary text is preserved, not mistaken for the
/// renderer's tag separator).
fn extract_index_entry_summary(after: &str) -> Option<String> {
    let mut s = after.trim();
    // Drop a leading "(N ...)" count segment, if present.
    if s.starts_with('(') {
        if let Some(close) = s.find(')') {
            s = s[close + 1..].trim_start();
        }
    }
    // Require an em dash or hyphen separator before the summary.
    let s = if let Some(rest) = s.strip_prefix('—') {
        rest.trim()
    } else if let Some(rest) = s.strip_prefix('-') {
        rest.trim()
    } else {
        return None;
    };
    if s.is_empty() {
        return None;
    }
    // Strip a trailing tag block — but ONLY when it matches the EXACT delimiter
    // the renderer emits: `  ·  #tag #tag` (a *double*-spaced middot, per
    // `crate::index::format_md_entry`'s `format!("  ·  {tags}")`), dropped when
    // the file has no tags. The previous code also accepted a *single*-spaced
    // ` · ` separator, which collided with a legal summary whose own text ends
    // in a single-spaced middot-plus-hashtag tail — e.g. a tagless file with
    // `summary: "Standup notes · #standup"`. The renderer round-trips that
    // summary verbatim (no tag block, since there are no tags), but the loose
    // strip mistook the ` · #standup` for the renderer's tag suffix, compared
    // `"Standup notes"` against the file's full summary, and emitted a spurious
    // `INDEX_SUMMARY_MISMATCH` that `dbmd index rebuild` could never fix
    // (rebuild regenerates the identical line). Matching the renderer's exact
    // double-spaced delimiter makes the comparison round-trip. `rsplit_once`
    // matches from the right so only the real trailing tag block is considered.
    let s = match s.rsplit_once("  ·  ") {
        Some((summary, tags)) if is_tag_suffix(tags) => summary.trim(),
        _ => s,
    };
    Some(s.to_string())
}

/// True if `s` is a non-empty tag block: one or more whitespace-separated tokens
/// each starting with `#`, the exact shape the index renderer appends after the
/// `·` separator (`crate::index::format_md_entry`). Used to distinguish the
/// renderer's `  ·  #tag` suffix from a literal `·` inside the summary text.
fn is_tag_suffix(s: &str) -> bool {
    let mut any = false;
    for tok in s.split_whitespace() {
        if !tok.starts_with('#') || tok.len() < 2 {
            return false;
        }
        any = true;
    }
    any
}

/// Parse a `log.md` entry header `## [YYYY-MM-DD HH:MM] <kind> | <object>`.
/// Returns `(timestamp, kind, object)`; `None` if the timestamp is unparseable
/// or the header isn't well-formed.
fn parse_log_header(line: &str) -> Option<(DateTime<FixedOffset>, String, Option<String>)> {
    let rest = line.strip_prefix("## [")?;
    let close = rest.find(']')?;
    let ts_str = &rest[..close];
    let tail = rest[close + 1..].trim();

    // Parse `YYYY-MM-DD HH:MM` (the SPEC header form) as a naive local time and
    // attach a zero offset — the log header carries minute precision, no zone.
    let naive = NaiveDateTime::parse_from_str(ts_str.trim(), "%Y-%m-%d %H:%M").ok()?;
    let offset = FixedOffset::east_opt(0)?;
    let ts = naive.and_local_timezone(offset).single()?;

    // kind | object
    let (kind, object) = match tail.split_once('|') {
        Some((k, o)) => {
            let o = o.trim();
            (
                k.trim().to_string(),
                if o.is_empty() {
                    None
                } else {
                    Some(o.to_string())
                },
            )
        }
        None => (tail.to_string(), None),
    };
    if kind.is_empty() {
        return None;
    }
    Some((ts, kind, object))
}

/// Every log file that holds entries for the working-set scan: the active
/// `log.md` plus every `log/<YYYY-MM>.md` archive. [`Log::append`] rotates
/// strictly-prior-month entries into the archives, so the active file alone is
/// NOT the full timeline — both the last `validate` cutoff and a changed-but-
/// unvalidated object can live in an archive after a month rollover. Reading the
/// archives here keeps the working-set readers in sync with the rest of the log
/// layer (`Log::since`/`Log::tail`), which deliberately cross archives, and
/// prevents `dbmd validate` from silently skipping archived changed files. Reads
/// only log headers, never the content store, so the loop budget is preserved.
fn log_files_for_working_set(store: &Store) -> Vec<PathBuf> {
    let mut files = vec![store.root.join("log.md")];
    let archive_dir = store.root.join("log");
    if let Ok(entries) = std::fs::read_dir(&archive_dir) {
        let mut archives: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .and_then(|s| s.to_str())
                        .and_then(|n| n.strip_suffix(".md"))
                        .is_some_and(is_year_month_archive)
            })
            .collect();
        // Deterministic order (oldest month first); the callers fold across all
        // files so order doesn't affect the result, but a stable order keeps the
        // scan reproducible.
        archives.sort();
        files.extend(archives);
    }
    files
}

/// True if `s` looks like a `YYYY-MM` archive stem (4 digits, `-`, 2 digits) —
/// the `log/<YYYY-MM>.md` naming the rotation in [`crate::log`] emits.
fn is_year_month_archive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 7
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
}

/// The timestamp of the most recent `validate` entry across the active `log.md`
/// **and** the `log/<YYYY-MM>.md` archives — the default working-set cutoff.
/// Reads only headers; never the whole store. Archive-aware so a `validate`
/// entry that rotated into an archive after a month rollover still anchors the
/// cutoff (without this, the cutoff silently resets to `None`).
fn last_validate_at(store: &Store) -> Option<DateTime<FixedOffset>> {
    let mut latest: Option<DateTime<FixedOffset>> = None;
    for file in log_files_for_working_set(store) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            if !line.starts_with("## [") {
                continue;
            }
            if let Some((ts, kind, _)) = parse_log_header(line) {
                if kind == "validate" {
                    latest = Some(match latest {
                        Some(p) if p >= ts => p,
                        _ => ts,
                    });
                }
            }
        }
    }
    latest
}

/// The set of content objects changed since `cutoff`, read from log entries
/// whose kind mutates a file. When `cutoff` is `None`, every mutating entry
/// counts (no prior validate window). Returns store-relative `.md` paths.
///
/// Scans the active `log.md` **and** every `log/<YYYY-MM>.md` archive: after a
/// month rollover [`Log::append`] rotates prior-month entries out of the active
/// file, so an object changed-but-never-validated in a prior month lives only in
/// an archive. Reading the archives here is what keeps `dbmd validate` from
/// silently skipping those files. Reads only log headers, never the content
/// store.
fn changed_objects_since(
    store: &Store,
    cutoff: Option<DateTime<FixedOffset>>,
) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    for file in log_files_for_working_set(store) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            if !line.starts_with("## [") {
                continue;
            }
            let Some((ts, kind, object)) = parse_log_header(line) else {
                continue;
            };
            if let Some(c) = cutoff {
                if ts < c {
                    continue;
                }
            }
            if !matches!(
                kind.as_str(),
                "create" | "update" | "ingest" | "rename" | "delete" | "link"
            ) {
                continue;
            }
            if let Some(obj) = object {
                // The object slot is a store-relative path (or a wiki-link target).
                let bare = obj
                    .trim()
                    .trim_start_matches("[[")
                    .trim_end_matches("]]")
                    .split('|')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_end_matches(".md")
                    .to_string();
                if bare.is_empty() {
                    continue;
                }
                // Containment: the object slot is a log-header field that can
                // carry a `..`/absolute/prefix path (a hand-edited or
                // merge-malformed log line). Route it through the same safety gate
                // every other disk-touching validator path uses
                // (`safe_md_target_rel`, which `link_target_type` already applies)
                // so a `records/../../leaky` object cannot make
                // `validate_working_set` read + frontmatter-report on a file
                // OUTSIDE the store root. An unsafe object is dropped from the
                // changed set rather than probed.
                if let Some(rel) = safe_md_target_rel(&bare) {
                    out.insert(rel);
                }
            }
        }
    }
    out
}

/// The result of the [`derived_from_ignored_type`] policy check: the
/// `derived_from` target that resolves to an ignored-type record, plus that
/// record's type. Carries exactly what both the validate finding and the
/// write-time warning need to render their message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedFromIgnored {
    /// The `derived_from` wiki-link target as written (bare store-relative path,
    /// no `.md`).
    pub target: String,
    /// The resolved `type` of that target, which is present in
    /// `store.config.ignored_types`.
    pub target_type: String,
}

/// **The single authoritative `### Ignored types` derivation check.** Decides
/// whether a conclusion record derives from an ignored-type record: the
/// `meta-type` must be `conclusion`, `### Ignored types` must be non-empty, and
/// some `derived_from` target must resolve to a record whose `type` is in
/// `ignored_types`. Returns the first such target (and its type), or `None`.
///
/// Both surfaces call this so the policy lives in exactly one place:
/// [`check_content_file`] (read side — `dbmd validate`) feeds it the
/// `derived_from` targets it scanned from the raw frontmatter, and the write
/// surface (`dbmd write`) feeds it the targets from the composed frontmatter.
/// The link *extraction* differs per surface (text-scan with line numbers vs.
/// the parsed `Frontmatter`); the *decision* — type gate, target-type
/// resolution, and `ignored_types` membership — does not.
pub fn derived_from_ignored_type<I, S>(
    store: &Store,
    meta_type: &str,
    derived_from_targets: I,
) -> Option<DerivedFromIgnored>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if meta_type != "conclusion" || store.config.ignored_types.is_empty() {
        return None;
    }
    for target in derived_from_targets {
        let target = target.as_ref();
        if let Some(target_type) = link_target_type(store, target) {
            if store.config.ignored_types.contains(&target_type) {
                return Some(DerivedFromIgnored {
                    target: target.to_string(),
                    target_type,
                });
            }
        }
    }
    None
}

/// Resolve the `type` of a wiki-link target file (bare, no `.md`), or `None`.
fn link_target_type(store: &Store, target: &str) -> Option<String> {
    let bare = target.trim_end_matches(".md");
    let abs = store.root.join(safe_md_target_rel(bare)?);
    let text = std::fs::read_to_string(&abs).ok()?;
    let (yaml, _, _) = split_frontmatter(&text)?;
    let value: Value = serde_norway::from_str(&yaml).ok()?;
    if let Value::Mapping(m) = value {
        m.get(Value::String("type".into())).and_then(scalar_string)
    } else {
        None
    }
}

// ── Shape validators ─────────────────────────────────────────────────────────

/// True if a string is RFC3339 / ISO-8601 with a time + zone (the
/// `created`/`updated` contract: `2026-05-27T08:00:00-07:00`).
fn is_iso8601(s: &str) -> bool {
    DateTime::parse_from_rfc3339(s.trim()).is_ok()
}

/// True if a string is an ISO-8601 *date* (`2026-05-27`) or a full RFC3339
/// datetime. Type-specific date fields (`expense.date`, `contact.last_touch`)
/// accept the date-only form per the SPEC's worked example.
fn is_iso8601_date_or_datetime(s: &str) -> bool {
    let s = s.trim();
    if DateTime::parse_from_rfc3339(s).is_ok() {
        return true;
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

/// True for `<local>@<domain>` with a non-empty local part and a dotted domain.
/// There must be exactly one `@`: a domain that still contains an `@` after the
/// split (the common double-`@` typo `sarah@@acme.com`, or `a@b@c.com`) is
/// rejected — without this the domain `@acme.com` passed every other check.
fn is_email(s: &str) -> bool {
    let s = s.trim();
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains(' ')
        && !local.contains(' ')
}

/// True for a currency amount: an optional symbol or 3-letter ISO code, then a
/// plain decimal number with optional thousands separators and ≤ 2 decimals.
///
/// The numeric part is validated by hand (not `f64::parse`) so the non-numeric
/// floats `f64` accepts — `inf`, `-inf`, `NaN`, and `1e3`-style exponents — are
/// rejected, and the ≤ 2-decimal rule is actually enforced.
fn is_currency(s: &str) -> bool {
    let mut t = s.trim();
    // Strip a leading currency symbol …
    for sym in ["$", "€", "£", "¥"] {
        if let Some(rest) = t.strip_prefix(sym) {
            t = rest.trim_start();
            break;
        }
    }
    // … or a leading 3-letter ISO-4217-ish code (`USD 100`, `EUR 9.50`). The
    // code must be exactly three ASCII letters and separated from the number by
    // whitespace, so a bare `USD` with no amount still fails.
    if let Some((head, rest)) = t.split_once(char::is_whitespace) {
        if head.len() == 3 && head.chars().all(|c| c.is_ascii_alphabetic()) {
            t = rest.trim_start();
        }
    }

    let cleaned: String = t.chars().filter(|c| *c != ',').collect();
    is_plain_amount(cleaned.trim())
}

/// True for a bare decimal amount: optional sign, ≥ 1 digit, an optional
/// fractional part of 1–2 digits. No exponents, no `inf`/`NaN`, no empty string.
fn is_plain_amount(s: &str) -> bool {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (int_part, frac_part) = match digits.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (digits, None),
    };
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match frac_part {
        None => true,
        Some(f) => (1..=2).contains(&f.len()) && f.bytes().all(|b| b.is_ascii_digit()),
    }
}

/// True for an http(s) URL: a recognized scheme prefix with at least one
/// character after it. The length guard uses the *matched* scheme's own length,
/// so a single-character host on the shorter `http://` scheme (`http://x`, 8
/// bytes — e.g. an intranet/container hostname) is accepted; a bare scheme with
/// nothing after it (`http://`, `https://`) is rejected.
fn is_url(s: &str) -> bool {
    let s = s.trim();
    for scheme in ["http://", "https://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            return !rest.is_empty();
        }
    }
    false
}

/// A short, deterministic suggestion for a `SCHEMA_SHAPE_MISMATCH`.
fn shape_suggestion(shape: Shape) -> String {
    match shape {
        Shape::String => "use a scalar string".into(),
        Shape::Int => "use an integer".into(),
        Shape::Bool => "use `true` or `false`".into(),
        Shape::Date => "use an ISO-8601 date, e.g. 2026-05-27".into(),
        Shape::Email => "use a `<local>@<domain>` address".into(),
        Shape::Currency => "use a numeric amount, e.g. 1234.56".into(),
        Shape::Url => "use an http(s) URL".into(),
    }
}

/// Suggest a full-path rewrite for a short-form wiki-link. Without the layer we
/// can't know the folder, so the suggestion is generic but actionable.
fn short_form_suggestion(bare: &str) -> Option<String> {
    Some(format!(
        "use a full store-relative path, e.g. [[records/contacts/{}]]",
        slugish(bare)
    ))
}

/// A filesystem-ish leaf for a plain string (lowercase, spaces → hyphens).
fn slugish(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '/' || *c == '_')
        .collect()
}

/// Cross-file asset-manifest integrity (the `--all` sweep). Text-only: it never
/// hashes a byte or reads an asset file's contents — byte presence and hash
/// correctness are `dbmd assets verify`, not `validate`, so a fresh clone with
/// no restored bytes still passes. Cross-checks `assets.jsonl` against every
/// content file's `asset`/`assets` declarations.
fn check_assets(store: &Store, parsed: &[(PathBuf, Parsed)], issues: &mut Vec<Issue>) {
    use crate::assets;

    let manifest_rel = Path::new(assets::MANIFEST_FILE);
    let manifest_abs = store.root.join(assets::MANIFEST_FILE);

    // Lenient manifest read: a malformed line is reported, not fatal.
    let mut manifest: BTreeMap<String, assets::AssetRecord> = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(&manifest_abs) {
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<assets::AssetRecord>(line) {
                Ok(rec) => {
                    manifest.insert(rec.path.clone(), rec);
                }
                Err(e) => push(
                    issues,
                    Severity::Error,
                    codes::ASSET_MANIFEST_MALFORMED,
                    manifest_rel,
                    Some((i as u32) + 1),
                    None,
                    format!("invalid {} record: {e}", assets::MANIFEST_FILE),
                    Some("run `dbmd assets scan` to rebuild the manifest".to_string()),
                    vec![],
                ),
            }
        }
    }

    // Per-wrapper declarations: every declared asset must be in the manifest and
    // must not point at a markdown content file.
    let mut declared: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (rel, p) in parsed {
        let Some(map) = &p.fm else {
            continue;
        };
        for decl in assets::declarations_from_yaml_map(map) {
            let norm = match assets::normalize_asset_path(&decl.path) {
                Ok(n) => n,
                Err(_) => continue, // a bad declared path is surfaced by `scan`, not here
            };
            declared.insert(norm.clone());
            let is_md = Path::new(&norm)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("md"))
                .unwrap_or(false);
            if is_md {
                push(
                    issues,
                    Severity::Warning,
                    codes::ASSET_PATH_IS_CONTENT,
                    rel,
                    None,
                    Some("asset".to_string()),
                    format!("asset path `{norm}` points at a markdown content file"),
                    Some("assets are raw binaries; reference a non-markdown path".to_string()),
                    vec![PathBuf::from(&norm)],
                );
            }
            if !manifest.contains_key(&norm) {
                push(
                    issues,
                    Severity::Error,
                    codes::ASSET_UNDECLARED,
                    rel,
                    None,
                    Some("asset".to_string()),
                    format!(
                        "references asset `{norm}` with no record in {}",
                        assets::MANIFEST_FILE
                    ),
                    Some("run `dbmd assets scan` to catalog it".to_string()),
                    vec![PathBuf::from(&norm)],
                );
            }
        }
    }

    // Per-record: wrapper existence + orphan detection.
    for (path, rec) in &manifest {
        for w in &rec.wrappers {
            if !store.root.join(w).is_file() {
                push(
                    issues,
                    Severity::Error,
                    codes::ASSET_WRAPPER_BROKEN,
                    Path::new(path),
                    None,
                    None,
                    format!("manifest record for `{path}` names a missing wrapper `{w}`"),
                    Some("run `dbmd assets scan` to reconcile the manifest".to_string()),
                    vec![PathBuf::from(w)],
                );
            }
        }
        if !declared.contains(path) {
            push(
                issues,
                Severity::Warning,
                codes::ASSET_MANIFEST_ORPHAN,
                Path::new(path),
                None,
                None,
                format!(
                    "`{path}` is in {} but no wrapper references it",
                    assets::MANIFEST_FILE
                ),
                Some("run `dbmd assets scan` to drop the orphan, or add a wrapper".to_string()),
                vec![],
            );
        }
    }
}

/// Push a fully-formed [`Issue`].
#[allow(clippy::too_many_arguments)]
fn push(
    issues: &mut Vec<Issue>,
    severity: Severity,
    code: &'static str,
    file: &Path,
    line: Option<u32>,
    key: Option<String>,
    message: String,
    suggestion: Option<String>,
    related: Vec<PathBuf>,
) {
    issues.push(Issue {
        severity,
        code,
        file: file.to_path_buf(),
        line,
        key,
        message,
        suggestion,
        related,
    });
}

/// 1-based line of a top-level frontmatter key inside the YAML block, offset to
/// the file (the YAML starts at file line 2). `None` if not found.
fn fm_key_line(fm_yaml: &str, key: &str) -> Option<u32> {
    for (i, line) in fm_yaml.lines().enumerate() {
        let trimmed = line.trim_start();
        // A top-level key line: `key:` with no leading list dash.
        if let Some(rest) = trimmed.strip_prefix(key) {
            if rest.starts_with(':') && line.starts_with(key) {
                // +2: file line 1 is the opening `---`, YAML line 0 → file line 2.
                return Some((i as u32) + 2);
            }
        }
    }
    None
}

/// The line a *field-absence* issue (a required key that is missing entirely)
/// anchors to: the key's line when present, else line `1` — the frontmatter
/// block's opening `---`. A missing key has no line of its own; anchoring it to
/// the block top gives the agent (and the `EXPECTED` golden) a stable, non-null
/// line to point at instead of an unhelpful `null`.
fn fm_key_line_or_top(fm_yaml: &str, key: &str) -> Option<u32> {
    fm_key_line(fm_yaml, key).or(Some(1))
}

/// A stable sort order for issues: by file, then line, then code. Keeps `--json`
/// output deterministic across runs.
fn issue_order(a: &Issue, b: &Issue) -> std::cmp::Ordering {
    a.file
        .cmp(&b.file)
        .then(a.line.cmp(&b.line))
        .then(a.code.cmp(b.code))
        .then(a.key.cmp(&b.key))
}

// ═════════════════════════════════════════════════════════════════════════════
//  Tests
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{Config, FieldSpec};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn split_frontmatter_tolerates_leading_bom() {
        // Regression (finding #19 cross-module): a UTF-8 BOM before the opening
        // fence must not make validate treat the file as frontmatter-less while
        // the catalog indexes it. Pre-fix `first.trim_end() != "---"` was true
        // for `\u{feff}---` and the function returned None.
        let text = "\u{feff}---\ntype: contact\nsummary: hi\n---\nbody\n";
        let parsed = split_frontmatter(text);
        assert!(
            parsed.is_some(),
            "a leading BOM must not hide frontmatter from validate"
        );
        let (yaml, body, close_line) = parsed.unwrap();
        assert_eq!(yaml, "type: contact\nsummary: hi\n");
        assert_eq!(body, "body");
        assert_eq!(close_line, 4, "BOM is inline on line 1, not a new line");
    }

    /// A test store builder over a real tempdir. Every helper writes real files
    /// so the assertions exercise real behavior, not mocks.
    struct Fixture {
        dir: TempDir,
        config: Config,
    }

    impl Fixture {
        /// A fresh store with a **valid** `DB.md` (the identity contract:
        /// `type: db-md` + `scope` + `owner`) and the two layer dirs. A valid
        /// DB.md keeps `check_db_md` silent so a "clean store" fixture is truly
        /// clean; tests that want a broken DB.md write their own via `write`.
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            fs::write(
                dir.path().join("DB.md"),
                "---\ntype: db-md\nscope: company\nowner: Test\n---\n",
            )
            .unwrap();
            for layer in ["sources", "records"] {
                fs::create_dir_all(dir.path().join(layer)).unwrap();
            }
            Fixture {
                dir,
                config: Config::default(),
            }
        }

        /// A store with no `DB.md` marker.
        fn bare() -> Self {
            let dir = TempDir::new().unwrap();
            Fixture {
                dir,
                config: Config::default(),
            }
        }

        /// Write a file at a store-relative path, creating parent dirs.
        fn write(&self, rel: &str, contents: &str) {
            let abs = self.dir.path().join(rel);
            fs::create_dir_all(abs.parent().unwrap()).unwrap();
            fs::write(abs, contents).unwrap();
        }

        fn store(&self) -> Store {
            Store {
                root: self.dir.path().to_path_buf(),
                config: self.config.clone(),
            }
        }

        fn store_all(&self) -> Vec<Issue> {
            validate_all(&self.store()).unwrap()
        }

        /// Write the canonical `index.md` + `index.jsonl` at every level via the
        /// real builder ([`crate::index::Index::rebuild_all`]) — the same
        /// projection a `dbmd index rebuild` produces. Use this (rather than a
        /// hand-typed sidecar line) whenever a test asserts a *clean* store, so
        /// the sidecar carries the COMPLETE per-field projection and the fixture
        /// can't silently drift from what the index writer emits.
        fn rebuild_indexes(&self) {
            crate::index::Index::rebuild_all(&self.store()).unwrap();
        }
    }

    /// True if any issue has this code.
    fn has(issues: &[Issue], code: &str) -> bool {
        issues.iter().any(|i| i.code == code)
    }

    /// Count issues with a code.
    fn count(issues: &[Issue], code: &str) -> usize {
        issues.iter().filter(|i| i.code == code).count()
    }

    /// The first issue with a code, or panic.
    fn find<'a>(issues: &'a [Issue], code: &str) -> &'a Issue {
        issues
            .iter()
            .find(|i| i.code == code)
            .unwrap_or_else(|| panic!("expected an issue with code {code}; got {issues:#?}"))
    }

    /// A minimal valid `contact` body for reuse.
    fn valid_contact(summary: &str) -> String {
        format!(
            "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: \"{summary}\"\nname: A\n---\n\n# A\n"
        )
    }

    // ── store marker ──────────────────────────────────────────────────────────

    #[test]
    fn not_a_store_when_db_md_absent() {
        let fx = Fixture::bare();
        let issues = fx.store_all();
        assert_eq!(issues.len(), 1, "only NOT_A_STORE expected: {issues:#?}");
        assert_eq!(issues[0].code, codes::NOT_A_STORE);
        assert!(issues[0].is_error());
    }

    #[test]
    fn working_set_also_reports_not_a_store() {
        let fx = Fixture::bare();
        let issues = validate_working_set(&fx.store(), None).unwrap();
        assert!(has(&issues, codes::NOT_A_STORE));
    }

    #[test]
    fn clean_store_has_no_issues() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("A contact"));
        // Build the canonical indexes (complete per-field jsonl included) the
        // same way `dbmd index rebuild` does, so a freshly-rebuilt store is
        // proven clean across every projected field, not just summary/type.
        fx.rebuild_indexes();
        let issues = fx.store_all();
        assert!(
            issues.is_empty(),
            "expected a clean store, got: {issues:#?}"
        );
    }

    // ── meta-type closed enum ─────────────────────────────────────────────────

    /// Regression (adversarial review): a NON-SCALAR `meta-type` (a YAML list or
    /// mapping) must be rejected with `FM_BAD_META_TYPE`, not silently slip past
    /// the enum check (and then get reclassified as the default `fact`). Pre-fix
    /// the check was gated on `and_then(scalar_string)`, which returned `None`
    /// for a sequence/mapping and short-circuited the whole branch.
    #[test]
    fn meta_type_enum_is_closed_for_scalars_and_non_scalars() {
        let fx = Fixture::new();
        let body = |mt: &str| {
            format!(
                "---\ntype: profile\nmeta-type: {mt}\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\n---\n\nbody\n"
            )
        };

        // Valid enum members + absent (default fact) → no FM_BAD_META_TYPE.
        for ok in ["fact", "operational", "conclusion"] {
            fx.write("records/profiles/ok.md", &body(ok));
            let issues = validate_working_set(&fx.store(), None).unwrap();
            assert!(
                !has(&issues, codes::FM_BAD_META_TYPE),
                "`meta-type: {ok}` must be accepted; got {issues:#?}"
            );
        }
        fx.write(
            "records/profiles/absent.md",
            "---\ntype: profile\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\n---\n\nbody\n",
        );
        assert!(
            !has(
                &validate_working_set(&fx.store(), None).unwrap(),
                codes::FM_BAD_META_TYPE
            ),
            "an absent meta-type is the default `fact` and must be accepted"
        );

        // Scalar-but-wrong, AND non-scalar (list / mapping) → FM_BAD_META_TYPE.
        for bad in ["xyz", "Fact", "[fact, conclusion]", "{kind: conclusion}"] {
            let fx2 = Fixture::new();
            fx2.write("records/profiles/bad.md", &body(bad));
            let issues = validate_working_set(&fx2.store(), None).unwrap();
            assert!(
                has(&issues, codes::FM_BAD_META_TYPE),
                "`meta-type: {bad}` must be rejected with FM_BAD_META_TYPE; got {issues:#?}"
            );
        }
    }

    // ── DB.md structure ───────────────────────────────────────────────────────

    /// The `Fixture::new` DB.md is valid → no `DB_MD_*` issue. This pins the
    /// "valid identity file is silent" half (a bug that flagged a valid DB.md
    /// would fail here).
    #[test]
    fn valid_db_md_emits_no_structure_issue() {
        let fx = Fixture::new();
        let issues = fx.store_all();
        assert!(
            !has(&issues, codes::DB_MD_BAD_TYPE)
                && !has(&issues, codes::DB_MD_MISSING_FIELD)
                && !has(&issues, codes::DB_MD_UNKNOWN_SECTION),
            "a valid DB.md (type: db-md + scope + owner, recognized sections) is silent: {issues:#?}"
        );
    }

    /// A DB.md whose `type:` isn't `db-md` → `DB_MD_BAD_TYPE`, keyed on `type`,
    /// anchored to the `type:` line (file line 2). Failing to read the type, or
    /// accepting a non-`db-md` type, breaks this.
    #[test]
    fn db_md_wrong_type_is_error() {
        let fx = Fixture::new();
        fx.write("DB.md", "---\ntype: notes\nscope: company\nowner: T\n---\n");
        let issues = fx.store_all();
        let i = find(&issues, codes::DB_MD_BAD_TYPE);
        assert!(i.is_error());
        assert_eq!(i.file, PathBuf::from("DB.md"));
        assert_eq!(i.key.as_deref(), Some("type"));
        assert_eq!(i.line, Some(2), "anchors to the `type:` line");
    }

    /// A DB.md missing `scope` and `owner` → one `DB_MD_MISSING_FIELD` per
    /// absent field, each keyed on its field name, anchored to the block top.
    #[test]
    fn db_md_missing_scope_and_owner_each_report() {
        let fx = Fixture::new();
        fx.write("DB.md", "---\ntype: db-md\n---\n");
        let issues = fx.store_all();
        assert_eq!(
            count(&issues, codes::DB_MD_MISSING_FIELD),
            2,
            "both scope and owner absent → two issues: {issues:#?}"
        );
        let keys: BTreeSet<Option<String>> = issues
            .iter()
            .filter(|i| i.code == codes::DB_MD_MISSING_FIELD)
            .map(|i| i.key.clone())
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from([Some("scope".to_string()), Some("owner".to_string())]),
            "one issue keyed on each missing field"
        );
        for i in issues
            .iter()
            .filter(|i| i.code == codes::DB_MD_MISSING_FIELD)
        {
            assert!(i.is_error());
            assert_eq!(i.line, Some(1), "absent field anchors to the block top");
        }
    }

    /// A present-but-blank required field is still missing (`DB_MD_MISSING_FIELD`),
    /// anchored to its own line — guarding against an "is the key textually
    /// present?" shortcut that would miss `owner:` with an empty value.
    #[test]
    fn db_md_blank_required_field_is_missing() {
        let fx = Fixture::new();
        fx.write(
            "DB.md",
            "---\ntype: db-md\nscope: company\nowner: \"\"\n---\n",
        );
        let issues = fx.store_all();
        let i = find(&issues, codes::DB_MD_MISSING_FIELD);
        assert_eq!(i.key.as_deref(), Some("owner"));
        assert_eq!(
            i.line,
            Some(4),
            "a present-but-empty field anchors to its line"
        );
        assert!(
            count(&issues, codes::DB_MD_MISSING_FIELD) == 1,
            "scope is present and non-empty → only owner reported"
        );
    }

    /// An unrecognized `##` section → `DB_MD_UNKNOWN_SECTION` (warning), anchored
    /// to the heading's file line; the three recognized sections stay silent.
    #[test]
    fn db_md_unknown_section_is_warning() {
        let fx = Fixture::new();
        fx.write(
            "DB.md",
            // line 1 `---`, 2 type, 3 scope, 4 owner, 5 `---`, 6 blank,
            // 7 `## Agent instructions`, 8 blank, 9 prose, 10 blank,
            // 11 `## Glossary`.
            "---\ntype: db-md\nscope: company\nowner: T\n---\n\n## Agent instructions\n\nbe good\n\n## Glossary\n\nterms\n",
        );
        let issues = fx.store_all();
        let i = find(&issues, codes::DB_MD_UNKNOWN_SECTION);
        assert!(!i.is_error(), "unknown section is a warning, not an error");
        assert_eq!(i.severity, Severity::Warning);
        assert_eq!(
            i.line,
            Some(11),
            "anchors to the `## Glossary` heading line"
        );
        assert!(
            i.message.contains("Glossary"),
            "the message names the offending section: {}",
            i.message
        );
        // The recognized `## Agent instructions` section did NOT fire.
        assert_eq!(
            count(&issues, codes::DB_MD_UNKNOWN_SECTION),
            1,
            "only the unrecognized section is flagged: {issues:#?}"
        );
    }

    /// A DB.md with no frontmatter at all → `DB_MD_BAD_TYPE` plus both
    /// `DB_MD_MISSING_FIELD`s (no provable type, no provable fields).
    #[test]
    fn db_md_no_frontmatter_reports_type_and_both_fields() {
        let fx = Fixture::new();
        fx.write("DB.md", "# just a heading, no frontmatter\n");
        let issues = fx.store_all();
        assert!(has(&issues, codes::DB_MD_BAD_TYPE));
        assert_eq!(count(&issues, codes::DB_MD_MISSING_FIELD), 2);
    }

    // ── frontmatter ─────────────────────────────────────────────────────────

    #[test]
    fn missing_type_is_error() {
        let fx = Fixture::new();
        fx.write(
            "records/contacts/a.md",
            "---\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\n---\n\n# A\n",
        );
        let issues = fx.store_all();
        assert!(has(&issues, codes::FM_MISSING_TYPE));
        assert!(find(&issues, codes::FM_MISSING_TYPE).is_error());
    }

    #[test]
    fn missing_universal_timestamps_are_errors_on_content_files() {
        let fx = Fixture::new();
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\nsummary: x\nname: A\n---\n\n# A\n",
        );
        let issues = fx.store_all();

        let missing_created = find(&issues, codes::FM_MISSING_CREATED);
        assert_eq!(missing_created.key.as_deref(), Some("created"));
        assert!(missing_created.is_error());

        let missing_updated = find(&issues, codes::FM_MISSING_UPDATED);
        assert_eq!(missing_updated.key.as_deref(), Some("updated"));
        assert!(missing_updated.is_error());
    }

    #[test]
    fn meta_files_do_not_require_universal_timestamps() {
        let fx = Fixture::new();
        let issues = fx.store_all();

        assert!(
            !has(&issues, codes::FM_MISSING_CREATED),
            "DB.md/log/index meta files must not require content timestamps: {issues:#?}"
        );
        assert!(
            !has(&issues, codes::FM_MISSING_UPDATED),
            "DB.md/log/index meta files must not require content timestamps: {issues:#?}"
        );
    }

    #[test]
    fn content_file_with_no_frontmatter_block_reports_type_and_summary() {
        let fx = Fixture::new();
        fx.write(
            "records/profiles/a.md",
            "# Just a heading\n\nNo frontmatter here.\n",
        );
        let issues = fx.store_all();
        assert!(has(&issues, codes::FM_MISSING_TYPE), "{issues:#?}");
        assert!(has(&issues, codes::SUMMARY_MISSING), "{issues:#?}");
    }

    #[test]
    fn content_file_with_empty_frontmatter_reports_type_and_summary() {
        let fx = Fixture::new();
        fx.write("records/profiles/a.md", "---\n---\n\nbody\n");
        let issues = fx.store_all();
        assert!(has(&issues, codes::FM_MISSING_TYPE), "{issues:#?}");
        assert!(has(&issues, codes::SUMMARY_MISSING), "{issues:#?}");
    }

    #[test]
    fn malformed_yaml_is_error_and_suppresses_field_checks() {
        let fx = Fixture::new();
        // A tab inside a mapping value is invalid YAML.
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\n  bad: : : :\n: : nope\n---\n\nbody\n",
        );
        let issues = fx.store_all();
        let issue = find(&issues, codes::FM_MALFORMED_YAML);
        assert!(issue.is_error());
        assert!(issue.suggestion.as_deref().is_some_and(|s| !s.is_empty()));
        // When YAML doesn't parse we don't *also* claim the summary is missing;
        // the agent fixes the YAML first.
        assert!(
            !has(&issues, codes::SUMMARY_MISSING),
            "malformed YAML should suppress SUMMARY_MISSING: {issues:#?}"
        );
    }

    #[test]
    fn bad_created_timestamp_is_error() {
        let fx = Fixture::new();
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\ncreated: not-a-date\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\n---\n\n# A\n",
        );
        let issues = fx.store_all();
        let issue = find(&issues, codes::FM_BAD_TIMESTAMP);
        assert_eq!(issue.key.as_deref(), Some("created"));
        assert!(issue.is_error());
    }

    #[test]
    fn date_only_created_is_rejected_but_type_date_field_accepted() {
        let fx = Fixture::new();
        // `created` must be a full RFC3339 datetime → a date-only value is bad.
        // `last_touch` is a type-specific date field → date-only is fine.
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\ncreated: 2026-05-22\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\nlast_touch: 2026-05-22\n---\n\n# A\n",
        );
        let issues = fx.store_all();
        let created_issues: Vec<_> = issues
            .iter()
            .filter(|i| i.code == codes::FM_BAD_TIMESTAMP && i.key.as_deref() == Some("created"))
            .collect();
        assert_eq!(
            created_issues.len(),
            1,
            "date-only `created` must fail: {issues:#?}"
        );
        assert!(
            !issues.iter().any(
                |i| i.code == codes::FM_BAD_TIMESTAMP && i.key.as_deref() == Some("last_touch")
            ),
            "date-only `last_touch` is valid: {issues:#?}"
        );
    }

    // ── summary ─────────────────────────────────────────────────────────────

    #[test]
    fn summary_missing_empty_multiline_toolong() {
        let fx = Fixture::new();
        fx.write(
            "records/profiles/missing.md",
            "---\ntype: profile\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\n---\n\nbody\n",
        );
        fx.write(
            "records/profiles/empty.md",
            "---\ntype: profile\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: \"   \"\n---\n\nbody\n",
        );
        let long = "x".repeat(201);
        fx.write(
            "records/profiles/long.md",
            &format!("---\ntype: profile\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: \"{long}\"\n---\n\nbody\n"),
        );
        let issues = fx.store_all();
        assert!(has(&issues, codes::SUMMARY_MISSING));
        assert_eq!(
            find(&issues, codes::SUMMARY_MISSING).file,
            PathBuf::from("records/profiles/missing.md")
        );
        assert!(has(&issues, codes::SUMMARY_EMPTY));
        assert!(has(&issues, codes::SUMMARY_TOO_LONG));
        assert_eq!(
            find(&issues, codes::SUMMARY_TOO_LONG).severity,
            Severity::Warning
        );
    }

    #[test]
    fn summary_multiline_via_yaml_block_scalar() {
        let fx = Fixture::new();
        // A literal block scalar produces a value with a newline.
        fx.write(
            "records/profiles/a.md",
            "---\ntype: profile\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: |\n  line one\n  line two\n---\n\nbody\n",
        );
        let issues = fx.store_all();
        assert!(has(&issues, codes::SUMMARY_MULTILINE), "{issues:#?}");
    }

    #[test]
    fn summary_exactly_200_chars_is_ok() {
        let fx = Fixture::new();
        let s = "y".repeat(200);
        fx.write(
            "records/profiles/a.md",
            &format!("---\ntype: profile\nmeta-type: conclusion\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: \"{s}\"\n---\n\nbody\n"),
        );
        let issues = fx.store_all();
        assert!(
            !has(&issues, codes::SUMMARY_TOO_LONG),
            "200 is the bound, inclusive: {issues:#?}"
        );
    }

    #[test]
    fn meta_files_need_no_summary() {
        let fx = Fixture::new();
        // The root/layer/type indexes + log carry no summary and must not be
        // flagged. (A lone DB.md store with one contact and full indexes.)
        fx.write("records/contacts/a.md", &valid_contact("A contact"));
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n# I\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        fx.write("records/contacts/index.md", "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/a]] — A contact\n");
        fx.write(
            "records/contacts/index.jsonl",
            "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"A contact\"}\n",
        );
        fx.write("log.md", "---\ntype: log\n---\n\n# Log\n");
        let issues = fx.store_all();
        assert!(!has(&issues, codes::SUMMARY_MISSING), "{issues:#?}");
    }

    // ── tags ────────────────────────────────────────────────────────────────

    #[test]
    fn nested_tags_warns_flat_tags_ok() {
        let fx = Fixture::new();
        fx.write(
            "records/contacts/nested.md",
            "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\ntags:\n  - good\n  - [nested, list]\n---\n\n# A\n",
        );
        fx.write(
            "records/contacts/flat.md",
            "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\ntags: [customer, vip]\n---\n\n# A\n",
        );
        let issues = fx.store_all();
        let tag_issues: Vec<_> = issues
            .iter()
            .filter(|i| i.code == codes::TAGS_MALFORMED)
            .collect();
        assert_eq!(
            tag_issues.len(),
            1,
            "only the nested-tags file should warn: {issues:#?}"
        );
        assert_eq!(
            tag_issues[0].file,
            PathBuf::from("records/contacts/nested.md")
        );
        assert_eq!(tag_issues[0].severity, Severity::Warning);
    }

    // ── wiki-links ────────────────────────────────────────────────────────────

    #[test]
    fn short_form_wiki_link_is_error() {
        let fx = Fixture::new();
        let mut body = valid_contact("links to a short form");
        body.push_str("\nSee [[sarah-chen]] for details.\n");
        fx.write("records/contacts/a.md", &body);
        let issues = fx.store_all();
        let issue = find(&issues, codes::WIKI_LINK_SHORT_FORM);
        assert!(issue.is_error());
        assert!(issue.message.contains("sarah-chen"));
        // A short-form link must NOT also be reported broken — fix the form first.
        assert!(
            !issues
                .iter()
                .any(|i| i.code == codes::WIKI_LINK_BROKEN && i.message.contains("sarah-chen")),
            "short-form should suppress broken: {issues:#?}"
        );
    }

    #[test]
    fn broken_full_path_wiki_link_is_error() {
        let fx = Fixture::new();
        let mut body = valid_contact("links to a missing file");
        body.push_str("\nSee [[records/contacts/ghost]].\n");
        fx.write("records/contacts/a.md", &body);
        let issues = fx.store_all();
        let issue = find(&issues, codes::WIKI_LINK_BROKEN);
        assert!(issue.is_error());
        assert!(issue.message.contains("records/contacts/ghost"));
        assert!(issue.suggestion.as_deref().is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn traversal_full_path_wiki_link_is_rejected_before_probe() {
        let fx = Fixture::new();
        let mut body = valid_contact("links with traversal");
        body.push_str("\nSee [[records/contacts/../../ghost]].\n");
        fx.write("records/contacts/a.md", &body);
        let issues = fx.store_all();
        let issue = find(&issues, codes::WIKI_LINK_BROKEN);
        assert!(issue.message.contains("not a safe store-relative path"));
        assert!(issue.suggestion.as_deref().is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn valid_full_path_wiki_link_passes() {
        let fx = Fixture::new();
        fx.write("records/contacts/target.md", &valid_contact("target"));
        let mut body = valid_contact("links to target");
        body.push_str("\nSee [[records/contacts/target]].\n");
        fx.write("records/contacts/a.md", &body);
        let issues = fx.store_all();
        assert!(!has(&issues, codes::WIKI_LINK_BROKEN), "{issues:#?}");
        assert!(!has(&issues, codes::WIKI_LINK_SHORT_FORM), "{issues:#?}");
    }

    #[test]
    fn md_extension_wiki_link_warns_and_resolves() {
        let fx = Fixture::new();
        fx.write("records/contacts/target.md", &valid_contact("target"));
        let mut body = valid_contact("links with extension");
        body.push_str("\nSee [[records/contacts/target.md]].\n");
        fx.write("records/contacts/a.md", &body);
        let issues = fx.store_all();
        let issue = find(&issues, codes::WIKI_LINK_HAS_EXTENSION);
        assert_eq!(issue.severity, Severity::Warning);
        assert_eq!(
            issue.suggestion.as_deref(),
            Some("drop the extension: [[records/contacts/target]]")
        );
        // The target exists once `.md` is stripped → not broken.
        assert!(!has(&issues, codes::WIKI_LINK_BROKEN), "{issues:#?}");
    }

    #[test]
    fn wiki_links_in_code_fences_are_ignored() {
        let fx = Fixture::new();
        let mut body = valid_contact("has a fenced example");
        body.push_str("\n```\n[[sarah-chen]]\n```\n");
        fx.write("records/contacts/a.md", &body);
        let issues = fx.store_all();
        assert!(
            !has(&issues, codes::WIKI_LINK_SHORT_FORM),
            "fenced wiki-links must be ignored: {issues:#?}"
        );
    }

    #[test]
    fn flow_form_link_list_in_frontmatter_is_error() {
        let fx = Fixture::new();
        fx.write(
            "records/meetings/m.md",
            "---\ntype: meeting\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: a meeting\ndate: 2026-05-22\nattendees: [[[records/contacts/a]], [[records/contacts/b]]]\n---\n\n# M\n",
        );
        let issues = fx.store_all();
        let issue = find(&issues, codes::WIKI_LINK_FLOW_FORM_LIST);
        assert!(issue.is_error());
        assert_eq!(issue.key.as_deref(), Some("attendees"));
    }

    #[test]
    fn block_form_link_list_in_frontmatter_is_not_flow_form() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("a"));
        fx.write("records/contacts/b.md", &valid_contact("b"));
        fx.write(
            "records/meetings/m.md",
            "---\ntype: meeting\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: a meeting\ndate: 2026-05-22\nattendees:\n  - [[records/contacts/a]]\n  - [[records/contacts/b]]\n---\n\n# M\n",
        );
        let issues = fx.store_all();
        assert!(
            !has(&issues, codes::WIKI_LINK_FLOW_FORM_LIST),
            "{issues:#?}"
        );
        // Block-form link targets are still integrity-checked (both exist here).
        assert!(!has(&issues, codes::WIKI_LINK_BROKEN), "{issues:#?}");
    }

    #[test]
    fn frontmatter_short_form_link_field_is_error() {
        let fx = Fixture::new();
        // `related` is a *custom* (non-schema) wiki-link field, so it goes
        // through the generic doctrine path → a short form is WIKI_LINK_SHORT_FORM.
        fx.write(
            "records/synthesis/a.md",
            "---\ntype: synthesis\nmeta-type: conclusion\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nrelated: \"[[sarah-chen]]\"\n---\n\n# A\n",
        );
        let issues = fx.store_all();
        let issue = find(&issues, codes::WIKI_LINK_SHORT_FORM);
        assert!(issue.is_error());
        assert_eq!(issue.key.as_deref(), Some("related"));
    }

    #[test]
    fn unquoted_frontmatter_link_is_recognized() {
        // An UNQUOTED `[[...]]` parses in YAML as a nested sequence, not a
        // string. The validator must still see it as a wiki-link (text-based
        // extraction). A short-form custom field must report SHORT_FORM, and a
        // full-path one with a missing target must report BROKEN.
        let fx = Fixture::new();
        fx.write(
            "records/synthesis/short.md",
            "---\ntype: synthesis\nmeta-type: conclusion\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nrelated: [[sarah-chen]]\n---\n\n# A\n",
        );
        fx.write(
            "records/synthesis/broken.md",
            "---\ntype: synthesis\nmeta-type: conclusion\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nrelated: [[records/contacts/ghost]]\n---\n\n# A\n",
        );
        let issues = fx.store_all();
        assert!(
            issues.iter().any(|i| i.code == codes::WIKI_LINK_SHORT_FORM
                && i.file == Path::new("records/synthesis/short.md")
                && i.key.as_deref() == Some("related")),
            "unquoted short-form frontmatter link must be caught: {issues:#?}"
        );
        assert!(
            issues.iter().any(|i| i.code == codes::WIKI_LINK_BROKEN
                && i.file == Path::new("records/synthesis/broken.md")),
            "unquoted full-path frontmatter link to a missing file must be caught: {issues:#?}"
        );
    }

    #[test]
    fn short_form_in_declared_link_field_is_prefix_mismatch_not_double_reported() {
        // A short-form value in a *declared* link field (a `### contact` schema
        // with `company link to records/companies/`) is SCHEMA_LINK_PREFIX_MISMATCH
        // (the target isn't under the prefix), and must NOT also be reported as a
        // bare WIKI_LINK_SHORT_FORM — the schema path owns that field once.
        let mut fx = Fixture::new();
        fx.config.schemas.insert(
            "contact".into(),
            Schema {
                fields: vec![FieldSpec {
                    name: "company".into(),
                    link_prefix: Some(PathBuf::from("records/companies")),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\ncompany: \"[[northstar]]\"\n---\n\n# A\n",
        );
        let issues = fx.store_all();
        let issue = find(&issues, codes::SCHEMA_LINK_PREFIX_MISMATCH);
        assert_eq!(issue.key.as_deref(), Some("company"));
        // The same link must NOT also be double-reported via the generic path.
        assert!(
            !issues
                .iter()
                .any(|i| i.code == codes::WIKI_LINK_SHORT_FORM
                    && i.key.as_deref() == Some("company")),
            "schema link fields are checked once, by the schema path: {issues:#?}"
        );
    }

    #[test]
    fn schema_link_field_with_md_extension_still_warns() {
        let mut fx = Fixture::new();
        fx.config.schemas.insert(
            "contact".into(),
            Schema {
                fields: vec![FieldSpec {
                    name: "company".into(),
                    link_prefix: Some(PathBuf::from("records/companies")),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        fx.write(
            "records/companies/acme.md",
            "---\ntype: company\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: Acme\nname: Acme\n---\n\n# Acme\n",
        );
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\ncompany: \"[[records/companies/acme.md]]\"\n---\n\n# A\n",
        );
        let issues = fx.store_all();
        let issue = issues
            .iter()
            .find(|i| {
                i.code == codes::WIKI_LINK_HAS_EXTENSION && i.key.as_deref() == Some("company")
            })
            .unwrap_or_else(|| panic!("schema link extension warning missing: {issues:#?}"));
        assert_eq!(issue.severity, Severity::Warning);
        assert!(
            !issues
                .iter()
                .any(|i| i.code == codes::WIKI_LINK_BROKEN && i.key.as_deref() == Some("company")),
            "extensionless existence check should still find acme.md: {issues:#?}"
        );
    }

    // ── schema: explicit DB.md schema (required / shape / enum) ───────────────

    #[test]
    fn explicit_schema_required_shape_enum() {
        let fx = {
            let mut fx = Fixture::new();
            // contact schema: name required, email required+email shape,
            // status enum: active|inactive
            let schema = Schema {
                fields: vec![
                    FieldSpec {
                        name: "name".into(),
                        required: true,
                        ..Default::default()
                    },
                    FieldSpec {
                        name: "email".into(),
                        required: true,
                        shape: Some(Shape::Email),
                        ..Default::default()
                    },
                    FieldSpec {
                        name: "status".into(),
                        enum_values: Some(vec!["active".into(), "inactive".into()]),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            };
            fx.config.schemas.insert("contact".into(), schema);
            fx
        };
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nemail: not-an-email\nstatus: archived\n---\n\n# A\n",
        );
        let issues = fx.store_all();
        // name absent → MISSING_REQUIRED
        assert!(
            issues
                .iter()
                .any(|i| i.code == codes::SCHEMA_MISSING_REQUIRED
                    && i.key.as_deref() == Some("name")),
            "{issues:#?}"
        );
        // email malformed → SHAPE_MISMATCH
        assert!(
            issues.iter().any(
                |i| i.code == codes::SCHEMA_SHAPE_MISMATCH && i.key.as_deref() == Some("email")
            ),
            "{issues:#?}"
        );
        // status archived not in enum → ENUM_VIOLATION
        assert!(
            issues
                .iter()
                .any(|i| i.code == codes::SCHEMA_ENUM_VIOLATION
                    && i.key.as_deref() == Some("status")),
            "{issues:#?}"
        );
    }

    #[test]
    fn schema_without_link_field_allows_plain_value() {
        // A `contact` schema with no `company` link field means a plain `company`
        // string is fine — schema enforcement is exactly what the store declares,
        // nothing implicit.
        let mut fx = Fixture::new();
        fx.config.schemas.insert(
            "contact".into(),
            Schema {
                fields: vec![FieldSpec {
                    name: "name".into(),
                    required: true,
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: Sarah\ncompany: \"Acme Co\"\n---\n\n# Sarah\n",
        );
        let issues = fx.store_all();
        assert!(
            !has(&issues, codes::SCHEMA_LINK_PREFIX_MISMATCH),
            "no declared link field for `company` → a plain value is fine: {issues:#?}"
        );
    }

    #[test]
    fn schema_link_field_plain_value_is_prefix_mismatch() {
        // The surviving link-enforcement path: a declared `link to <prefix>/`
        // field with a plain-string value is SCHEMA_LINK_PREFIX_MISMATCH.
        let mut fx = Fixture::new();
        fx.config.schemas.insert(
            "contact".into(),
            Schema {
                fields: vec![FieldSpec {
                    name: "company".into(),
                    link_prefix: Some(PathBuf::from("records/companies")),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: Sarah\ncompany: \"Acme Co\"\n---\n\n# Sarah\n",
        );
        let issues = fx.store_all();
        let issue = find(&issues, codes::SCHEMA_LINK_PREFIX_MISMATCH);
        assert_eq!(issue.key.as_deref(), Some("company"));
        assert!(issue
            .suggestion
            .as_deref()
            .unwrap()
            .contains("records/companies/"));
    }

    #[test]
    fn schema_shape_int_and_url_and_currency() {
        let mut fx = Fixture::new();
        fx.config.schemas.insert(
            "widget".into(),
            Schema {
                fields: vec![
                    FieldSpec {
                        name: "qty".into(),
                        shape: Some(Shape::Int),
                        ..Default::default()
                    },
                    FieldSpec {
                        name: "site".into(),
                        shape: Some(Shape::Url),
                        ..Default::default()
                    },
                    FieldSpec {
                        name: "price".into(),
                        shape: Some(Shape::Currency),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        );
        // `USD 100` is the corpus-realistic shape (an `expense.currency`-style
        // ISO code + amount). It must pass — it used to spuriously fail.
        fx.write(
            "records/widgets/ok.md",
            "---\ntype: widget\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: ok\nqty: 5\nsite: https://example.com\nprice: \"USD 1,234.50\"\n---\n\n# ok\n",
        );
        // `free` is non-numeric; `inf`/`NaN`/3-decimal used to slip through
        // because the old impl leaned on `f64::parse`. `price: inf` here guards
        // the under-rejection half of the finding.
        fx.write(
            "records/widgets/bad.md",
            "---\ntype: widget\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: bad\nqty: five\nsite: ftp://nope\nprice: inf\n---\n\n# bad\n",
        );
        let issues = fx.store_all();
        let bad_shape: Vec<_> = issues
            .iter()
            .filter(|i| {
                i.code == codes::SCHEMA_SHAPE_MISMATCH
                    && i.file == Path::new("records/widgets/bad.md")
            })
            .map(|i| i.key.clone().unwrap_or_default())
            .collect();
        assert!(bad_shape.contains(&"qty".to_string()), "{issues:#?}");
        assert!(bad_shape.contains(&"site".to_string()), "{issues:#?}");
        assert!(
            bad_shape.contains(&"price".to_string()),
            "inf must be rejected as currency: {issues:#?}"
        );
        assert!(
            !issues.iter().any(|i| i.code == codes::SCHEMA_SHAPE_MISMATCH
                && i.file == Path::new("records/widgets/ok.md")),
            "valid shapes (incl. `USD 1,234.50`) must not fire: {issues:#?}"
        );
    }

    #[test]
    fn schema_shape_or_enum_field_with_non_scalar_value_is_shape_mismatch() {
        let mut fx = Fixture::new();
        fx.config.schemas.insert(
            "contact".into(),
            Schema {
                fields: vec![
                    FieldSpec {
                        name: "email".into(),
                        required: true,
                        shape: Some(Shape::Email),
                        ..Default::default()
                    },
                    FieldSpec {
                        name: "status".into(),
                        enum_values: Some(vec!["active".into(), "inactive".into()]),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        );
        // A required EMAIL field and an ENUM field, each holding a LIST. Both
        // used to slip through entirely (`scalar_string` → None → the shape and
        // enum bodies silently no-op); now they flag SCHEMA_SHAPE_MISMATCH.
        fx.write(
            "records/contacts/bad.md",
            "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: bad\nemail:\n  - a@b.com\n  - c@d.com\nstatus:\n  - active\n---\n\n# bad\n",
        );
        let issues = fx.store_all();
        let mismatched: Vec<_> = issues
            .iter()
            .filter(|i| i.code == codes::SCHEMA_SHAPE_MISMATCH)
            .map(|i| i.key.clone().unwrap_or_default())
            .collect();
        assert!(
            mismatched.contains(&"email".to_string()),
            "list-valued required email must flag: {issues:#?}"
        );
        assert!(
            mismatched.contains(&"status".to_string()),
            "list-valued enum must flag: {issues:#?}"
        );
    }

    #[test]
    fn is_currency_accepts_codes_and_rejects_non_numeric() {
        // Symbols and 3-letter ISO codes both strip; plain numbers pass.
        for ok in [
            "100",
            "1234.56",
            "$1,234.50",
            "USD 100", // the finding's headline probe — used to be false
            "usd 100", // case-insensitive code
            "EUR 9.50",
            "£12",
            "¥1000",
            "-5.00", // signed amounts are real (refunds)
            "+5",
            "1,000,000",
        ] {
            assert!(is_currency(ok), "expected currency: {ok:?}");
        }
        // Non-numeric floats `f64::parse` would accept, and the > 2-decimal /
        // bare-code / exponent cases the docstring forbids.
        for bad in [
            "inf", "-inf", "infinity", "NaN", "nan",    // f64 accepts these; we must not
            "12.999", // 3 decimals
            "1.2345", // 4 decimals
            "USD",    // bare code, no amount
            "$",      // bare symbol
            "free", "", " ", "1e3",      // exponent form
            "1.",       // trailing dot, no fractional digits
            ".5",       // leading dot, no integer digits
            "1 000",    // space as separator is not a thousands separator
            "USDD 100", // 4-letter "code" must not strip
        ] {
            assert!(!is_currency(bad), "expected NOT currency: {bad:?}");
        }
    }

    // ── policies ───────────────────────────────────────────────────────────

    #[test]
    fn ignored_type_present_is_info() {
        let mut fx = Fixture::new();
        fx.config.ignored_types.push("temp".into());
        fx.write(
            "records/temps/x.md",
            "---\ntype: temp\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: a temp\n---\n\n# x\n",
        );
        let issues = fx.store_all();
        let issue = find(&issues, codes::POLICY_IGNORED_TYPE_PRESENT);
        assert_eq!(issue.severity, Severity::Info);
        assert!(!issue.is_error());
        assert!(issue.suggestion.as_deref().is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn conclusion_record_derived_from_ignored_type_warns() {
        let mut fx = Fixture::new();
        fx.config.ignored_types.push("temp".into());
        fx.write(
            "records/temps/x.md",
            "---\ntype: temp\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: a temp\n---\n\n# x\n",
        );
        // The policy now gates on `meta-type: conclusion` (not the retired
        // `type: wiki-page`): a conclusion record that derives from an
        // ignored-type record warns.
        fx.write(
            "records/synthesis/t.md",
            "---\ntype: synthesis\nmeta-type: conclusion\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: derived\nderived_from: \"[[records/temps/x]]\"\n---\n\n# t\n",
        );
        let issues = fx.store_all();
        let issue = find(&issues, codes::POLICY_IGNORED_TYPE_DERIVED);
        assert_eq!(issue.severity, Severity::Warning);
        assert_eq!(issue.key.as_deref(), Some("derived_from"));
        assert!(issue.suggestion.as_deref().is_some_and(|s| !s.is_empty()));
    }

    /// The shared `derived_from_ignored_type` entry point — the single
    /// policy-decision both `dbmd validate` (read) and `dbmd write` (write-time
    /// warning) now route through, so they cannot diverge. This pins its
    /// contract directly: the meta-type gate (now `meta-type: conclusion`, not
    /// the retired `type: wiki-page`), the empty-ignored-types gate, a positive
    /// match carrying the resolved target type, and a non-ignored target
    /// rejected.
    #[test]
    fn derived_from_ignored_type_is_the_shared_policy_decision() {
        let mut fx = Fixture::new();
        fx.config.ignored_types.push("secret".into());
        // An ignored-type record …
        fx.write(
            "records/secrets/s.md",
            "---\ntype: secret\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: hush\n---\n\n# s\n",
        );
        // … and a non-ignored record.
        fx.write(
            "records/contacts/c.md",
            "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: ok\nname: C\n---\n\n# c\n",
        );
        let store = fx.store();

        // Positive: a conclusion record deriving from the ignored-type record
        // matches, and the hit carries both the target (as written) and its
        // resolved type.
        let hit =
            derived_from_ignored_type(&store, "conclusion", std::iter::once("records/secrets/s"))
                .expect("conclusion → ignored-type record must match");
        assert_eq!(hit.target, "records/secrets/s");
        assert_eq!(hit.target_type, "secret");

        // Meta-type gate: a non-`conclusion` meta-type never triggers, even with
        // the same ignored-type target.
        assert_eq!(
            derived_from_ignored_type(&store, "fact", std::iter::once("records/secrets/s")),
            None,
            "only conclusion derivation is policed"
        );

        // Target gate: a conclusion deriving from a non-ignored record is fine.
        assert_eq!(
            derived_from_ignored_type(&store, "conclusion", std::iter::once("records/contacts/c")),
            None,
            "deriving from a non-ignored type is allowed"
        );

        // First match wins across multiple targets (here the second is the hit).
        let hit = derived_from_ignored_type(
            &store,
            "conclusion",
            ["records/contacts/c", "records/secrets/s"],
        )
        .expect("a later ignored-type target must still be found");
        assert_eq!(hit.target, "records/secrets/s");

        // Empty-policy gate: with no `### Ignored types`, nothing is policed.
        fx.config.ignored_types.clear();
        let store = fx.store();
        assert_eq!(
            derived_from_ignored_type(&store, "conclusion", std::iter::once("records/secrets/s")),
            None,
            "an empty ignored-types policy short-circuits"
        );
    }

    // ── duplicates ───────────────────────────────────────────────────────────

    #[test]
    fn dup_id_is_hard_error_with_related() {
        let fx = Fixture::new();
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\nid: shared\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: a\nname: A\n---\n\n# A\n",
        );
        fx.write(
            "records/contacts/b.md",
            "---\ntype: contact\nid: shared\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: b\nname: B\n---\n\n# B\n",
        );
        let issues = fx.store_all();
        // Reporting rule #1: ONE issue per collision group, keyed on the
        // lexicographically smallest path (`a.md`), partner in `related`.
        assert_eq!(
            count(&issues, codes::DUP_ID),
            1,
            "one issue per group: {issues:#?}"
        );
        let a = issues.iter().find(|i| i.code == codes::DUP_ID).unwrap();
        assert_eq!(a.file, PathBuf::from("records/contacts/a.md"));
        assert!(a.is_error());
        assert_eq!(a.key.as_deref(), Some("id"));
        assert_eq!(
            a.line,
            Some(3),
            "anchors to the `id` line on the reported file"
        );
        assert_eq!(a.related, vec![PathBuf::from("records/contacts/b.md")]);
    }

    #[test]
    fn dup_id_not_fired_in_working_set() {
        // DUP_* is an --all-only cross-file check; the working set must not run it.
        let fx = Fixture::new();
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\nid: shared\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: a\nname: A\n---\n\n# A\n",
        );
        fx.write(
            "records/contacts/b.md",
            "---\ntype: contact\nid: shared\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: b\nname: B\n---\n\n# B\n",
        );
        // Log says both changed since epoch, so they're in the working set.
        fx.write(
            "log.md",
            "---\ntype: log\n---\n\n## [2026-05-22 10:00] create | records/contacts/a\nx\n\n## [2026-05-22 10:01] create | records/contacts/b\nx\n",
        );
        let issues = validate_working_set(&fx.store(), None).unwrap();
        assert!(
            !has(&issues, codes::DUP_ID),
            "DUP_ID is --all only: {issues:#?}"
        );
    }

    #[test]
    fn dup_unique_key_single_field_is_warning() {
        let mut fx = Fixture::new();
        // contact declares `- unique: email`.
        fx.config.schemas.insert(
            "contact".into(),
            Schema {
                unique_keys: vec![vec!["email".into()]],
                ..Default::default()
            },
        );
        for (f, name) in [("a", "A"), ("b", "B")] {
            fx.write(
                &format!("records/contacts/{f}.md"),
                &format!("---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: s\nname: {name}\nemail: dup@x.com\n---\n\n# {name}\n"),
            );
        }
        let issues = fx.store_all();
        // One issue per group (rule #1), keyed on the smallest path, anchored to
        // the single `email` field.
        assert_eq!(count(&issues, codes::DUP_UNIQUE_KEY), 1);
        let dup = find(&issues, codes::DUP_UNIQUE_KEY);
        assert_eq!(dup.severity, Severity::Warning);
        assert_eq!(dup.file, PathBuf::from("records/contacts/a.md"));
        assert_eq!(dup.key.as_deref(), Some("email"));
        assert_eq!(dup.related, vec![PathBuf::from("records/contacts/b.md")]);
    }

    #[test]
    fn dup_unique_key_compound_and_clean_when_one_field_differs() {
        let mut fx = Fixture::new();
        // expense declares `- unique: date, amount, vendor` (a compound key).
        fx.config.schemas.insert(
            "expense".into(),
            Schema {
                unique_keys: vec![vec!["date".into(), "amount".into(), "vendor".into()]],
                ..Default::default()
            },
        );
        fx.write("records/companies/acme.md", "---\ntype: company\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: c\nname: Acme\n---\n# A\n");
        let exp = |f: &str, amount: &str| {
            format!(
            "---\ntype: expense\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: e\ndate: 2026-05-01\namount: {amount}\nvendor: \"[[records/companies/acme]]\"\n---\n\n# {f}\n"
        )
        };
        fx.write("records/expenses/e1.md", &exp("e1", "100"));
        fx.write("records/expenses/e2.md", &exp("e2", "100"));
        fx.write("records/expenses/e3.md", &exp("e3", "200")); // different amount
        let issues = fx.store_all();
        // One issue for the e1+e2 group (rule #1), keyed on the smallest path
        // (e1) with e2 in `related`; e3 differs on amount and never appears.
        assert_eq!(
            count(&issues, codes::DUP_UNIQUE_KEY),
            1,
            "only e1+e2 collide, one issue: {issues:#?}"
        );
        let dup = find(&issues, codes::DUP_UNIQUE_KEY);
        assert_eq!(dup.file, PathBuf::from("records/expenses/e1.md"));
        assert_eq!(
            dup.line,
            Some(1),
            "compound-key collision anchors to line 1"
        );
        assert_eq!(dup.related, vec![PathBuf::from("records/expenses/e2.md")]);
        assert!(
            !issues.iter().any(|i| i.code == codes::DUP_UNIQUE_KEY
                && i.related.contains(&PathBuf::from("records/expenses/e3.md"))),
            "e3 differs on amount and must not collide: {issues:#?}"
        );
    }

    #[test]
    fn dup_unique_key_list_field_is_order_independent() {
        let mut fx = Fixture::new();
        // meeting declares `- unique: date, attendees`; the list field is a set.
        fx.config.schemas.insert(
            "meeting".into(),
            Schema {
                unique_keys: vec![vec!["date".into(), "attendees".into()]],
                ..Default::default()
            },
        );
        fx.write("records/contacts/a.md", &valid_contact("a"));
        fx.write("records/contacts/b.md", &valid_contact("b"));
        let m = |f: &str, order: &str| {
            let attendees = if order == "ab" {
                "  - [[records/contacts/a]]\n  - [[records/contacts/b]]"
            } else {
                "  - [[records/contacts/b]]\n  - [[records/contacts/a]]"
            };
            format!(
                "---\ntype: meeting\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: m\ndate: 2026-05-01\nattendees:\n{attendees}\n---\n\n# {f}\n"
            )
        };
        fx.write("records/meetings/m1.md", &m("m1", "ab"));
        fx.write("records/meetings/m2.md", &m("m2", "ba"));
        let issues = fx.store_all();
        // The attendee SET is order-independent, so m1 (ab) and m2 (ba) collide
        // → a single issue on the smaller path.
        assert_eq!(
            count(&issues, codes::DUP_UNIQUE_KEY),
            1,
            "same date + same attendee set (any order) collide as one issue: {issues:#?}"
        );
        let dup = find(&issues, codes::DUP_UNIQUE_KEY);
        assert_eq!(dup.file, PathBuf::from("records/meetings/m1.md"));
        assert_eq!(dup.related, vec![PathBuf::from("records/meetings/m2.md")]);
    }

    // ── indexes ───────────────────────────────────────────────────────────────

    #[test]
    fn missing_indexes_at_all_three_levels() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("a"));
        let issues = fx.store_all();
        // root, layer (records), and type-folder (records/contacts) all missing.
        // The type-folder INDEX_MISSING is keyed on the FOLDER path (not its
        // would-be index.md), per the field convention `EXPECTED` pins.
        let missing_files: BTreeSet<PathBuf> = issues
            .iter()
            .filter(|i| i.code == codes::INDEX_MISSING)
            .map(|i| i.file.clone())
            .collect();
        assert!(
            missing_files.contains(&PathBuf::from("index.md")),
            "{issues:#?}"
        );
        assert!(
            missing_files.contains(&PathBuf::from("records/index.md")),
            "{issues:#?}"
        );
        assert!(
            missing_files.contains(&PathBuf::from("records/contacts")),
            "{issues:#?}"
        );
        // When the index.md is entirely absent we do NOT additionally fire
        // INDEX_JSONL_MISSING — one INDEX_MISSING covers the folder (rule #4).
        assert!(!has(&issues, codes::INDEX_JSONL_MISSING), "{issues:#?}");
    }

    #[test]
    fn index_stale_entry_and_missing_entry() {
        let fx = Fixture::new();
        fx.write(
            "records/contacts/present.md",
            &valid_contact("present contact"),
        );
        // Indexes for the parents (root/layer) present so we isolate type-folder.
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        // Type-folder index lists a GHOST (stale) and omits `present` (missing).
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/ghost]] — gone\n",
        );
        fx.write("records/contacts/index.jsonl", "{\"path\":\"records/contacts/present.md\",\"type\":\"contact\",\"summary\":\"present contact\"}\n");
        let issues = fx.store_all();
        let stale = find(&issues, codes::INDEX_STALE_ENTRY);
        assert!(stale.message.contains("ghost"));
        assert!(stale.is_error());
        let missing = find(&issues, codes::INDEX_MISSING_ENTRY);
        assert!(
            missing.message.contains("present.md"),
            "{}",
            missing.message
        );
    }

    #[test]
    fn index_md_entry_with_traversal_path_is_stale_not_probe() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("a"));
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/../../ghost]] — unsafe\n",
        );
        fx.write(
            "records/contacts/index.jsonl",
            "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"a\"}\n",
        );
        let issues = fx.store_all();
        let stale = find(&issues, codes::INDEX_STALE_ENTRY);
        assert!(stale.message.contains("not a safe store-relative path"));
    }

    #[test]
    fn index_summary_mismatch() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("the real summary"));
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/a]] — a STALE summary\n",
        );
        fx.write("records/contacts/index.jsonl", "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"the real summary\"}\n");
        let issues = fx.store_all();
        let issue = find(&issues, codes::INDEX_SUMMARY_MISMATCH);
        assert!(issue.is_error());
        assert_eq!(issue.related, vec![PathBuf::from("records/contacts/a.md")]);
    }

    #[test]
    fn index_summary_match_passes() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("matching summary"));
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/a]] — matching summary\n",
        );
        fx.write("records/contacts/index.jsonl", "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"matching summary\"}\n");
        let issues = fx.store_all();
        assert!(!has(&issues, codes::INDEX_SUMMARY_MISMATCH), "{issues:#?}");
    }

    #[test]
    fn index_entry_with_tag_suffix_matches_summary() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("clean summary"));
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        // Entry carries the renderer's `  ·  #tag` suffix (the EXACT double-spaced
        // delimiter `crate::index::format_md_entry` emits for a tagged file),
        // which must be stripped before comparing against the file's summary.
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/a]] — clean summary  ·  #customer\n",
        );
        fx.write("records/contacts/index.jsonl", "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"clean summary\"}\n");
        let issues = fx.store_all();
        assert!(
            !has(&issues, codes::INDEX_SUMMARY_MISMATCH),
            "tag suffix should be stripped: {issues:#?}"
        );
    }

    #[test]
    fn index_entry_single_spaced_middot_tail_is_part_of_summary() {
        // Regression (the finding): a tagless file whose `summary` legitimately
        // ends in a single-spaced ` · #word` tail round-trips through `index
        // rebuild` verbatim (the renderer appends NO `  ·  #tag` block, since the
        // file has no tags). The validator must NOT mistake that single-spaced
        // tail for the renderer's tag suffix, or it reports a spurious — and
        // unfixable — INDEX_SUMMARY_MISMATCH on a freshly rebuilt store.
        let fx = Fixture::new();
        fx.write(
            "records/contacts/a.md",
            &valid_contact("Standup notes · #standup"),
        );
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/a]] — Standup notes · #standup\n",
        );
        fx.write("records/contacts/index.jsonl", "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"Standup notes · #standup\"}\n");
        let issues = fx.store_all();
        assert!(
            !has(&issues, codes::INDEX_SUMMARY_MISMATCH),
            "a single-spaced middot tail is part of the summary, not a tag block: {issues:#?}"
        );
    }

    #[test]
    fn index_jsonl_desync_missing_file_in_jsonl() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("a"));
        fx.write("records/contacts/b.md", &valid_contact("b"));
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (2 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/a]] — a\n- [[records/contacts/b]] — b\n",
        );
        // jsonl only lists `a` → `b` is a desync (the twin must be complete).
        fx.write(
            "records/contacts/index.jsonl",
            "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"a\"}\n",
        );
        let issues = fx.store_all();
        let desync = find(&issues, codes::INDEX_JSONL_DESYNC);
        assert!(desync.message.contains("b.md"), "{}", desync.message);
    }

    #[test]
    fn index_jsonl_desync_record_points_at_missing_file() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("a"));
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/a]] — a\n",
        );
        fx.write(
            "records/contacts/index.jsonl",
            "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"a\"}\n{\"path\":\"records/contacts/ghost.md\",\"type\":\"contact\",\"summary\":\"x\"}\n",
        );
        let issues = fx.store_all();
        assert!(
            issues
                .iter()
                .any(|i| i.code == codes::INDEX_JSONL_DESYNC && i.message.contains("ghost.md")),
            "{issues:#?}"
        );
    }

    #[test]
    fn index_jsonl_record_with_traversal_path_is_desync_not_probe() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("a"));
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/a]] — a\n",
        );
        fx.write(
            "records/contacts/index.jsonl",
            "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"a\"}\n{\"path\":\"records/contacts/../../ghost.md\",\"type\":\"contact\",\"summary\":\"x\"}\n",
        );
        let issues = fx.store_all();
        assert!(
            issues.iter().any(|i| i.code == codes::INDEX_JSONL_DESYNC
                && i.message.contains("not a safe store-relative path")),
            "{issues:#?}"
        );
    }

    #[test]
    fn index_jsonl_stale_summary() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("real summary"));
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/a]] — real summary\n",
        );
        // jsonl summary disagrees with the file frontmatter.
        fx.write(
            "records/contacts/index.jsonl",
            "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"OUTDATED\"}\n",
        );
        let issues = fx.store_all();
        let stale = find(&issues, codes::INDEX_JSONL_STALE);
        assert_eq!(stale.related, vec![PathBuf::from("records/contacts/a.md")]);
        assert!(stale.key.as_deref().unwrap().contains("summary"));
    }

    /// The whole point of `INDEX_JSONL_STALE`: a sidecar field the query/search
    /// path actually reads (`email`, `domain`, the `(date,amount,vendor)` dedup
    /// tuple, `tags`, `updated`, `links`, `company` …) that disagrees with the
    /// `.md` is STALE — even when `summary` and `type` are perfectly correct.
    /// Pre-fix the validator only diffed summary+type, so a sidecar with a wrong
    /// `email` validated clean and answered `--where email=…` with a phantom
    /// value present in no file. This is the direct regression guard.
    #[test]
    fn index_jsonl_stale_queryable_field_email() {
        let fx = Fixture::new();
        let contact = "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: \"a contact\"\nname: A\nemail: real@correct.com\n---\n\n# A\n";
        fx.write("records/contacts/a.md", contact);
        // Start from the canonical, fully-correct sidecar set …
        fx.rebuild_indexes();
        let jsonl_path = fx.dir.path().join("records/contacts/index.jsonl");
        let good = fs::read_to_string(&jsonl_path).unwrap();
        // sanity: the canonical store is clean (no STALE on a fresh rebuild).
        assert!(
            !has(&fx.store_all(), codes::INDEX_JSONL_STALE),
            "freshly-rebuilt sidecar must not be stale"
        );
        // … then desync ONLY the email so it's the single differing field.
        assert!(
            good.contains("real@correct.com"),
            "sidecar projects email: {good}"
        );
        fx.write(
            "records/contacts/index.jsonl",
            &good.replace("real@correct.com", "STALE-WRONG@evil.com"),
        );

        let issues = fx.store_all();
        let stale = find(&issues, codes::INDEX_JSONL_STALE);
        assert_eq!(stale.related, vec![PathBuf::from("records/contacts/a.md")]);
        // The mismatch is reported precisely on `email`, and summary/type — which
        // still match — are NOT named.
        let key = stale.key.as_deref().unwrap();
        assert!(
            key.contains("email"),
            "expected `email` in stale key, got {key:?}"
        );
        assert!(!key.contains("summary"), "summary still matches: {key:?}");
        assert!(!key.contains("type"), "type still matches: {key:?}");
    }

    /// Broaden the guard across the typed/list/timestamp projections at once:
    /// a wrong `tags`, `updated`, and a custom dedup field (`amount`) are each
    /// caught, with all three named in one issue.
    #[test]
    fn index_jsonl_stale_typed_and_list_fields() {
        let fx = Fixture::new();
        let expense = "---\ntype: expense\ncreated: 2026-05-20T08:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: \"office chairs\"\ntags: [furniture, q2]\namount: 1299\nvendor: Acme\ndate: 2026-05-20\n---\n\n# Expense\n";
        fx.write("records/expenses/e.md", expense);
        fx.rebuild_indexes();
        let jsonl_path = fx.dir.path().join("records/expenses/index.jsonl");
        let good = fs::read_to_string(&jsonl_path).unwrap();
        assert!(
            !has(&fx.store_all(), codes::INDEX_JSONL_STALE),
            "freshly-rebuilt sidecar must not be stale"
        );
        // Desync a list field (tags), a timestamp (updated), and a number (amount).
        let stale_line = good
            .replace("\"q2\"", "\"WRONG-TAG\"")
            .replace("2026-05-22T10:00:00-07:00", "2099-01-01T00:00:00-07:00")
            .replace("1299", "9999");
        fx.write("records/expenses/index.jsonl", &stale_line);

        let issues = fx.store_all();
        let stale = find(&issues, codes::INDEX_JSONL_STALE);
        let key = stale.key.as_deref().unwrap();
        for expected in ["amount", "tags", "updated"] {
            assert!(
                key.contains(expected),
                "expected `{expected}` in stale key, got {key:?}"
            );
        }
    }

    #[test]
    fn index_orphan_in_noncanonical_folder() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("a"));
        // Build the canonical indexes so they aren't reported as orphans.
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        fx.write("records/contacts/index.md", "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/a]] — a\n");
        fx.write(
            "records/contacts/index.jsonl",
            "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"a\"}\n",
        );
        // An index.md inside a sub-sub-folder (operator territory) is an orphan.
        fx.write(
            "records/contacts/subfolder/index.md",
            "---\ntype: index\nscope: type-folder\n---\n\n# stray\n",
        );
        let issues = fx.store_all();
        let orphan = find(&issues, codes::INDEX_ORPHAN);
        assert_eq!(orphan.severity, Severity::Warning);
        assert_eq!(
            orphan.file,
            PathBuf::from("records/contacts/subfolder/index.md")
        );
    }

    #[test]
    fn index_wrong_scope() {
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("a"));
        // Root index declares the wrong scope.
        fx.write("index.md", "---\ntype: index\nscope: layer\n---\n\n## Records\n- [[records/contacts/index|C]] (1 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        fx.write("records/contacts/index.md", "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/a]] — a\n");
        fx.write(
            "records/contacts/index.jsonl",
            "{\"path\":\"records/contacts/a.md\",\"type\":\"contact\",\"summary\":\"a\"}\n",
        );
        let issues = fx.store_all();
        let issue = find(&issues, codes::INDEX_WRONG_SCOPE);
        assert_eq!(issue.severity, Severity::Warning);
        assert_eq!(issue.file, PathBuf::from("index.md"));
    }

    #[test]
    fn capped_type_folder_index_does_not_flag_missing_entries() {
        // Over the 500-entry cap, omitted entries are expected, not an error.
        let fx = Fixture::new();
        for i in 0..501 {
            fx.write(
                &format!("records/contacts/c{i:04}.md"),
                &valid_contact(&format!("contact {i}")),
            );
        }
        fx.write("index.md", "---\ntype: index\nscope: root\n---\n\n## Records\n- [[records/contacts/index|C]] (501 files)\n");
        fx.write(
            "records/index.md",
            "---\ntype: index\nscope: layer\nfolder: records\n---\n# r\n",
        );
        // Type-folder index lists only ONE entry + a More footer.
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\nscope: type-folder\nfolder: records/contacts\n---\n\n- [[records/contacts/c0000]] — contact 0\n\n## More\n\nThis folder has 501 files.\n",
        );
        // jsonl must still be complete — write all 501 lines.
        let mut jsonl = String::new();
        for i in 0..501 {
            jsonl.push_str(&format!(
                "{{\"path\":\"records/contacts/c{i:04}.md\",\"type\":\"contact\",\"summary\":\"contact {i}\"}}\n"
            ));
        }
        fx.write("records/contacts/index.jsonl", &jsonl);
        let issues = fx.store_all();
        assert!(
            !has(&issues, codes::INDEX_MISSING_ENTRY),
            "over the cap, missing browse entries are expected: {issues:#?}"
        );
        // But the jsonl is complete → no desync.
        assert!(
            !has(&issues, codes::INDEX_JSONL_DESYNC),
            "{:#?}",
            issues
                .iter()
                .filter(|i| i.code == codes::INDEX_JSONL_DESYNC)
                .collect::<Vec<_>>()
        );
    }

    // ── log ────────────────────────────────────────────────────────────────

    #[test]
    fn log_bad_timestamp_unknown_kind_out_of_order() {
        let fx = Fixture::new();
        fx.write(
            "log.md",
            concat!(
                "---\ntype: log\n---\n\n# Log\n\n",
                "## [2026-05-27 10:00] create | records/contacts/a\nx\n\n",
                "## [2026-05-27 09:00] update | records/contacts/b\nx\n\n", // out of order
                "## [2026-05-27 11:00] frobnicate | records/contacts/c\nx\n\n", // unknown kind
                "## [not-a-date] create | records/contacts/d\nx\n",         // bad timestamp
            ),
        );
        let issues = fx.store_all();
        assert!(has(&issues, codes::LOG_OUT_OF_ORDER), "{issues:#?}");
        assert_eq!(
            find(&issues, codes::LOG_OUT_OF_ORDER).severity,
            Severity::Warning
        );
        let unknown = find(&issues, codes::LOG_UNKNOWN_KIND);
        assert_eq!(unknown.severity, Severity::Warning);
        assert!(unknown.message.contains("frobnicate"));
        assert!(unknown
            .suggestion
            .as_deref()
            .is_some_and(|s| s.contains("create")));
        let bad = find(&issues, codes::LOG_BAD_TIMESTAMP);
        assert!(bad.is_error());
    }

    #[test]
    fn log_validate_entry_without_object_is_well_formed() {
        let fx = Fixture::new();
        fx.write(
            "log.md",
            "---\ntype: log\n---\n\n## [2026-05-27 10:00] validate\nPASS\n",
        );
        let issues = fx.store_all();
        assert!(!has(&issues, codes::LOG_BAD_TIMESTAMP), "{issues:#?}");
        assert!(!has(&issues, codes::LOG_UNKNOWN_KIND), "{issues:#?}");
    }

    #[test]
    fn log_in_order_is_clean() {
        let fx = Fixture::new();
        fx.write(
            "log.md",
            concat!(
                "---\ntype: log\n---\n\n",
                "## [2026-05-27 10:00] create | records/contacts/a\nx\n\n",
                "## [2026-05-27 10:05] update | records/contacts/a\nx\n",
            ),
        );
        let issues = fx.store_all();
        assert!(!has(&issues, codes::LOG_OUT_OF_ORDER), "{issues:#?}");
    }

    #[test]
    fn log_not_checked_in_working_set() {
        // log.md ordering is an --all-only check.
        let fx = Fixture::new();
        fx.write(
            "log.md",
            concat!(
                "---\ntype: log\n---\n\n",
                "## [2026-05-27 10:00] create | records/contacts/a\nx\n\n",
                "## [2026-05-27 09:00] update | records/contacts/a\nx\n",
            ),
        );
        let issues = validate_working_set(&fx.store(), None).unwrap();
        assert!(
            !has(&issues, codes::LOG_OUT_OF_ORDER),
            "log ordering is --all only: {issues:#?}"
        );
    }

    // ── working-set scoping ───────────────────────────────────────────────────

    #[test]
    fn working_set_validates_only_changed_files() {
        let fx = Fixture::new();
        // `dirty` has a bad timestamp; `clean_but_unlogged` also does but is NOT
        // in the log → working set must skip it.
        fx.write(
            "records/contacts/dirty.md",
            "---\ntype: contact\ncreated: BAD\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\n---\n\n# A\n",
        );
        fx.write(
            "records/contacts/unlogged.md",
            "---\ntype: contact\ncreated: ALSO-BAD\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: B\n---\n\n# B\n",
        );
        fx.write(
            "log.md",
            "---\ntype: log\n---\n\n## [2026-05-22 10:00] update | records/contacts/dirty\nedited\n",
        );
        let issues = validate_working_set(&fx.store(), None).unwrap();
        assert!(
            issues.iter().any(|i| i.code == codes::FM_BAD_TIMESTAMP
                && i.file == Path::new("records/contacts/dirty.md")),
            "{issues:#?}"
        );
        assert!(
            !issues
                .iter()
                .any(|i| i.file == Path::new("records/contacts/unlogged.md")),
            "unlogged file must not be in the working set: {issues:#?}"
        );
    }

    #[test]
    fn working_set_includes_incoming_linkers_to_changed_path() {
        let fx = Fixture::new();
        // `changed` was renamed/removed (logged). `linker` points at it with a
        // now-broken link and was NOT itself logged — but must be pulled in.
        fx.write(
            "records/profiles/linker.md",
            "---\ntype: profile\nmeta-type: conclusion\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: links to a removed page\n---\n\nSee [[records/contacts/changed]].\n",
        );
        // `changed.md` does NOT exist on disk (removed).
        fx.write(
            "log.md",
            "---\ntype: log\n---\n\n## [2026-05-22 10:00] delete | records/contacts/changed\nremoved\n",
        );
        let issues = validate_working_set(&fx.store(), None).unwrap();
        assert!(
            issues.iter().any(|i| i.code == codes::WIKI_LINK_BROKEN
                && i.file == Path::new("records/profiles/linker.md")),
            "incoming linker to a removed path must be validated: {issues:#?}"
        );
    }

    #[test]
    fn working_set_respects_explicit_since_cutoff() {
        let fx = Fixture::new();
        fx.write(
            "records/contacts/old.md",
            "---\ntype: contact\ncreated: BAD\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\n---\n\n# A\n",
        );
        fx.write(
            "records/contacts/new.md",
            "---\ntype: contact\ncreated: BAD\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: B\n---\n\n# B\n",
        );
        fx.write(
            "log.md",
            concat!(
                "---\ntype: log\n---\n\n",
                "## [2026-05-20 10:00] update | records/contacts/old\nx\n\n",
                "## [2026-05-25 10:00] update | records/contacts/new\nx\n",
            ),
        );
        // Cutoff after `old` but before `new`.
        let since = DateTime::parse_from_rfc3339("2026-05-22T00:00:00+00:00").unwrap();
        let issues = validate_working_set(&fx.store(), Some(since)).unwrap();
        assert!(
            issues
                .iter()
                .any(|i| i.file == Path::new("records/contacts/new.md")),
            "{issues:#?}"
        );
        assert!(
            !issues
                .iter()
                .any(|i| i.file == Path::new("records/contacts/old.md")),
            "old change is before the cutoff: {issues:#?}"
        );
    }

    #[test]
    fn working_set_default_since_is_last_validate_entry() {
        let fx = Fixture::new();
        // `before` changed before the last validate; `after` changed after.
        fx.write(
            "records/contacts/before.md",
            "---\ntype: contact\ncreated: BAD\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\n---\n\n# A\n",
        );
        fx.write(
            "records/contacts/after.md",
            "---\ntype: contact\ncreated: BAD\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: B\n---\n\n# B\n",
        );
        fx.write(
            "log.md",
            concat!(
                "---\ntype: log\n---\n\n",
                "## [2026-05-20 10:00] update | records/contacts/before\nx\n\n",
                "## [2026-05-21 10:00] validate\nPASS\n\n",
                "## [2026-05-22 10:00] update | records/contacts/after\nx\n",
            ),
        );
        let issues = validate_working_set(&fx.store(), None).unwrap();
        assert!(
            issues
                .iter()
                .any(|i| i.file == Path::new("records/contacts/after.md")),
            "{issues:#?}"
        );
        assert!(
            !issues
                .iter()
                .any(|i| i.file == Path::new("records/contacts/before.md")),
            "change before the last validate entry is outside the default window: {issues:#?}"
        );
    }

    // ── ordering / determinism ────────────────────────────────────────────────

    #[test]
    fn issues_are_sorted_by_file_then_line() {
        let fx = Fixture::new();
        fx.write("records/profiles/z.md", "---\ntype: profile\nmeta-type: conclusion\ncreated: BAD\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\n---\n\nbody\n");
        fx.write("records/profiles/a.md", "---\ntype: profile\nmeta-type: conclusion\ncreated: BAD\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\n---\n\nbody\n");
        let issues = fx.store_all();
        let files: Vec<&PathBuf> = issues.iter().map(|i| &i.file).collect();
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(
            files, sorted,
            "issues must be emitted in a stable file order"
        );
    }

    // ── boundaries: codes validate must NOT emit ──────────────────────────────

    #[test]
    fn frozen_page_is_not_a_validate_error() {
        // POLICY_FROZEN_PAGE is a *write-time* refusal, never a validate finding.
        // A clean file listed in `### Frozen pages` must validate clean.
        let mut fx = Fixture::new();
        fx.config
            .frozen_pages
            .push(PathBuf::from("records/decisions/d.md"));
        fx.write(
            "records/decisions/d.md",
            "---\ntype: decision\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: a finalized decision\n---\n\n# D\n",
        );
        let issues = fx.store_all();
        assert!(
            !has(&issues, codes::POLICY_FROZEN_PAGE),
            "frozen pages are enforced at write-time, not by validate: {issues:#?}"
        );
    }

    #[test]
    fn wiki_link_ambiguous_is_never_emitted_under_full_path_doctrine() {
        // The full-path doctrine makes ambiguity impossible; the defensive code
        // must never fire on a normal store.
        let fx = Fixture::new();
        fx.write("records/contacts/sarah-chen.md", &valid_contact("sarah"));
        let mut body = valid_contact("links to sarah");
        body.push_str("\nSee [[records/contacts/sarah-chen]].\n");
        fx.write("records/contacts/p.md", &body);
        let issues = fx.store_all();
        assert!(!has(&issues, codes::WIKI_LINK_AMBIGUOUS), "{issues:#?}");
    }

    // ── unknown-type / unknown-field passthrough ──────────────────────────────

    #[test]
    fn unknown_type_passes_through() {
        // A custom type is ambient context: it has a `type`, so no
        // FM_MISSING_TYPE, and with no matching schema there are no schema
        // errors. Only the universal contract (summary, timestamps) applies.
        let fx = Fixture::new();
        fx.write(
            "records/proposals/x.md",
            "---\ntype: proposal\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: a proposal\ncustom_field: anything\nbudget: 5000\n---\n\n# Proposal\n",
        );
        let issues = fx.store_all();
        assert!(!has(&issues, codes::FM_MISSING_TYPE), "{issues:#?}");
        assert!(!has(&issues, codes::SCHEMA_MISSING_REQUIRED), "{issues:#?}");
        assert!(!has(&issues, codes::SCHEMA_SHAPE_MISMATCH), "{issues:#?}");
        // The unknown fields don't trip anything.
        assert!(
            !issues
                .iter()
                .any(|i| i.key.as_deref() == Some("custom_field")
                    || i.key.as_deref() == Some("budget")),
            "unknown fields are ambient context: {issues:#?}"
        );
    }

    // ── find_links_to prefix-collision safety (working set) ───────────────────

    #[test]
    fn incoming_linker_scan_does_not_prefix_match() {
        // A changed `records/contacts/sarah` must NOT pull in a file that only
        // links to `records/contacts/sarah-chen` (a longer path sharing a prefix).
        let fx = Fixture::new();
        fx.write(
            "records/profiles/only-sarah-chen.md",
            "---\ntype: profile\nmeta-type: conclusion\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\n---\n\nSee [[records/contacts/sarah-chen]].\n",
        );
        // The log says `records/contacts/sarah` (the shorter path) changed.
        fx.write(
            "log.md",
            "---\ntype: log\n---\n\n## [2026-05-22 10:00] delete | records/contacts/sarah\nremoved\n",
        );
        let issues = validate_working_set(&fx.store(), None).unwrap();
        assert!(
            !issues
                .iter()
                .any(|i| i.file == Path::new("records/profiles/only-sarah-chen.md")),
            "a prefix-sharing link must not pull a file into the working set: {issues:#?}"
        );
    }

    #[test]
    fn working_set_does_not_flag_stale_catalog_index_as_wiki_link_broken() {
        // The working-set incoming-linker scan rides embedded-ripgrep
        // `Store::find_links_to`, which scans EVERY `.md` — so a type-folder
        // `index.md` listing a now-deleted target IS pulled into the working set.
        // But its entries are GENERATED catalog entries, not authored body links:
        // a dangling one is an `INDEX_STALE_ENTRY` ("run `dbmd index rebuild`"),
        // the job of `check_indexes` under `--all` — NOT a `WIKI_LINK_BROKEN`
        // ("create the target"), whose remedy would steer an agent to recreate
        // the very data it just deleted. The loop default must therefore NOT
        // body-link-check the derived catalog (index integrity is an O(store)
        // sweep concern, not an O(changed) loop concern). Adversarial review #11:
        // the prior behavior gave WIKI_LINK_BROKEN here while `--all` gave
        // INDEX_STALE_ENTRY for the identical condition — two codes, opposite
        // remedies, across the loop default vs the sweep.
        let fx = Fixture::new();
        // A catalog that still lists the deleted contact (a real, common stale
        // state after an out-of-band `delete`).
        fx.write(
            "records/contacts/index.md",
            "---\ntype: index\n---\n\n- [[records/contacts/sarah-chen]] — Sarah Chen\n",
        );
        // The log says `records/contacts/sarah-chen` was deleted.
        fx.write(
            "log.md",
            "---\ntype: log\n---\n\n## [2026-05-22 10:00] delete | records/contacts/sarah-chen\nremoved\n",
        );
        let issues = validate_working_set(&fx.store(), None).unwrap();
        assert!(
            !issues
                .iter()
                .any(|i| i.file == Path::new("records/contacts/index.md")
                    && i.code == codes::WIKI_LINK_BROKEN),
            "a stale catalog `index.md` entry must NOT be WIKI_LINK_BROKEN in the \
             working set (it is an INDEX_STALE_ENTRY under `--all`): {issues:#?}"
        );
    }

    #[test]
    fn incoming_linker_scan_covers_the_whole_changed_set_in_one_pass() {
        // CONTRACT (the O(changed × store) fix): the working-set scan finds
        // incoming linkers for EVERY changed object, and does so via the single
        // batch pass `Store::find_links_to_any` — not one full store read per
        // changed object. This test pins the behavior that makes the single-pass
        // correct: with two DISTINCT deleted targets, the linker to EACH is pulled
        // into the working set and flagged. A regression that scanned for only the
        // first/last changed object, or that dropped the batch union, would leave
        // one of the two broken links unreported and fail here.
        let fx = Fixture::new();
        // Linker A → deleted target #1 (in the body).
        fx.write(
            "records/profiles/refers-sarah.md",
            "---\ntype: profile\nmeta-type: conclusion\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\n---\n\nSee [[records/contacts/sarah-chen]].\n",
        );
        // Linker B → deleted target #2 (in a typed frontmatter field — an edge the
        // sidecar `links` projection would miss, which is why this must be a
        // content scan, not a sidecar read).
        fx.write(
            "records/meetings/2026/05/kickoff.md",
            "---\ntype: meeting\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: m\ndate: 2026-05-01\ncompany: \"[[records/companies/acme]]\"\n---\n\n# Kickoff\n",
        );
        // The log says BOTH targets were deleted in this window.
        fx.write(
            "log.md",
            "---\ntype: log\n---\n\n## [2026-05-22 10:00] delete | records/contacts/sarah-chen\nremoved\n\n## [2026-05-22 10:05] delete | records/companies/acme\nremoved\n",
        );

        let issues = validate_working_set(&fx.store(), None).unwrap();
        assert!(
            issues
                .iter()
                .any(|i| i.file == Path::new("records/profiles/refers-sarah.md")
                    && i.code == codes::WIKI_LINK_BROKEN),
            "linker to the FIRST deleted target must be pulled in and flagged: {issues:#?}"
        );
        assert!(
            issues.iter().any(
                |i| i.file == Path::new("records/meetings/2026/05/kickoff.md")
                    && i.code == codes::WIKI_LINK_BROKEN
            ),
            "linker to the SECOND deleted target (typed-field edge) must also be \
             pulled in and flagged — proves the scan covers the whole changed set, \
             not just one object: {issues:#?}"
        );
    }

    #[test]
    fn frontmatter_block_sequence_links_each_get_their_own_line() {
        // Each block-sequence wiki-link reports on its own source line.
        let fx = Fixture::new();
        // Neither target exists → two WIKI_LINK_BROKEN, on different lines.
        fx.write(
            "records/meetings/m.md",
            "---\ntype: meeting\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: m\ndate: 2026-05-01\nparticipants:\n  - [[records/contacts/ghost1]]\n  - [[records/contacts/ghost2]]\n---\n\n# M\n",
        );
        let issues = fx.store_all();
        let broken_lines: BTreeSet<Option<u32>> = issues
            .iter()
            .filter(|i| i.code == codes::WIKI_LINK_BROKEN)
            .map(|i| i.line)
            .collect();
        assert_eq!(
            broken_lines.len(),
            2,
            "two distinct broken-link lines: {issues:#?}"
        );
    }

    // ── Regression: null / non-scalar created/updated ────────────────────────

    #[test]
    fn null_created_is_missing_not_silently_passed() {
        // Regression: a present-but-`null` `created:` previously slipped past
        // both FM_MISSING_CREATED (only `!contains_key` was checked) and
        // FM_BAD_TIMESTAMP (`scalar_string(null)` is None → branch no-oped).
        let fx = Fixture::new();
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\ncreated:\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\n---\n\n# A\n",
        );
        let issues = fx.store_all();
        assert!(
            has(&issues, codes::FM_MISSING_CREATED),
            "null `created:` must read as missing: {issues:#?}"
        );
    }

    #[test]
    fn sequence_created_is_bad_timestamp() {
        // A non-scalar `created: [2026]` is not a timestamp string → FM_BAD_TIMESTAMP.
        let fx = Fixture::new();
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\ncreated: [2026]\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\n---\n\n# A\n",
        );
        let issues = fx.store_all();
        assert!(
            issues
                .iter()
                .any(|i| i.code == codes::FM_BAD_TIMESTAMP && i.key.as_deref() == Some("created")),
            "a sequence `created:` must be FM_BAD_TIMESTAMP: {issues:#?}"
        );
    }

    // ── Regression: schema required null / empty-collection ──────────────────

    #[test]
    fn required_field_null_or_empty_collection_is_missing() {
        // Regression: a plain required field (no shape/enum) holding YAML null
        // (`name:`), an empty list (`name: []`), or an empty mapping (`name: {}`)
        // previously validated with 0 issues — `scalar_string` returned None and
        // `.unwrap_or(false)` treated the value as non-empty.
        for value in ["", " []", " {}"] {
            let mut fx = Fixture::new();
            fx.config.schemas.insert(
                "contact".into(),
                Schema {
                    fields: vec![FieldSpec {
                        name: "name".into(),
                        required: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            );
            fx.write(
                "records/contacts/a.md",
                &format!(
                    "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname:{value}\n---\n\n# A\n"
                ),
            );
            let issues = fx.store_all();
            assert!(
                issues
                    .iter()
                    .any(|i| i.code == codes::SCHEMA_MISSING_REQUIRED
                        && i.key.as_deref() == Some("name")),
                "required `name:{value}` must be SCHEMA_MISSING_REQUIRED: {issues:#?}"
            );
        }
    }

    // ── Regression: WIKI_LINK_BROKEN on raw source files ─────────────────────

    #[test]
    fn wiki_link_to_raw_source_file_resolves() {
        // Regression: a body link to a raw `.eml`/`.pdf` source kept verbatim
        // under `sources/` was flagged WIKI_LINK_BROKEN because the existence
        // probe only ever stat'd `{bare}.md`. It must resolve the literal path.
        let fx = Fixture::new();
        fx.write("sources/emails/2026-05-22-elena.eml", "raw email bytes\n");
        fx.write(
            "records/contacts/a.md",
            "---\ntype: contact\ncreated: 2026-05-22T10:00:00-07:00\nupdated: 2026-05-22T10:00:00-07:00\nsummary: x\nname: A\n---\n\nSee [[sources/emails/2026-05-22-elena.eml]] for context.\n",
        );
        let issues = fx.store_all();
        assert!(
            !issues.iter().any(|i| i.code == codes::WIKI_LINK_BROKEN),
            "a link to an existing raw source file must not be broken: {issues:#?}"
        );
    }

    // ── Regression: unreadable (non-UTF-8) content file ──────────────────────

    #[test]
    fn non_utf8_content_file_is_reported() {
        // Regression: a content file with invalid UTF-8 bytes made
        // check_content_file return None silently, so the store passed with exit
        // 0. It must surface FM_UNREADABLE instead of passing vacuously.
        let fx = Fixture::new();
        let abs = fx.dir.path().join("records/notes/corrupt.md");
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(&abs, [0xFF, 0xFE, 0x00, 0x01]).unwrap();
        let issues = validate_working_set(&fx.store(), None).unwrap();
        assert!(
            has(&issues, codes::FM_UNREADABLE),
            "an unreadable content file must be reported, not silently skipped: {issues:#?}"
        );
    }

    // ── Regression: code-fence char/run tracking ─────────────────────────────

    #[test]
    fn tilde_fence_containing_backtick_fence_does_not_invert() {
        // Regression: a `~~~` block legally contains ``` lines (documenting a
        // backtick fence); a naive toggle inverted `in_fence` and checked the
        // demo `[[fake]]` inside the code block as a live link. The link inside
        // BOTH fences must be skipped.
        let body = "~~~markdown\n```\n[[fake-link]]\n```\n~~~\n";
        let links = extract_wiki_links(body);
        assert!(
            links.is_empty(),
            "wiki-link inside a nested code fence must be skipped: {links:?}"
        );
    }

    // ── Regression: --all skips in-layer `log/` folder ───────────────────────

    #[test]
    fn all_sweep_visits_in_layer_log_folder() {
        // Regression: `validate --all` pruned every dir named `log`, so a real
        // content folder like `records/log/` was invisible to the full sweep —
        // reporting FEWER errors than the default scope. A frontmatter-less file
        // there must still surface FM_MISSING_TYPE under --all.
        let fx = Fixture::new();
        fx.write("records/log/2026-06-01-pricing.md", "no frontmatter here\n");
        let issues = fx.store_all();
        assert!(
            has(&issues, codes::FM_MISSING_TYPE),
            "--all must validate files under an in-layer `log/` folder: {issues:#?}"
        );
    }

    // ── Regression: flow-form list with whitespace ───────────────────────────

    #[test]
    fn flow_form_link_list_with_spaces_is_flagged() {
        // Regression: `attendees: [ [[a]] ]` parses to the same nested-sequence
        // mis-encoding as `[[[a]]]` but evaded the literal `starts_with("[[[")`
        // text test. The value-based detector must catch the whitespace variant.
        let keys = detect_flow_form_link_lists("attendees: [ [[records/contacts/elena]] ]\n");
        assert!(
            keys.iter().any(|k| k == "attendees"),
            "spaced flow-form list must be detected: {keys:?}"
        );
    }

    // ── Regression: INDEX_SUMMARY_MISMATCH middot tail ───────────────────────

    #[test]
    fn middot_hashtag_summary_tail_round_trips() {
        // Regression: a tagless summary that legitimately ends in a single-spaced
        // ` · #word` tail round-trips through the renderer verbatim, but the loose
        // ` · ` strip mistook it for the tag block and reported a spurious,
        // unfixable INDEX_SUMMARY_MISMATCH. The strip must use the renderer's
        // exact double-spaced `  ·  ` delimiter.
        assert_eq!(
            extract_index_entry_summary("— Standup notes · #standup").as_deref(),
            Some("Standup notes · #standup"),
            "a single-spaced middot tail is part of the summary, not a tag block"
        );
        // The renderer's real double-spaced tag suffix IS still stripped.
        assert_eq!(
            extract_index_entry_summary("— Renewal champion  ·  #renewal #acme").as_deref(),
            Some("Renewal champion"),
            "the renderer's double-spaced `  ·  #tag` suffix is stripped"
        );
    }

    // ── Regression: shape Url / Email edge cases ─────────────────────────────

    #[test]
    fn url_shape_accepts_short_http_and_rejects_bare_scheme() {
        assert!(is_url("http://x"), "an 8-char http URL is valid");
        assert!(is_url("https://x"), "a 9-char https URL is valid");
        assert!(!is_url("http://"), "a bare scheme with no host is rejected");
        assert!(!is_url("https://"), "a bare https scheme is rejected");
    }

    #[test]
    fn email_shape_rejects_double_at() {
        assert!(!is_email("sarah@@acme.com"), "double-@ domain is rejected");
        assert!(!is_email("a@b@c.com"), "two @ signs are rejected");
        assert!(is_email("sarah@acme.com"), "a normal address still passes");
    }

    // ── Regression: working-set vs --all agree on log.md links ───────────────

    #[test]
    fn working_set_does_not_flag_log_md_body_links() {
        // Regression: the working-set incoming-linker scan runs root `log.md`
        // through the body wiki-link check, flagging a historical `[[deleted]]`
        // mention as WIKI_LINK_BROKEN — an error `--all` never reports and that
        // the append-only log can't have "fixed". The root meta files must be
        // excluded from the body link check, matching --all.
        let fx = Fixture::new();
        fx.write("records/contacts/a.md", &valid_contact("A"));
        fx.write(
            "log.md",
            "---\ntype: log\n---\n\n## [2026-06-01 10:00] delete | records/contacts/ghost\n\nRemoved [[records/contacts/ghost]] per cleanup.\n",
        );
        let issues = validate_working_set(&fx.store(), None).unwrap();
        assert!(
            !issues
                .iter()
                .any(|i| i.code == codes::WIKI_LINK_BROKEN
                    && i.file == std::path::Path::new("log.md")),
            "a broken wiki-link inside append-only log.md must not be flagged: {issues:#?}"
        );
    }

    // ── Regression: DB.md schema field lint ──────────────────────────────────

    #[test]
    fn schema_duplicate_field_name_is_flagged() {
        let mut fx = Fixture::new();
        fx.config.schemas.insert(
            "contact".into(),
            Schema {
                fields: vec![
                    FieldSpec {
                        name: "name".into(),
                        required: true,
                        ..Default::default()
                    },
                    FieldSpec {
                        name: "name".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        );
        let issues = fx.store_all();
        assert!(
            issues
                .iter()
                .any(|i| i.code == codes::DB_MD_SCHEMA_FIELD && i.key.as_deref() == Some("name")),
            "a duplicate schema field name must be flagged: {issues:#?}"
        );
    }

    #[test]
    fn schema_unknown_modifier_is_info() {
        let mut fx = Fixture::new();
        fx.config.schemas.insert(
            "contact".into(),
            Schema {
                fields: vec![FieldSpec {
                    name: "name".into(),
                    unknown_modifiers: vec!["requierd".into()],
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        let issues = fx.store_all();
        assert!(
            issues.iter().any(|i| i.code == codes::DB_MD_SCHEMA_FIELD
                && i.severity == Severity::Info
                && i.key.as_deref() == Some("name")),
            "an unrecognized schema modifier must surface as Info: {issues:#?}"
        );
    }

    /// Every code in `mod codes` must appear as a row in SPEC.md § Validation —
    /// the SPEC table is the declared "complete vocabulary" an agent branches on,
    /// and the module doc-comment promises this code implements "exactly those
    /// codes — no more, no fewer." This guards against the code/SPEC drift where a
    /// new validation code is added to the engine but never documented.
    #[test]
    fn every_code_constant_is_documented_in_spec() {
        // Parse the canonical constant *values* straight out of this module's
        // source, so a future `pub const X: &str = "X";` is covered with no test
        // edit. Format is uniform: `    pub const NAME: &str = "VALUE";`.
        let this_src = include_str!("validate.rs");
        let mut codes_in_module: Vec<String> = Vec::new();
        let mut in_codes_mod = false;
        for line in this_src.lines() {
            let t = line.trim();
            if t.starts_with("pub mod codes") {
                in_codes_mod = true;
                continue;
            }
            // The `mod codes` block ends at its closing brace at column 0.
            if in_codes_mod && line == "}" {
                break;
            }
            if in_codes_mod {
                if let Some(rest) = t.strip_prefix("pub const ") {
                    // rest = `NAME: &str = "VALUE";`
                    let value = rest
                        .split_once('=')
                        .map(|(_, v)| v.trim())
                        .and_then(|v| v.strip_prefix('"'))
                        .and_then(|v| v.strip_suffix("\";"))
                        .unwrap_or_else(|| panic!("unparseable code constant line: {line:?}"));
                    codes_in_module.push(value.to_string());
                }
            }
        }
        assert!(
            codes_in_module.len() >= 36,
            "parsed only {} code constants from `mod codes`; the parser likely \
             broke against a source-format change",
            codes_in_module.len()
        );

        // SPEC.md lives at the repo root, two levels up from this crate's manifest.
        let spec_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../SPEC.md");
        let spec = fs::read_to_string(&spec_path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", spec_path.display()));

        // Each code must appear as a SPEC § Validation table cell: `` | `CODE` | ``.
        let missing: Vec<&String> = codes_in_module
            .iter()
            .filter(|code| !spec.contains(&format!("| `{code}` |")))
            .collect();
        assert!(
            missing.is_empty(),
            "validation codes emitted by the engine but absent from SPEC.md \
             § Validation (the declared complete vocabulary): {missing:?}"
        );
    }

    // ── loose files (directly at a layer root, no type-folder) ───────────────

    const LOOSE_ALICE: &str = "---\ntype: contact\nid: alice\ncreated: 2026-06-01T08:00:00-07:00\nupdated: 2026-06-01T08:00:00-07:00\nsummary: Alice\n---\nbody\n";
    const LOOSE_BOB: &str = "---\ntype: contact\nid: bob\ncreated: 2026-06-01T08:00:00-07:00\nupdated: 2026-06-01T08:00:00-07:00\nsummary: Bob loose\n---\nbody\n";

    #[test]
    fn loose_file_catalogued_in_layer_jsonl_validates_clean() {
        let fx = Fixture::new();
        fx.write("records/contacts/alice.md", LOOSE_ALICE);
        fx.write("records/bob.md", LOOSE_BOB); // loose, directly under records/
        fx.rebuild_indexes();
        let issues = fx.store_all();
        assert!(
            issues.is_empty(),
            "a rebuilt store with a catalogued loose file must validate clean, got: {issues:?}"
        );
    }

    #[test]
    fn loose_file_with_missing_layer_jsonl_is_index_jsonl_missing() {
        let fx = Fixture::new();
        fx.write("records/contacts/alice.md", LOOSE_ALICE);
        fx.write("records/bob.md", LOOSE_BOB);
        fx.rebuild_indexes();
        // Simulate the layer sidecar going missing (a hand-deletion / bad sync).
        fs::remove_file(fx.dir.path().join("records/index.jsonl")).unwrap();
        let issues = fx.store_all();
        assert!(
            has(&issues, codes::INDEX_JSONL_MISSING),
            "a loose file with no layer index.jsonl must raise INDEX_JSONL_MISSING, got: {issues:?}"
        );
    }

    #[test]
    fn loose_only_store_validates_clean_without_a_rollup_index_md() {
        // A store whose ONLY content is a loose file (no type-folder anywhere):
        // rebuild writes the layer `index.jsonl` but no root/layer `index.md`
        // rollup — there is nothing to roll up. `validate --all` must accept that;
        // the rollup is required only when type-folders exist. (Regression: this
        // emitted two false INDEX_MISSING errors in 0.4.4.)
        let fx = Fixture::new();
        fx.write("records/solo.md", LOOSE_ALICE);
        fx.rebuild_indexes();
        assert!(
            !fx.dir.path().join("index.md").is_file(),
            "no root rollup index.md should exist for a loose-only store"
        );
        let issues = fx.store_all();
        assert!(
            issues.is_empty(),
            "a loose-only store must validate clean (catalog is the layer index.jsonl), got: {issues:?}"
        );
    }
}
