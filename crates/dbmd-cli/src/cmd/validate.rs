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
use dbmd_core::validate::{apply_projection_policy, validate_all, validate_working_set};
use dbmd_core::{Config, Issue, Severity, Store};

use crate::cli::ValidateArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};
use crate::sanitize::sanitize_single_line;

use super::projection::{load as load_projection, load_manifest as load_projection_manifest};

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
        Store::from_root_and_config(root, Config::default()).map_err(CliError::from)?
    };

    let scoped_view = dbmd_core::linkmd::has_verified_local_scoped_view(&store);
    let projection = match (
        args.projection_excludes.as_deref(),
        args.projection_manifest.as_deref(),
    ) {
        (Some(path), None) => Some(load_projection(&store, path)?),
        (None, Some(path)) => Some(load_projection_manifest(&store, path)?),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("clap rejects conflicting projection inputs"),
    };
    let scope = match (args.all, scoped_view, projection.is_some()) {
        (true, true, true) => "scoped-projection-all",
        (false, true, true) => "scoped-projection-working-set",
        (true, true, false) => "scoped-all",
        (false, true, false) => "scoped-working-set",
        (true, false, true) => "projection-all",
        (false, false, true) => "projection-working-set",
        (true, false, false) => "all",
        (false, false, false) => "working-set",
    };

    let mut issues = if args.all {
        validate_all(&store).map_err(CliError::from)?
    } else {
        let since = parse_since(args.since.as_deref())?;
        validate_working_set(&store, since).map_err(CliError::from)?
    };
    if scoped_view {
        make_scoped_link_findings_non_disclosing(&mut issues);
    }
    if let Some(excludes) = projection.as_ref() {
        apply_projection_policy(&mut issues, excludes);
    }

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

fn make_scoped_link_findings_non_disclosing(issues: &mut [Issue]) {
    for issue in issues {
        if issue.code != dbmd_core::validate::codes::WIKI_LINK_BROKEN {
            continue;
        }
        issue.severity = Severity::Info;
        issue.code = "WIKI_LINK_SCOPED_UNRESOLVED";
        issue.message =
            "wiki-link leaves this materialized permission view; target existence is undisclosed"
                .to_string();
        issue.suggestion = Some(
            "request a wider grant or inspect the link from a full-authority checkout".to_string(),
        );
        issue.related.clear();
    }
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
            sanitize_single_line(issue.code),
            sanitize_single_line(&issue.file.to_string_lossy())
        ));
        if let Some(line) = issue.line {
            out.push_str(&format!(":{line}"));
        }
        if let Some(key) = &issue.key {
            out.push_str(&format!(" [{}]", sanitize_single_line(key)));
        }
        out.push_str(&format!(" — {}", sanitize_single_line(&issue.message)));
        out.push('\n');
        if let Some(suggestion) = &issue.suggestion {
            out.push_str(&format!("    hint: {}\n", sanitize_single_line(suggestion)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_report_neutralizes_malicious_issue_fields_but_json_is_exact() {
        let issue = Issue {
            severity: Severity::Error,
            code: "TEST",
            file: PathBuf::from("records/bad\nfake\u{1b}[31m.md"),
            line: Some(7),
            key: Some("summary\tspoof\u{202e}".to_string()),
            message: "first\nerror: forged\u{1b}]0;owned\u{7}".to_string(),
            suggestion: Some("fix\tit\nnow".to_string()),
            related: Vec::new(),
        };
        let counts = Counts::of(std::slice::from_ref(&issue));
        let human = text_report(&counts, std::slice::from_ref(&issue));
        assert!(!human.contains('\u{1b}'));
        assert!(!human.contains('\u{202e}'));
        assert!(human.contains(r"records/bad\nfake.md:7"));
        assert!(human.contains(r"[summary\tspoof]"));
        assert!(human.contains(r"first\nerror: forged"));
        assert!(human.contains(r"hint: fix\tit\nnow"));

        let json = issue_json(&issue);
        assert_eq!(json["file"], "records/bad\nfake\u{1b}[31m.md");
        assert_eq!(json["key"], "summary\tspoof\u{202e}");
        assert_eq!(json["message"], "first\nerror: forged\u{1b}]0;owned\u{7}");
    }

    #[test]
    fn scoped_view_never_claims_an_unmaterialized_link_is_broken() {
        let mut issues = vec![Issue {
            severity: Severity::Error,
            code: dbmd_core::validate::codes::WIKI_LINK_BROKEN,
            file: PathBuf::from("records/contacts/a.md"),
            line: Some(9),
            key: Some("company".to_string()),
            message: "wiki-link target `records/companies/secret` doesn't exist".to_string(),
            suggestion: Some("create the target".to_string()),
            related: vec![PathBuf::from("records/companies/secret.md")],
        }];
        make_scoped_link_findings_non_disclosing(&mut issues);
        assert_eq!(issues[0].severity, Severity::Info);
        assert_eq!(issues[0].code, "WIKI_LINK_SCOPED_UNRESOLVED");
        assert!(!issues[0].message.contains("secret"));
        assert!(issues[0].related.is_empty());
        assert_eq!(Counts::of(&issues).errors, 0);
    }
}
