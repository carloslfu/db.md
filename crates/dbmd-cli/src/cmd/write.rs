//! `dbmd write <path> --type <t>` — create a new file with frontmatter.
//!
//! Thin wrapper target: parse [`WriteArgs`], compose the frontmatter (default
//! `summary` via `dbmd_core::summary::compose_default` when `--summary` is
//! absent — a content file with no usable summary is refused), auto-shard
//! source-layer paths via `Store::shard_path_for`, refuse on path collision
//! (structured error with the existing file's summary + type), enforce the
//! `DB.md` frozen-page policy, write via the parser write path, then update
//! both indexes write-through (`dbmd_core::index::on_write`). Print the
//! resolved store-relative path (text + `--json`).
//!
//! This module also hosts the **cross-cutting write-surface helpers** every
//! writer in this group shares (`open_store`, `enforce_frozen`,
//! `to_store_relative`, `index_on_write` / `index_on_rename`,
//! `policy_frozen_error`). `link` and `rename` call them via
//! `crate::cmd::write::…` so the policy + write-through behavior is identical on
//! every surface and lives in exactly one place. Keeping them here (rather than
//! a new module) respects the wired module tree — `write`/`link`/`rename` are
//! already declared in `cmd/mod.rs`.

use std::path::{Path, PathBuf};

use dbmd_core::{summary, Frontmatter, Store};

use crate::cli::WriteArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd write`.
///
/// Order of operations (the contract the tests pin):
/// 1. Open the store (`NOT_A_STORE` if no `DB.md`).
/// 2. Seed `created`/`updated`, apply `--fm k=v`, set `type`.
/// 3. Compose `summary` (`--summary` wins; else the deterministic default) —
///    refuse a content file with no usable summary.
/// 4. Resolve the on-disk path (`Store::shard_path_for` auto-shards source +
///    event types by date; flat types pass through).
/// 5. Refuse on collision with a structured error carrying the existing file's
///    `summary` + `type`.
/// 6. Refuse a write to a `### Frozen pages` path (`POLICY_FROZEN_PAGE`).
/// 7. Write the file, then update both indexes write-through.
/// 8. Print the resolved store-relative path (and a richer object under
///    `--json`), plus the ignored-type-derivation warning when it applies.
pub fn run(ctx: &Context, args: &WriteArgs) -> CliResult {
    let store = open_store(&args.dir)?;

    // ── compose frontmatter ──────────────────────────────────────────────────
    let mut fm = Frontmatter::default();
    // Seed the universal timestamps first so an explicit `--fm created=…` /
    // `--fm updated=…` can override them (via `apply_fm_assignments` below), and
    // so a sharding type with no primary date field still has `created` for the
    // shard fallback. The seed is `dbmd_core::now()` — the one canonical
    // wall-clock every write surface (write, fm init, fm set, log append)
    // shares — assigned straight to the typed fields, no RFC3339 round-trip.
    let now = dbmd_core::now();
    fm.created = Some(now);
    fm.updated = Some(now);
    apply_fm_assignments(&mut fm, &args.fm)?;
    // `--type` is authoritative for the type (it is the required flag); set it
    // after `--fm` so a stray `--fm type=…` can never disagree with it.
    set_fm(&mut fm, "type", &args.r#type)?;

    // ── body (optional) ──────────────────────────────────────────────────────
    let body = match &args.body_file {
        Some(p) => read_body_file(p)?,
        None => String::new(),
    };

    // ── summary: explicit wins; else compose a deterministic default ─────────
    let summary_text = match &args.summary {
        Some(s) => summary::normalize(s),
        None => summary::compose_default(&store, &args.r#type, &fm, &body)?,
    };
    if is_content_type(&args.r#type) && summary_text.trim().is_empty() {
        return Err(no_summary_error(&args.r#type));
    }
    fm.summary = Some(summary_text);

    // ── policy: refuse a frozen-page write on the CALLER'S path first ─────────
    // A frozen page is refused regardless of whether it exists on disk and
    // regardless of how sharding would relocate the name — refusal is keyed on
    // the policy path. Enforcing on the caller-supplied (normalized, unsharded)
    // path here catches an explicit frozen target like
    // `wiki/synthesis/2026-annual-plan` even though `wiki-page` sharding would
    // otherwise rewrite it to `wiki/topics/…` and slip past the policy. This
    // also runs BEFORE the collision check so an *existing* frozen page reports
    // `POLICY_FROZEN_PAGE` (a policy refusal), not `PATH_COLLISION`.
    let requested_rel = to_store_relative(&store, &args.path);
    enforce_frozen(&store, &requested_rel)?;

    // ── resolve the on-disk path (auto-shard) ────────────────────────────────
    let resolved = resolve_write_path(&store, &args.r#type, &fm, &args.path)?;
    let resolved_disp = path_to_unix(&resolved);
    let abs = store.abs_path(&resolved);

    // ── policy: also refuse on the resolved path (sharded destination) ───────
    enforce_frozen(&store, &resolved)?;

    // ── collision refusal (after the frozen-page gate) ───────────────────────
    if abs.exists() {
        return Err(collision_error(&store, &resolved));
    }

    // ── write, then maintain the catalog write-through ───────────────────────
    dbmd_core::parser::write_file(&abs, &fm, &body).map_err(core_err)?;
    let index_warning = index_on_write(&store, &resolved);
    let policy_warning = ignored_type_derivation_warning(&store, &args.r#type, &fm);

    emit_result(
        ctx,
        &resolved_disp,
        &args.r#type,
        &index_warning,
        &policy_warning,
    );
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Cross-cutting write-surface helpers (shared by write / link / rename).
// ─────────────────────────────────────────────────────────────────────────────

/// Open the store rooted at `dir`, mapping a missing `DB.md` to the structured
/// `NOT_A_STORE` exit. The single store-open gate every writer in this group
/// goes through.
pub(crate) fn open_store(dir: &str) -> Result<Store, CliError> {
    // `Store::open` yields `NotAStore`; route it through `dbmd_core::Error`
    // (which has the `#[from] NotAStore` arm) so the CLI's single
    // `From<dbmd_core::Error>` conversion maps it to the `NOT_A_STORE` exit.
    Store::open(Path::new(dir)).map_err(|e| CliError::from(dbmd_core::Error::from(e)))
}

/// Normalize a caller-supplied path argument to a clean store-relative path.
///
/// Accepts a store-relative path (the common case), an absolute path under the
/// store root (rewritten to relative), or a `./`-prefixed path. The result uses
/// `/` separators and is what the index + policy layers key on.
///
/// **Canonicalizes both the target and the store root first** (via
/// [`canonical_store_relative`]) so an absolute target resolves to the *same*
/// store-relative key as the equivalent relative one. This is what makes the
/// frozen-page gate match when the store is opened from CWD (`store.root` is the
/// literal `.`) and the caller passes an absolute path: a bare
/// `strip_prefix(".")` / `rel_path` against a `.` root fails on an absolute
/// target, leaving the raw absolute path that no relative frozen entry can
/// equal, and the gate is silently skipped. A target that does not yet exist (a
/// fresh `write` / `rename` destination) cannot be canonicalized; it falls
/// through to the literal normalization below — correct, because such a path is
/// not on disk to be frozen.
pub(crate) fn to_store_relative(store: &Store, raw: &str) -> PathBuf {
    if let Some(rel) = canonical_store_relative(store, Path::new(raw)) {
        return rel;
    }
    let p = Path::new(raw);
    let rel = if p.is_absolute() {
        store.rel_path(p).unwrap_or_else(|| p.to_path_buf())
    } else {
        // Drop a single leading `./`.
        p.strip_prefix("./").unwrap_or(p).to_path_buf()
    };
    PathBuf::from(path_to_unix(&rel))
}

/// Resolve `target` to a store-relative path by canonicalizing **both** it and
/// the store root, then stripping the root prefix. The single canonicalizing
/// path resolver every write surface funnels through, so an absolute and a
/// relative spelling of the same file collapse to one key before the
/// frozen-page matcher (`Config::frozen_match`) and the index write-through see
/// it — the same property `format` already had via its own canonicalizing
/// `store_relative`.
///
/// Returns `Some(rel)` only when `target` exists on disk **and** lives under the
/// canonicalized store root; otherwise `None` so the caller falls back to
/// literal normalization. The result uses `/` separators on every OS.
pub(crate) fn canonical_store_relative(store: &Store, target: &Path) -> Option<PathBuf> {
    let canonical_target = std::fs::canonicalize(target).ok()?;
    let canonical_root = std::fs::canonicalize(&store.root).unwrap_or_else(|_| store.root.clone());
    let rel = canonical_target.strip_prefix(&canonical_root).ok()?;
    Some(PathBuf::from(path_to_unix(rel)))
}

/// Enforce the `DB.md` `### Frozen pages` policy: refuse a write to a frozen
/// path with the structured `POLICY_FROZEN_PAGE` error. `target` is a
/// store-relative path. The single funnel every write surface calls before it
/// touches disk.
pub(crate) fn enforce_frozen(store: &Store, target: &Path) -> Result<(), CliError> {
    if let Some(frozen) = store.config.frozen_match(target) {
        return Err(policy_frozen_error(&frozen));
    }
    Ok(())
}

/// Update both indexes write-through after a successful create/update. A failed
/// index update is **non-fatal** (the file is the source of truth) and is
/// returned as a human warning string the caller surfaces; the agent clears it
/// with `dbmd index rebuild --folder <p>`.
pub(crate) fn index_on_write(store: &Store, file: &Path) -> Option<String> {
    match dbmd_core::index::Index::on_write(store, file) {
        Ok(()) => None,
        Err(e) => Some(index_warning_text(&e)),
    }
}

/// Move a file's entry between type-folder indexes write-through. Non-fatal on
/// failure, same as [`index_on_write`].
pub(crate) fn index_on_rename(store: &Store, old: &Path, new: &Path) -> Option<String> {
    match dbmd_core::index::Index::on_rename(store, old, new) {
        Ok(()) => None,
        Err(e) => Some(index_warning_text(&e)),
    }
}

/// The structured `POLICY_FROZEN_PAGE` refusal (exit `4`). Names the policy
/// source (`DB.md ## Policies → ### Frozen pages`) and the frozen path so the
/// agent can branch on `code` without scraping prose.
pub(crate) fn policy_frozen_error(frozen: &Path) -> CliError {
    let path = path_to_unix(frozen);
    CliError::new(
        ExitCode::Policy,
        dbmd_core::validate::codes::POLICY_FROZEN_PAGE,
        format!("write refused: `{path}` is a frozen page (DB.md ## Policies → ### Frozen pages)"),
    )
    .with_hint(
        "write to a different path, or ask the operator to remove it from DB.md ### Frozen pages",
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// `write`-local helpers.
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve the final on-disk path for a `write`: auto-shard source/event types
/// by date via `Store::shard_path_for`; flat types pass through. The caller's
/// `<path>` names the type-folder + filename — `shard_path_for` inserts the
/// canonical type-folder and (for sharding types) the `<YYYY>/<MM>` segment from
/// the primary date field (or `created` fallback).
fn resolve_write_path(
    store: &Store,
    type_: &str,
    fm: &Frontmatter,
    raw_path: &str,
) -> Result<PathBuf, CliError> {
    let rel = to_store_relative(store, raw_path);
    // The filename is the last component; the folder is rebuilt below, so we hand
    // the sharder just the name. This is what lets the agent write
    // `dbmd write emails/e1.md --type email` (or even `e1.md`) and get the
    // canonical sharded location back.
    let name = rel.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        CliError::runtime(format!("write path `{raw_path}` has no filename component"))
    })?;

    // Honour an agent-supplied **conforming** type-folder. When the path names a
    // `<layer>/<sub-folder>/…/<file>` whose layer matches the type's canonical
    // layer, the agent's `<layer>/<sub-folder>` is the type-folder — this is the
    // only way to reach the SPEC's `wiki/people/`, `wiki/projects/`,
    // `wiki/synthesis/` (a `wiki-page` is filed under `wiki/<topic>/`, any topic),
    // and likewise an alternate records sub-folder. Sharding still applies under
    // the chosen folder (we pass the 2-component type-folder and let the sharder
    // re-derive `<YYYY>/<MM>`, so a re-supplied shard segment isn't doubled).
    // Anything under-specified (bare filename, or `<layer>/<file>`) or in the
    // wrong layer falls back to the canonical default folder.
    if let Some(folder) = explicit_type_folder(&rel, type_) {
        return store
            .shard_path_in(&folder, type_, fm, name)
            .map_err(core_err);
    }
    store.shard_path_for(type_, fm, name).map_err(core_err)
}

/// If `rel` names a conforming type-folder for `type_` — at least
/// `<layer>/<sub-folder>/<file>` (3 components), first component a recognized
/// layer, and that layer equal to the type's canonical layer — return the
/// 2-component `<layer>/<sub-folder>` to use as the explicit type-folder.
/// Returns `None` for an under-specified or wrong-layer path, so the caller uses
/// the canonical default instead.
fn explicit_type_folder(rel: &Path, type_: &str) -> Option<PathBuf> {
    let comps: Vec<&str> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    // Need at least layer + sub-folder + filename.
    if comps.len() < 3 {
        return None;
    }
    let layer = dbmd_core::Layer::from_dir_name(comps[0])?;
    if layer != dbmd_core::layer_for_type(type_) {
        return None;
    }
    Some(PathBuf::from(comps[0]).join(comps[1]))
}

/// Build the structured collision error (exit `5`) carrying the existing file's
/// `summary` and `type` so the agent can decide: update the existing file, or
/// write to a disambiguated path.
fn collision_error(store: &Store, resolved: &Path) -> CliError {
    let path = path_to_unix(resolved);
    let (existing_type, existing_summary) = read_type_and_summary(&store.abs_path(resolved));

    let mut message = format!("`{path}` already exists");
    match (&existing_type, &existing_summary) {
        (Some(t), Some(s)) => message.push_str(&format!(" — existing type: {t}, summary: {s}")),
        (Some(t), None) => message.push_str(&format!(" — existing type: {t}")),
        (None, Some(s)) => message.push_str(&format!(" — existing summary: {s}")),
        (None, None) => {}
    }

    // The structured error keeps `code = PATH_COLLISION` (exit 5) and carries
    // the existing `type` + `summary` in the message so a `--json` caller reads
    // them off the one structured channel `CliError` exposes. The agent's
    // decision (update vs. disambiguate) keys off the code; the metadata tells
    // it whether this is the same entity.
    CliError::new(ExitCode::Collision, "PATH_COLLISION", message)
        .with_hint("update the existing file (dbmd fm set), or write to a disambiguated path")
}

/// The refusal for a content file that has no usable summary (exit `1`). A
/// content file is invalid without `summary`; rather than write it, we refuse
/// and tell the agent to supply one.
fn no_summary_error(type_: &str) -> CliError {
    CliError::new(
        ExitCode::Runtime,
        "SUMMARY_REQUIRED",
        format!("a `{type_}` content file requires a summary, and none could be composed"),
    )
    .with_hint("pass --summary '<one line>' (the deterministic default was empty for this file)")
}

/// Emit the success result: the resolved store-relative path on its own line
/// (text), or a structured object under `--json`. Any non-fatal index /
/// ignored-type warnings go to stderr so stdout stays the clean path/JSON.
fn emit_result(
    ctx: &Context,
    resolved: &str,
    type_: &str,
    index_warning: &Option<String>,
    policy_warning: &Option<String>,
) {
    for w in [index_warning, policy_warning].into_iter().flatten() {
        eprintln!("dbmd: warning: {w}");
    }
    if ctx.json {
        let out = serde_json::json!({
            "written": resolved,
            "type": type_,
        });
        println!("{out}");
    } else {
        println!("{resolved}");
    }
}

/// Apply `--fm key=value` assignments to the frontmatter via the parser's
/// typed/extra routing. A token without `=` is a usage-class runtime error.
fn apply_fm_assignments(fm: &mut Frontmatter, assignments: &[String]) -> Result<(), CliError> {
    for raw in assignments {
        let (key, value) = raw.split_once('=').ok_or_else(|| {
            CliError::runtime(format!("--fm expects key=value, got `{raw}`"))
                .with_hint("use --fm date=2026-05-22 (repeat the flag for multiple fields)")
        })?;
        let key = key.trim();
        if key.is_empty() {
            return Err(CliError::runtime(format!(
                "--fm has an empty key in `{raw}`"
            )));
        }
        set_fm(fm, key, value)?;
    }
    Ok(())
}

/// Read the optional `--body-file` into a string (verbatim; the writer preserves
/// it byte-for-byte).
fn read_body_file(path: &str) -> Result<String, CliError> {
    std::fs::read_to_string(path)
        .map_err(|e| CliError::runtime(format!("cannot read --body-file {path}: {e}")))
}

/// Emit the `POLICY_IGNORED_TYPE_DERIVED` warning when a freshly-written
/// `wiki-page` declares a `derived_from:` wiki-link to a record whose type is in
/// `### Ignored types`. Read-only enforcement: writes don't block on it, they
/// warn (matches `dbmd validate`'s Warning-severity finding). Returns the human
/// warning text, or `None` when it doesn't apply.
///
/// The policy decision is **not** re-implemented here: it routes through the
/// single `dbmd_core::validate::derived_from_ignored_type` entry point that
/// `dbmd validate` also uses, so the two surfaces can't diverge. This handler
/// only supplies the `derived_from` targets from the composed frontmatter and
/// renders the write-time warning string.
fn ignored_type_derivation_warning(store: &Store, type_: &str, fm: &Frontmatter) -> Option<String> {
    let targets = fm
        .link_fields()
        .into_iter()
        .filter(|(key, _)| key == "derived_from")
        .map(|(_, link)| link.target);
    let hit = dbmd_core::validate::derived_from_ignored_type(store, type_, targets)?;
    Some(format!(
        "wiki-page derives from ignored-type record `{}` (type `{}`, per DB.md ## Policies → ### Ignored types)",
        hit.target, hit.target_type
    ))
}

/// True for a content type — everything that requires `summary`. Meta types
/// (`db-md`, `index`, `log`) are the only exceptions; `dbmd write` is for
/// content, but we keep the guard precise so a custom type still requires a
/// summary (per SPEC: custom types also require `summary`).
fn is_content_type(type_: &str) -> bool {
    !matches!(type_, "db-md" | "index" | "log")
}

/// Read a file's `type` + `summary` frontmatter for collision / derivation
/// reporting. Resilient: any read/parse failure yields `(None, None)` rather
/// than erroring — the collision itself is the message, not the parse.
fn read_type_and_summary(abs: &Path) -> (Option<String>, Option<String>) {
    match dbmd_core::parser::read_file(abs) {
        Ok((fm, _body)) => (fm.type_, fm.summary),
        Err(_) => (None, None),
    }
}

/// Turn an index/store error into the non-fatal write-through warning text.
fn index_warning_text(e: &dbmd_core::Error) -> String {
    format!("index not updated ({e}); run `dbmd index rebuild` to resync")
}

/// Map any `dbmd-core` module error (`ParseError`, `StoreError`, `NotAStore`)
/// into a [`CliError`] via the crate-root `dbmd_core::Error` (which has the
/// `#[from]` arms) and the CLI's single `From<dbmd_core::Error>` conversion.
/// The one funnel every fallible-core call in this group routes through, so the
/// exit-code mapping stays in exactly one place (`error.rs`).
pub(crate) fn core_err<E: Into<dbmd_core::Error>>(e: E) -> CliError {
    CliError::from(e.into())
}

/// Set a single frontmatter key, mapping the parser's `ParseError` to a
/// [`CliError`]. Used wherever a writer seeds or overrides frontmatter.
fn set_fm(fm: &mut Frontmatter, key: &str, value: &str) -> Result<(), CliError> {
    fm.set(key, value).map_err(core_err)
}

/// Render a path with `/` separators on every OS so output + comparisons are
/// platform-stable.
fn path_to_unix(p: &Path) -> String {
    p.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_now_populates_typed_timestamps_and_round_trips_to_rfc3339() {
        // `write` seeds `created`/`updated` from the canonical `dbmd_core::now()`
        // straight into the typed fields (no string round-trip). Pin that: the
        // seeded value is present, the writer renders it as valid RFC3339, and
        // re-parsing it yields the same instant — the contract the on-disk file
        // and every downstream parser depend on.
        let now = dbmd_core::now();
        let fm = Frontmatter {
            created: Some(now),
            updated: Some(now),
            ..Default::default()
        };

        let yaml = fm.to_yaml();
        // The writer emits both keys with the canonical RFC3339 rendering.
        assert!(yaml.contains("created:"), "{yaml}");
        assert!(yaml.contains("updated:"), "{yaml}");

        // Re-parsing the rendered value reproduces the seeded instant exactly.
        let rendered = now.to_rfc3339();
        let reparsed = chrono::DateTime::parse_from_rfc3339(&rendered)
            .expect("the seeded timestamp must render as valid RFC3339");
        assert_eq!(
            reparsed, now,
            "seeded `now` must round-trip through RFC3339"
        );
    }

    #[test]
    fn enforce_frozen_refuses_extensionless_policy_entry_against_md_target() {
        // The shared funnel every write surface (write/link/rename, and
        // transitively fm) routes through. An extensionless `### Frozen pages`
        // entry must still refuse the `.md` write target — the cross-surface
        // regression: `format` honored it but the others silently allowed it.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("DB.md"), "---\ntype: db-md\n---\n# s\n").unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.config.frozen_pages = vec![PathBuf::from("records/decisions/q1")];

        // `.md` target vs. extensionless policy entry → refused (the bug case).
        let err = enforce_frozen(&store, Path::new("records/decisions/q1.md")).unwrap_err();
        assert_eq!(err.code, dbmd_core::validate::codes::POLICY_FROZEN_PAGE);
        assert_eq!(err.exit, ExitCode::Policy);

        // A `./`-prefixed `.md` target also refused.
        assert!(enforce_frozen(&store, Path::new("./records/decisions/q1.md")).is_err());

        // An unlisted path passes.
        enforce_frozen(&store, Path::new("records/decisions/q2.md"))
            .expect("a non-frozen path must not be refused");
    }

    #[test]
    fn is_content_type_excludes_meta() {
        assert!(is_content_type("contact"));
        assert!(is_content_type("proposal")); // custom type still needs a summary
        assert!(!is_content_type("index"));
        assert!(!is_content_type("db-md"));
        assert!(!is_content_type("log"));
    }

    #[test]
    fn canonical_store_relative_rebases_an_absolute_in_store_target() {
        // The fix for the absolute-path frozen-page bypass: an absolute path to
        // an existing in-store file must resolve to the same store-relative key
        // the relative spelling produces, so the frozen matcher + index keying
        // see one value regardless of how the caller spelled the path.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("DB.md"), "---\ntype: db-md\n---\n# s\n").unwrap();
        let store = Store::open(dir.path()).unwrap();
        let abs = store.root.join("records/decisions/q1.md");
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, "---\ntype: decision\nsummary: x\n---\n# Q1\n").unwrap();

        // Absolute path → the store-relative key, `/`-separated.
        assert_eq!(
            canonical_store_relative(&store, &abs),
            Some(PathBuf::from("records/decisions/q1.md"))
        );
        // A non-existent target can't be canonicalized → None (caller falls back
        // to literal normalization; such a path is not on disk to be frozen).
        assert_eq!(
            canonical_store_relative(&store, &store.root.join("records/decisions/ghost.md")),
            None
        );
        // A target outside the store → None (not under the canonical root).
        let outside = tempfile::TempDir::new().unwrap();
        let outside_file = outside.path().join("elsewhere.md");
        std::fs::write(&outside_file, "x").unwrap();
        assert_eq!(canonical_store_relative(&store, &outside_file), None);
    }

    #[test]
    fn to_store_relative_collapses_absolute_and_relative_spellings() {
        // End-to-end of the helper `link`/`rename`/`write` call: the absolute
        // and the bare relative spelling of the same in-store file collapse to
        // one store-relative key, which is what the frozen gate keys on.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("DB.md"), "---\ntype: db-md\n---\n# s\n").unwrap();
        let store = Store::open(dir.path()).unwrap();
        let abs = store.root.join("records/decisions/q1.md");
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, "---\ntype: decision\nsummary: x\n---\n# Q1\n").unwrap();

        let from_abs = to_store_relative(&store, abs.to_str().unwrap());
        assert_eq!(from_abs, PathBuf::from("records/decisions/q1.md"));
        // A still-nonexistent relative destination passes through verbatim
        // (drops a leading `./`), the path `rename`'s `<new>` relies on.
        assert_eq!(
            to_store_relative(&store, "./records/decisions/new.md"),
            PathBuf::from("records/decisions/new.md")
        );
    }
}
