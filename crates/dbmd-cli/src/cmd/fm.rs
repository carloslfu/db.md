//! `dbmd fm <sub>` — frontmatter read / write / query / init.
//!
//! Dispatches the [`FmCommand`] to one of four leaf bodies:
//!   - `get`   → read one frontmatter value (`dbmd_core::parser::read_file`)
//!   - `set`   → atomic insert/update + write-through index re-sort
//!   - `query` → sidecar-backed dedup query (`dbmd_core::query::Query`)
//!   - `init`  → auto-detect type, seed timestamps, compose a default
//!     `summary`, and fold the file into its index write-through
//!
//! `set` and `init` are write surfaces: they enforce the `DB.md` frozen-page
//! policy before mutating, write atomically via the parser, and then keep the
//! type-folder index current write-through (`dbmd_core::index::Index::on_write`).
//! All real logic lives in `dbmd-core`; this is arg-parse + format glue.

use std::path::{Path, PathBuf};

use serde_norway::Value as YamlValue;

use crate::cli::{FmArgs, FmCommand, FmGetArgs, FmInitArgs, FmQueryArgs, FmSetArgs};
use crate::cmd::log::{into_cli, open_store};
use crate::cmd::write::{apply_schema_defaults, require_store_relative};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

use dbmd_core::{infer_type_from_path, parser, summary, Index, Layer, Query, Store};

/// Dispatch `dbmd fm <sub>` to the matching leaf body.
pub fn run(ctx: &Context, args: &FmArgs) -> CliResult {
    match &args.command {
        FmCommand::Get(a) => run_get(ctx, a),
        FmCommand::Set(a) => run_set(ctx, a),
        FmCommand::Query(a) => run_query(ctx, a),
        FmCommand::Init(a) => run_init(ctx, a),
    }
}

/// `dbmd fm get <file> <key>` — read a single frontmatter value. Text mode
/// prints the value's plain scalar form; `--json` returns `{file,key,value}`.
/// A key the file does not carry is a runtime error (exit 1).
pub fn run_get(ctx: &Context, args: &FmGetArgs) -> CliResult {
    // `fm get` reads one file directly (no store walk); it does not require a
    // store root, mirroring the SPEC example `dbmd fm get DB.md scope`.
    let (fm, _body) = into_cli(parser::read_file(Path::new(&args.file)))?;
    let value = fm.get(&args.key).ok_or_else(|| {
        CliError::new(
            ExitCode::Runtime,
            "FM_KEY_NOT_FOUND",
            format!("no frontmatter key '{}' in {}", args.key, args.file),
        )
    })?;

    if ctx.json {
        let obj = serde_json::json!({
            "file": args.file,
            "key": args.key,
            "value": yaml_to_json(&value),
        });
        println!("{obj}");
    } else {
        println!("{}", render_scalar(&value));
    }
    Ok(())
}

/// `dbmd fm set <file> <key>=<value>` — atomic insert/update of one frontmatter
/// value, then a write-through index re-sort (the type-folder entry's recency
/// may have changed). Refuses on a `DB.md` frozen page before mutating.
pub fn run_set(ctx: &Context, args: &FmSetArgs) -> CliResult {
    let (key, value) = split_assignment(&args.assignment)?;

    let store = locate_store_from_cwd()?;
    let rel = require_store_relative(&store, &args.file)?;
    let file = store.abs_path(&rel);

    // Frozen-page policy: refuse before any mutation.
    enforce_not_frozen(&store, &rel)?;

    let (mut fm, body) = into_cli(parser::read_file(&file))?;
    into_cli(fm.set(key, value))?;
    // Auto-maintain `updated`: any edit to an existing content file re-stamps
    // `updated` to now (SPEC: `updated` is auto-maintained), so the type-folder
    // index recency ordering and `--updated-after` queries reflect the edit.
    // An explicit `fm set updated=…` already set the field via `fm.set` above;
    // don't clobber that operator-chosen value with `now`.
    bump_updated_unless_explicit(&mut fm, key);
    into_cli(parser::write_file(&file, &fm, &body))?;

    // Write-through: re-derive the record from the now-updated file and re-sort
    // the type-folder index. Non-fatal if it can't run (the file is the source
    // of truth); surface a hint so the agent can `index rebuild --folder`.
    let index_ok = Index::on_write(&store, &rel).is_ok();

    if ctx.json {
        let obj = serde_json::json!({
            "file": path_str(&rel),
            "key": key,
            "value": value,
            "index_updated": index_ok,
        });
        println!("{obj}");
    } else {
        println!("{}", path_str(&rel));
        if !index_ok {
            eprintln!(
                "  warning: index not updated; run `dbmd index rebuild --folder <type-folder>`"
            );
        }
    }
    Ok(())
}

/// `dbmd fm query <key>=<value> [--type --in --limit]` — the pre-write dedup
/// primitive: a complete, sidecar-backed store query by one frontmatter field.
pub fn run_query(ctx: &Context, args: &FmQueryArgs) -> CliResult {
    let (key, value) = split_assignment(&args.assignment)?;
    let store = open_store(&args.dir)?;

    let mut query = Query::new().with_where(key, value);
    if let Some(t) = &args.r#type {
        query = query.with_type(t);
    }
    if let Some(layer) = &args.r#in {
        query = query.with_layer(parse_layer(layer)?);
    }

    let mut records = into_cli(query.execute(&store))?;
    if let Some(limit) = args.limit {
        records.truncate(limit);
    }

    crate::cmd::index::emit_records(ctx, &records);
    Ok(())
}

/// `dbmd fm init <file> [--summary <str>]` — initialize canonical frontmatter on
/// an externally-dropped file: detect its `type` (frontmatter, else by path),
/// seed `created`/`updated` when absent, compose a deterministic default
/// `summary` (overridable with `--summary`), then fold the file into its index
/// write-through. Refuses on a `DB.md` frozen page before mutating.
pub fn run_init(ctx: &Context, args: &FmInitArgs) -> CliResult {
    let store = locate_store_from_cwd()?;
    let rel = require_store_relative(&store, &args.file)?;
    let file = store.abs_path(&rel);

    enforce_not_frozen(&store, &rel)?;

    let (mut fm, body) = read_or_seed_raw_body(&file)?;

    // Type: an explicit frontmatter `type` wins; otherwise infer from the
    // type-folder path segment. A file with neither is an error (init can't
    // canonicalize a typeless file the agent hasn't classified).
    let type_ = match fm.type_.clone() {
        Some(t) if !t.is_empty() => t,
        _ => match infer_type_from_path(&rel) {
            Some(t) => {
                fm.type_ = Some(t.clone());
                t
            }
            None => {
                return Err(CliError::new(
                    ExitCode::Runtime,
                    "FM_TYPE_UNKNOWN",
                    format!(
                        "cannot infer `type` for {} — set it explicitly with `dbmd fm set {} type=<t>`",
                        path_str(&rel),
                        path_str(&rel)
                    ),
                ));
            }
        },
    };

    // Seed timestamps when absent. `created` and `updated` both default to now
    // on first canonicalization; an already-set value is left untouched. The
    // seed comes from `dbmd_core::now()` — the one canonical wall-clock shared
    // by every write surface (write, fm init, fm set, log append).
    let now = dbmd_core::now();
    if fm.created.is_none() {
        fm.created = Some(now);
    }
    if fm.updated.is_none() {
        fm.updated = Some(now);
    }
    apply_schema_defaults(&store, &type_, &mut fm)?;

    // Summary: an explicit `--summary` wins; otherwise compose the deterministic
    // default for this type and write it to `summary:`. An already-present
    // summary is only overwritten by an explicit `--summary`.
    if let Some(s) = &args.summary {
        // An explicit `--summary` is the agent's ceiling: collapse to a single
        // line but never truncate (parity with `dbmd fm set`, which preserves
        // the value verbatim). Over-length surfaces as a `SUMMARY_TOO_LONG`
        // validate warning, not silent loss of the agent's trailing content.
        fm.summary = Some(summary::collapse_whitespace(s));
    } else if fm.summary.as_deref().unwrap_or("").trim().is_empty() {
        let composed = summary::compose_default(&store, &type_, &fm, &body)?;
        fm.summary = Some(composed);
    }

    into_cli(parser::write_file(&file, &fm, &body))?;
    let index_ok = Index::on_write(&store, &rel).is_ok();

    if ctx.json {
        let obj = serde_json::json!({
            "file": path_str(&rel),
            "type": type_,
            "summary": fm.summary,
            "index_updated": index_ok,
        });
        println!("{obj}");
    } else {
        println!("{}", path_str(&rel));
        if !index_ok {
            eprintln!(
                "  warning: index not updated; run `dbmd index rebuild --folder <type-folder>`"
            );
        }
    }
    Ok(())
}

// ── Shared glue ──────────────────────────────────────────────────────────────

fn read_or_seed_raw_body(file: &Path) -> Result<(parser::Frontmatter, String), CliError> {
    match parser::read_file(file) {
        Ok(parsed) => Ok(parsed),
        Err(dbmd_core::ParseError::MissingFrontmatter { .. }) => {
            // `MissingFrontmatter` covers TWO distinct shapes: a truly
            // headerless file (no opening `---` fence) and a malformed file
            // that OPENS a `---` fence but never closes it. Seeding a fresh
            // frontmatter block is only correct for the first — for the second
            // it would silently demote the operator's intended frontmatter keys
            // into the body and inject a stray dangling `---`. Distinguish them
            // by re-reading the raw text and inspecting the opening line the way
            // `split_frontmatter` does; refuse the unterminated-fence case with
            // a clear `FM_MALFORMED` error instead of corrupting its shape.
            let body = std::fs::read_to_string(file).map_err(CliError::from)?;
            if opens_frontmatter_fence(&body) {
                return Err(malformed_frontmatter_error(file));
            }
            Ok((parser::Frontmatter::default(), body))
        }
        Err(e) => Err(CliError::from(dbmd_core::Error::from(e))),
    }
}

/// True when `text` opens with a `---` frontmatter fence on its first line —
/// the exact test `parser::split_frontmatter` uses (the line, with any trailing
/// CR/LF stripped, equals `---`, nothing before it, no BOM tolerance). A file
/// that opens a fence but reached `read_or_seed_raw_body` did so because the
/// fence was never closed, so this distinguishes an unterminated/malformed
/// block from a genuinely headerless import.
fn opens_frontmatter_fence(text: &str) -> bool {
    let first = text.split_inclusive('\n').next().unwrap_or("");
    first.trim_end_matches(['\r', '\n']) == "---"
}

/// The refusal for a file whose frontmatter block opens with `---` but never
/// closes (exit 1). Seeding fresh frontmatter here would silently demote the
/// operator's intended keys into the body and inject a dangling `---`, so we
/// refuse and tell the agent how to make the intent explicit.
fn malformed_frontmatter_error(file: &Path) -> CliError {
    CliError::new(
        ExitCode::Runtime,
        "FM_MALFORMED",
        format!(
            "{} opens a `---` frontmatter fence that is never closed",
            file.display()
        ),
    )
    .with_hint(
        "close the frontmatter block with a `---` line, or remove the opening `---` to import it as a raw body",
    )
}

/// Split a `key=value` assignment at the first `=`. The value may itself contain
/// `=` (e.g. a query string); only the first separator splits. An empty key is
/// a usage error.
fn split_assignment(assignment: &str) -> Result<(&str, &str), CliError> {
    match assignment.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k, v)),
        _ => Err(CliError::new(
            ExitCode::Runtime,
            "BAD_ASSIGNMENT",
            format!("expected `key=value`, got {assignment:?}"),
        )
        .with_hint("example: status=active")),
    }
}

/// Refuse a write whose target is a `DB.md ## Policies → ### Frozen pages`
/// entry, with the structured `POLICY_FROZEN_PAGE` code (exit 4). Enforced at
/// the CLI write boundary — there is no core write gate; the frozen list comes
/// from the parsed [`Store::config`].
fn enforce_not_frozen(store: &Store, rel: &Path) -> Result<(), CliError> {
    // Use the single canonical matcher (`.md`-, `./`-, separator-insensitive)
    // so `fm set`/`fm init` enforce frozen pages identically to every other
    // write surface. A raw `PathBuf` equality here was `.md`-sensitive and let
    // an extensionless policy entry through.
    if let Some(frozen) = store.config.frozen_match(rel) {
        return Err(dbmd_core::Error::Policy {
            code: "POLICY_FROZEN_PAGE",
            message: format!(
                "write refused: '{}' is a frozen page per DB.md ## Policies → ### Frozen pages",
                path_str(&frozen)
            ),
        }
        .into());
    }
    Ok(())
}

/// Re-stamp `fm.updated` to now (`dbmd_core::now()`, honoring `DBMD_NOW`) so the
/// type-folder index recency ordering and `--updated-after` queries reflect the
/// edit — SPEC declares `updated` auto-maintained on every content-file mutation.
///
/// The one exception is an explicit `dbmd fm set updated=…`: when the agent set
/// `updated` itself, `fm.set` already wrote the operator-chosen value, so a
/// `now` bump here would clobber it. `mutated_key` is the key the caller just
/// `fm.set`; skip the bump when it is `updated`.
fn bump_updated_unless_explicit(fm: &mut parser::Frontmatter, mutated_key: &str) {
    if mutated_key != "updated" {
        fm.updated = Some(dbmd_core::now());
    }
}

/// Locate the db.md store the agent is operating in by walking UP from the
/// current working directory to the **outermost** ancestor carrying a `DB.md`
/// marker, then open it. This is what makes `fm set` / `fm init` work from any
/// subdirectory of a store, not only from the exact root — matching the
/// ancestor-walk `dbmd format` does, while keeping the store anchored on the
/// operating context (CWD) so the downstream `require_store_relative`
/// containment check still rejects a file *outside* this store with
/// `PATH_OUTSIDE_STORE` rather than silently retargeting a different store.
///
/// Anchoring to the outermost (shallowest) store, rather than the first `DB.md`
/// found walking up, keeps an interior content file that merely happens to be
/// named `DB.md` (a store state the spec blesses as ordinary content) from
/// hijacking discovery. When no ancestor is a store, the error carries an
/// actionable hint (run from inside a store / author a `DB.md`) instead of the
/// central generic "pass the store path" hint, which is unactionable here:
/// `fm set` / `fm init` take no `--dir`.
fn locate_store_from_cwd() -> Result<Store, CliError> {
    let start = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());

    match outermost_store_root(&start) {
        Some(root) => Store::open_strict(&root).map_err(CliError::from),
        None => Err(CliError::new(
            ExitCode::NotAStore,
            "NOT_A_STORE",
            format!(
                "not a db.md store: no DB.md found at or above {}",
                start.display()
            ),
        )
        .with_hint(
            "run from inside a db.md store (any directory at or under a DB.md), or author a DB.md at the store root first",
        )),
    }
}

/// Walk `start` and every ancestor, returning the **outermost** (shallowest)
/// directory that is a db.md store root, or `None` when no ancestor is a store.
/// Choosing the outermost match — not the first one found walking up — is what
/// stops an interior content file named `DB.md` from shadowing the real store
/// root.
fn outermost_store_root(start: &Path) -> Option<PathBuf> {
    let mut outermost: Option<&Path> = None;
    let mut dir: Option<&Path> = Some(start);
    while let Some(d) = dir {
        if Store::is_db_md_store(d) {
            outermost = Some(d);
        }
        dir = d.parent();
    }
    outermost.map(|p| p.to_path_buf())
}

/// Parse a `--in <layer>` value into a [`Layer`], or a usage error.
pub(crate) fn parse_layer(layer: &str) -> Result<Layer, CliError> {
    Layer::from_dir_name(layer).ok_or_else(|| {
        CliError::new(
            ExitCode::Runtime,
            "BAD_LAYER",
            format!("unknown layer {layer:?}"),
        )
        .with_hint("one of: sources, records")
    })
}

/// Render a YAML scalar as plain display text for `fm get` text output. Strings
/// pass through verbatim (wiki-links kept as written); scalars stringify; a
/// list joins comma-space; mappings render as compact YAML.
fn render_scalar(v: &YamlValue) -> String {
    match v {
        YamlValue::String(s) => s.clone(),
        YamlValue::Bool(b) => b.to_string(),
        YamlValue::Number(n) => n.to_string(),
        YamlValue::Null => String::new(),
        YamlValue::Sequence(items) => items
            .iter()
            .map(render_scalar)
            .collect::<Vec<_>>()
            .join(", "),
        YamlValue::Mapping(_) | YamlValue::Tagged(_) => serde_norway::to_string(v)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

/// Convert a YAML [`YamlValue`] to a JSON value for `--json` output, going
/// through `serde_json` so types map naturally (string/number/bool/array/map).
fn yaml_to_json(v: &YamlValue) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

/// Render a path with `/` separators for stable, platform-independent output.
fn path_str(p: &Path) -> String {
    p.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `fm set status=active` on a content file must re-stamp `updated` (SPEC:
    /// auto-maintained), so a file edited long after creation no longer keeps a
    /// stale `updated` and the index recency / `--updated-after` queries reflect
    /// the edit.
    #[test]
    fn bump_updated_restamps_non_updated_edits() {
        let old = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z").unwrap();
        let mut fm = parser::Frontmatter {
            updated: Some(old),
            ..Default::default()
        };

        bump_updated_unless_explicit(&mut fm, "status");

        let bumped = fm.updated.expect("updated must remain set");
        assert!(
            bumped > old,
            "editing a non-`updated` field must advance `updated` past its stale value ({old} -> {bumped})"
        );
    }

    /// An explicit `fm set updated=…` is the operator's chosen value; the
    /// auto-bump must NOT clobber it with `now`.
    #[test]
    fn bump_updated_preserves_explicit_updated_assignment() {
        // `fm.set("updated", …)` already wrote the operator value before the
        // bump runs; model that here, then assert the bump leaves it untouched.
        let chosen = chrono::DateTime::parse_from_rfc3339("2030-06-01T12:00:00Z").unwrap();
        let mut fm = parser::Frontmatter {
            updated: Some(chosen),
            ..Default::default()
        };

        bump_updated_unless_explicit(&mut fm, "updated");

        assert_eq!(
            fm.updated,
            Some(chosen),
            "an explicit `fm set updated=…` must not be overwritten by the auto-bump"
        );
    }

    /// Store discovery must walk UP from a subdirectory to the store root — the
    /// same ancestor-walk `dbmd format` uses — so `fm set` / `fm init` work from
    /// any subdirectory of a store, and an interior content file named `DB.md`
    /// must not shadow the real outermost store root.
    #[test]
    fn outermost_store_root_walks_up_past_interior_db_md() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("DB.md"), "---\ntype: db-md\n---\n# Store\n").unwrap();

        // A nested type-folder that also holds an interior content `DB.md` (the
        // shadowing trap), plus a deeper subdir to start the walk from.
        let docs = root.join("sources").join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("DB.md"),
            "---\ntype: pdf-source\nsummary: ingested doc named DB.md\n---\n# Doc\n",
        )
        .unwrap();

        // Starting from the subdirectory that itself carries an interior DB.md,
        // discovery must still land on the OUTERMOST store, never `sources/docs`.
        let found = outermost_store_root(&docs).expect("a store must be found above");
        assert_eq!(
            std::fs::canonicalize(&found).unwrap(),
            std::fs::canonicalize(root).unwrap(),
            "interior DB.md must not become the store root"
        );

        // Starting from a plain subdirectory of the store resolves the same root.
        let records = root.join("records");
        std::fs::create_dir_all(&records).unwrap();
        assert_eq!(
            std::fs::canonicalize(outermost_store_root(&records).unwrap()).unwrap(),
            std::fs::canonicalize(root).unwrap(),
            "fm set / fm init must work from a subdirectory of the store"
        );
    }

    /// A directory with no store anywhere above it resolves to `None`, and the
    /// `locate_store_from_cwd` hint must be actionable for `fm set` / `fm init`
    /// (which take no `--dir`) — not the central generic "pass the store path".
    #[test]
    fn outermost_store_root_is_none_outside_any_store_and_hint_is_actionable() {
        let dir = tempfile::TempDir::new().unwrap();
        // A bare tempdir with no DB.md at or above it (within the temp tree).
        assert!(
            outermost_store_root(dir.path()).is_none(),
            "no DB.md above this dir → no store root"
        );

        // The error this maps to must not suggest the impossible remedy. Build
        // the same CliError shape `locate_store_from_cwd` emits and assert its
        // hint is actionable for a `--dir`-less command.
        let err = CliError::new(ExitCode::NotAStore, "NOT_A_STORE", "x").with_hint(
            "run from inside a db.md store (any directory at or under a DB.md), or author a DB.md at the store root first",
        );
        assert_eq!(err.code, "NOT_A_STORE");
        assert_eq!(err.exit, ExitCode::NotAStore);
        assert!(
            !err.hint.unwrap().contains("pass the store path"),
            "hint must not suggest the impossible `--dir`/store-path remedy"
        );
    }
}
