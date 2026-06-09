//! `dbmd log <sub>` — the append-only store timeline.
//!
//! Dispatches the [`LogCommand`] to one of three leaf bodies:
//!   - `tail`   → read the last N entries (`dbmd_core::log::Log::tail`)
//!   - `since`  → read entries newer than a timestamp (`Log::since`)
//!   - append   → `dbmd log <kind> <object> [-m <note>]` (`Log::append`)
//!
//! The append form arrives as an `external_subcommand` `Vec<String>` (clap
//! routes any first token that is not `tail`/`since`/`help` here). The body
//! parses `<kind> <object> [-m|--message <note>]` out of those tokens — clap
//! does NOT parse flags inside an external subcommand, so `-m` is captured
//! verbatim in the vector. For the same reason the global `--json` / `--color`
//! flags are captured verbatim too when they trail the append form; the body
//! recognizes and strips them so `dbmd log <kind> <object> --json` behaves like
//! the flag-first `dbmd --json log <kind> <object>` clap parses elsewhere.
//!
//! Thin wrapper: parse args, build a `dbmd_core::log::LogEntry`, call
//! `Log::{append,tail,since}`, format output (text or `--json`). The append
//! timestamp is wall-clock now in UTC; reads render the entry's own timestamp.

use std::path::Path;

use chrono::{DateTime, FixedOffset, NaiveDate, TimeZone};

use crate::cli::{LogArgs, LogCommand, LogSinceArgs, LogTailArgs};
use crate::context::{ColorChoice, Context};
use crate::error::{CliError, CliResult, ExitCode};

use dbmd_core::{Log, LogEntry, LogKind, Store};

/// Dispatch `dbmd log <sub>` to the matching leaf body.
pub fn run(ctx: &Context, args: &LogArgs) -> CliResult {
    match &args.command {
        LogCommand::Tail(a) => run_tail(ctx, a),
        LogCommand::Since(a) => run_since(ctx, a),
        LogCommand::Append(tokens) => run_append(ctx, tokens),
    }
}

/// `dbmd log tail [N]` — the last `N` entries, oldest→newest (chronological),
/// via the core reverse-from-EOF reader.
pub fn run_tail(ctx: &Context, args: &LogTailArgs) -> CliResult {
    let store = open_store(&args.dir)?;
    let entries = Log::tail(&store, args.n)?;
    emit_entries(ctx, &entries);
    Ok(())
}

/// `dbmd log since <timestamp>` — entries strictly newer than `timestamp`.
/// Date-only (`2026-05-27`) is accepted and treated as `T00:00:00Z`.
pub fn run_since(ctx: &Context, args: &LogSinceArgs) -> CliResult {
    let store = open_store(&args.dir)?;
    let time = parse_flexible_timestamp(&args.timestamp)?;
    let entries = Log::since(&store, time)?;
    emit_entries(ctx, &entries);
    Ok(())
}

/// `dbmd log <kind> <object> [-m <note>]` (the append form). `tokens` is the
/// raw, clap-unparsed argument list captured by the external subcommand: the
/// body splits out the kind, object, and optional `-m`/`--message` note, builds
/// a [`LogEntry`] timestamped now (UTC), and appends it (auto-rotating older
/// months into `log/<YYYY-MM>.md`).
pub fn run_append(ctx: &Context, tokens: &[String]) -> CliResult {
    let parsed = ParsedAppend::from_tokens(tokens)?;

    // The append form is a clap `external_subcommand`, so the global `--json` /
    // `--color` flags are NOT parsed by clap when they trail the form (the
    // natural, every-other-subcommand-accepts-it habit). `from_tokens` strips
    // them out of the token stream and reports them here; we fold them onto the
    // inherited `ctx` so `dbmd log <kind> <object> -m … --json` behaves the same
    // as the flag-first `dbmd --json log <kind> <object> -m …`.
    let ctx = parsed.effective_context(ctx);
    let ctx = &ctx;

    // The store root is not a flag on the append form (clap can't parse flags
    // inside an external subcommand), so the append form always operates on the
    // current directory — the documented convention for the loop-side `log`.
    let store = open_store(".")?;

    // `-` is the store-wide sentinel: no object slot in the header.
    let object = if parsed.object == "-" {
        None
    } else {
        Some(parsed.object.clone())
    };

    let entry = LogEntry {
        timestamp: now_fixed(),
        kind: LogKind::parse(&parsed.kind),
        object,
        note: parsed.note.unwrap_or_default(),
    };

    Log::append(&store, &entry)?;

    if ctx.json {
        let obj = serde_json::json!({
            "appended": true,
            "kind": entry.kind.as_str(),
            "object": entry.object,
            "timestamp": fmt_ts(&entry.timestamp),
        });
        println!("{obj}");
    } else {
        // Echo the canonical header line so the agent sees exactly what landed.
        match &entry.object {
            Some(o) => {
                println!(
                    "[{}] {} | {}",
                    fmt_ts(&entry.timestamp),
                    entry.kind.as_str(),
                    o
                )
            }
            None => println!("[{}] {}", fmt_ts(&entry.timestamp), entry.kind.as_str()),
        }
    }
    Ok(())
}

/// The parsed pieces of a `log <kind> <object> [-m <note>]` append invocation.
struct ParsedAppend {
    kind: String,
    object: String,
    note: Option<String>,
    /// `--json` seen trailing/embedded in the append form (clap can't parse it
    /// there). `None` ⇒ inherit `ctx.json`; `Some(true)` ⇒ force JSON.
    json: Option<bool>,
    /// `--color <when>` / `--color=<when>` seen trailing/embedded in the append
    /// form. `None` ⇒ inherit `ctx.color`.
    color: Option<ColorChoice>,
}

impl ParsedAppend {
    /// Fold the global flags captured off the append token stream onto the
    /// inherited `ctx`, yielding the [`Context`] the body should actually emit
    /// with. Lets a trailing `--json` / `--color` on the append form behave the
    /// same as the flag-first placement clap parses for every other subcommand.
    fn effective_context(&self, ctx: &Context) -> Context {
        Context {
            json: self.json.unwrap_or(ctx.json),
            color: self.color.unwrap_or(ctx.color),
        }
    }

    /// Split the raw external-subcommand tokens into `<kind> <object>` plus an
    /// optional `-m`/`--message` note. The two leading positionals are required;
    /// the note flag may appear before or after them. A `--message=<note>` /
    /// `-m<note>` joined form is also accepted.
    ///
    /// The global `--json` and `--color <when>` flags are recognized and stripped
    /// here (not counted as positionals): clap routes the append form through an
    /// `external_subcommand`, so these globals are captured verbatim when they
    /// trail the form — the natural placement every other subcommand accepts. We
    /// strip them, report them via `json`/`color`, and let the body fold them onto
    /// the inherited context (see [`Self::effective_context`]).
    fn from_tokens(tokens: &[String]) -> Result<ParsedAppend, CliError> {
        let mut positionals: Vec<String> = Vec::new();
        let mut note: Option<String> = None;
        let mut json: Option<bool> = None;
        let mut color: Option<ColorChoice> = None;

        let mut i = 0;
        while i < tokens.len() {
            let tok = tokens[i].as_str();
            if tok == "-m" || tok == "--message" {
                // The next token is the note value (verbatim, one argument).
                let val = tokens.get(i + 1).ok_or_else(|| {
                    usage_error("`-m` requires a note argument: dbmd log <kind> <object> -m <note>")
                })?;
                note = Some(val.clone());
                i += 2;
                continue;
            }
            if let Some(rest) = tok.strip_prefix("--message=") {
                note = Some(rest.to_string());
                i += 1;
                continue;
            }
            if let Some(rest) = tok.strip_prefix("-m") {
                if !rest.is_empty() {
                    note = Some(rest.to_string());
                    i += 1;
                    continue;
                }
            }
            // Strip the global `--json` flag wherever it lands on the append form.
            if tok == "--json" {
                json = Some(true);
                i += 1;
                continue;
            }
            // Strip the global `--color <when>` / `--color=<when>` flag.
            if tok == "--color" {
                let val = tokens.get(i + 1).ok_or_else(|| {
                    usage_error("`--color` requires a value: auto, always, or never")
                })?;
                color = Some(parse_color(val)?);
                i += 2;
                continue;
            }
            if let Some(rest) = tok.strip_prefix("--color=") {
                color = Some(parse_color(rest)?);
                i += 1;
                continue;
            }
            positionals.push(tok.to_string());
            i += 1;
        }

        if positionals.len() < 2 {
            return Err(usage_error(
                "usage: dbmd log <kind> <object> [-m <note>]  (<object> is a store-relative path, or `-` for store-wide)",
            ));
        }
        if positionals.len() > 2 {
            return Err(usage_error(
                "too many arguments: dbmd log <kind> <object> [-m <note>] — quote a multi-word note after -m",
            ));
        }

        Ok(ParsedAppend {
            kind: positionals[0].clone(),
            object: positionals[1].clone(),
            note,
            json,
            color,
        })
    }
}

/// Parse a `--color` value (`auto` | `always` | `never`) captured off the append
/// token stream. Mirrors clap's `value_enum` for [`ColorChoice`] so the trailing
/// form matches the flag-first form exactly; an unknown value is a usage error.
fn parse_color(value: &str) -> Result<ColorChoice, CliError> {
    match value {
        "auto" => Ok(ColorChoice::Auto),
        "always" => Ok(ColorChoice::Always),
        "never" => Ok(ColorChoice::Never),
        other => Err(usage_error(&format!(
            "invalid --color value {other:?}: expected auto, always, or never"
        ))),
    }
}

// ── Output helpers ───────────────────────────────────────────────────────────

/// Render a slice of log entries: a JSON array under `--json`, else one human
/// block per entry (the canonical header line, then any note body), blank-line
/// separated.
fn emit_entries(ctx: &Context, entries: &[LogEntry]) {
    if ctx.json {
        let arr: Vec<serde_json::Value> = entries.iter().map(entry_to_json).collect();
        println!("{}", serde_json::Value::Array(arr));
        return;
    }
    for (idx, e) in entries.iter().enumerate() {
        if idx > 0 {
            println!();
        }
        match &e.object {
            Some(o) => println!("[{}] {} | {}", fmt_ts(&e.timestamp), e.kind.as_str(), o),
            None => println!("[{}] {}", fmt_ts(&e.timestamp), e.kind.as_str()),
        }
        if !e.note.is_empty() {
            println!("{}", e.note);
        }
    }
}

/// One log entry as a JSON object.
fn entry_to_json(e: &LogEntry) -> serde_json::Value {
    serde_json::json!({
        "timestamp": fmt_ts(&e.timestamp),
        "kind": e.kind.as_str(),
        "object": e.object,
        "note": e.note,
    })
}

/// Render a timestamp in the on-disk header style (`YYYY-MM-DD HH:MM`, minute
/// precision, no timezone) so the text output matches the `log.md` headers.
fn fmt_ts(ts: &DateTime<FixedOffset>) -> String {
    ts.format("%Y-%m-%d %H:%M").to_string()
}

/// Wall-clock now as a fixed-offset (UTC) timestamp for a fresh log entry.
/// Delegates to `dbmd_core::now()` — the one canonical wall-clock every write
/// surface (write, fm init, fm set, log append) seeds timestamps from.
fn now_fixed() -> DateTime<FixedOffset> {
    dbmd_core::now()
}

// ── Shared glue ──────────────────────────────────────────────────────────────

/// Open the store at `dir`, mapping a missing `DB.md` to the standard
/// `NOT_A_STORE` CLI error.
pub(crate) fn open_store(dir: &str) -> Result<Store, CliError> {
    Store::open_strict(Path::new(dir)).map_err(CliError::from)
}

/// Lift any `dbmd-core` sub-error (`ParseError` / `StoreError` / `NotAStore`)
/// into a [`CliError`] via the crate-root [`dbmd_core::Error`] hop. The CLI's
/// `From` impls only cover the unified `dbmd_core::Error`, so the module-specific
/// errors several core functions return (e.g. `parser::read_file` → `ParseError`,
/// `Query::execute` → `StoreError`) need this one conversion to flow through `?`.
/// Shared by `fm`, `index`, and `log`.
pub(crate) fn into_cli<T, E: Into<dbmd_core::Error>>(r: Result<T, E>) -> Result<T, CliError> {
    r.map_err(|e| e.into().into())
}

/// Parse a user-supplied timestamp into a fixed-offset instant, accepting both
/// a full RFC3339 string (`2026-05-27T10:00:00Z`, `…-07:00`) and a bare
/// date (`2026-05-27`, treated as `T00:00:00Z`). Shared by `log since` and
/// `index query`'s `--*-after/-before` windows so both honor the same contract.
pub(crate) fn parse_flexible_timestamp(raw: &str) -> Result<DateTime<FixedOffset>, CliError> {
    let s = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt);
    }
    // Date-only fallback: midnight UTC on that calendar day.
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(naive) = date.and_hms_opt(0, 0, 0) {
            if let Some(dt) =
                FixedOffset::east_opt(0).and_then(|tz| tz.from_local_datetime(&naive).single())
            {
                return Ok(dt);
            }
        }
    }
    Err(CliError::new(
        ExitCode::Runtime,
        "BAD_TIMESTAMP",
        format!("not a valid RFC3339 timestamp or YYYY-MM-DD date: {raw:?}"),
    )
    .with_hint("use `2026-05-27T10:00:00Z`, `2026-05-27T10:00:00-07:00`, or `2026-05-27`"))
}

/// A usage error (exit code `1`, runtime class) for a malformed append form.
/// clap owns exit code `2` for the flags it parses; the append form is an
/// external subcommand clap does not introspect, so its arg errors surface here.
fn usage_error(message: &str) -> CliError {
    CliError::new(ExitCode::Runtime, "LOG_USAGE", message)
        .with_hint("dbmd log <kind> <object> [-m <note>]")
}
