//! `dbmd validate` — working-set by default, full SWEEP under `--all`.
//!
//! Thin wrapper: resolve the store, call
//! `dbmd_core::validate::validate_working_set` (default — O(changed)) or
//! `dbmd_core::validate::validate_all` (`--all` — full sweep), then render the
//! structured [`Issue`] list (text, or a machine-parseable envelope under
//! `--json`). Exit [`ExitCode::ValidationFailed`] (`6`) when any issue is an
//! error. All validation logic — schema rules, link integrity, index sync,
//! `log.md` well-formedness, entity-dedup — lives in `dbmd-core`.
//!
//! A directory with no `DB.md` is NOT a hard `open` failure here: validate is
//! the tool that *reports* `NOT_A_STORE` as an issue (and exits non-zero), so a
//! non-store path is run through the engine (which emits that single issue)
//! rather than rejected before the engine sees it.

use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};
use dbmd_core::validate::{validate_all, validate_working_set};
use dbmd_core::{Config, Issue, Severity, Store};

use crate::cli::ValidateArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

/// Run `dbmd validate`.
pub fn run(ctx: &Context, args: &ValidateArgs) -> CliResult {
    let root = Path::new(&args.dir);

    // Open the store if the marker is present; otherwise hand the engine a store
    // rooted at `root` with default config so it emits the `NOT_A_STORE` issue
    // (the validate contract reports it as an issue + non-zero exit, never a
    // bare open error). The engine's `store_marker_present` gate does the rest.
    let store = if Store::is_db_md_store(root) {
        Store::open(root).map_err(|e| CliError::from(dbmd_core::Error::from(e)))?
    } else {
        Store {
            root: root.to_path_buf(),
            config: Config::default(),
        }
    };

    let scope = if args.all { "all" } else { "working-set" };

    let issues = if args.all {
        validate_all(&store).map_err(CliError::from)?
    } else {
        let since = parse_since(args.since.as_deref())?;
        validate_working_set(&store, since).map_err(CliError::from)?
    };

    let counts = Counts::of(&issues);

    if ctx.json {
        print!("{}", json_report(scope, &args.dir, &counts, &issues));
    } else {
        print!("{}", text_report(&counts, &issues));
    }

    // Errors fail validation (exit 6); warnings/info do not change the exit.
    if counts.errors > 0 {
        return Err(CliError::new(
            ExitCode::ValidationFailed,
            "VALIDATION_FAILED",
            format!(
                "validation found {} error{}",
                counts.errors,
                if counts.errors == 1 { "" } else { "s" }
            ),
        ));
    }
    Ok(())
}

/// Parse the optional `--since` cutoff. Accepts a full RFC3339 timestamp or a
/// date-only `YYYY-MM-DD` (treated as `T00:00:00Z`, per the flag's contract).
/// `None` lets the engine fall back to the last `validate` log entry.
fn parse_since(raw: Option<&str>) -> Result<Option<DateTime<FixedOffset>>, CliError> {
    let Some(raw) = raw else { return Ok(None) };
    let raw = raw.trim();
    // Full RFC3339 first.
    if let Ok(ts) = DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(ts));
    }
    // Date-only → midnight UTC.
    if let Ok(ts) = DateTime::parse_from_rfc3339(&format!("{raw}T00:00:00Z")) {
        return Ok(Some(ts));
    }
    Err(CliError::new(
        ExitCode::Runtime,
        "BAD_TIMESTAMP",
        format!("`--since` value `{raw}` is not RFC3339 or a YYYY-MM-DD date"),
    )
    .with_hint("use e.g. 2026-05-27 or 2026-05-27T08:00:00-07:00"))
}

/// Error / warning / info tallies over an issue list.
struct Counts {
    errors: usize,
    warnings: usize,
    info: usize,
}

impl Counts {
    fn of(issues: &[Issue]) -> Self {
        let mut c = Counts {
            errors: 0,
            warnings: 0,
            info: 0,
        };
        for issue in issues {
            match issue.severity {
                Severity::Error => c.errors += 1,
                Severity::Warning => c.warnings += 1,
                Severity::Info => c.info += 1,
            }
        }
        c
    }

    fn total(&self) -> usize {
        self.errors + self.warnings + self.info
    }
}

/// Human form: one line per issue (`<severity> <code> <file>[:<line>][ <key>] —
/// <message>`), then a summary line. A clean store prints just the summary.
fn text_report(counts: &Counts, issues: &[Issue]) -> String {
    let mut out = String::new();
    for issue in issues {
        out.push_str(&format!(
            "{} {} {}",
            severity_word(issue.severity),
            issue.code,
            issue.file.display()
        ));
        if let Some(line) = issue.line {
            out.push_str(&format!(":{line}"));
        }
        if let Some(key) = &issue.key {
            out.push_str(&format!(" [{key}]"));
        }
        out.push_str(&format!(" — {}", issue.message));
        out.push('\n');
        if let Some(suggestion) = &issue.suggestion {
            out.push_str(&format!("    hint: {suggestion}\n"));
        }
    }
    out.push_str(&format!(
        "{} issue(s): {} error(s), {} warning(s), {} info\n",
        counts.total(),
        counts.errors,
        counts.warnings,
        counts.info
    ));
    out
}

/// Machine form: `{scope, store, summary:{errors,warnings,info,total}, issues:
/// [...]}` — the same envelope shape the corpora's `EXPECTED/validate.json`
/// uses, so a consumer can diff structurally. Issues are sorted by
/// `(file, line, code)` for stable output.
fn json_report(scope: &str, store: &str, counts: &Counts, issues: &[Issue]) -> String {
    let mut sorted: Vec<&Issue> = issues.iter().collect();
    sorted.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.code.cmp(b.code))
    });
    let issues_json: Vec<serde_json::Value> = sorted.iter().map(|i| issue_json(i)).collect();

    let obj = serde_json::json!({
        "scope": scope,
        "store": store,
        "summary": {
            "errors": counts.errors,
            "warnings": counts.warnings,
            "info": counts.info,
            "total": counts.total(),
        },
        "issues": issues_json,
    });
    let mut s = serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

/// One issue as a JSON object matching the corpora's issue shape (`severity`,
/// `code`, `file`, `line`, `key`, `message`, `suggestion`, `related`).
fn issue_json(issue: &Issue) -> serde_json::Value {
    let related: Vec<String> = issue
        .related
        .iter()
        .map(|p: &PathBuf| p.to_string_lossy().into_owned())
        .collect();
    serde_json::json!({
        "severity": severity_word(issue.severity),
        "code": issue.code,
        "file": issue.file.to_string_lossy(),
        "line": issue.line,
        "key": issue.key,
        "message": issue.message,
        "suggestion": issue.suggestion,
        "related": related,
    })
}

/// The lowercase severity word used in both text and JSON output.
fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}
