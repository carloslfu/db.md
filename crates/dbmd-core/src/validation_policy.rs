// SPDX-License-Identifier: Apache-2.0

//! Explicit store-owned validation policy. No generic error suppression:
//! only custom log vocabulary, intentional optional uniqueness, and exact-byte
//! acknowledgements of overlong summaries in preserved sources are supported.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::store::Store;
use crate::validate::{codes, Issue, Severity};

/// Parsed from the three named `DB.md ## Policies` subsections. Invalid
/// declarations remain diagnostics, never silently become effective policies.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidationPolicy {
    log_kinds: BTreeSet<String>,
    optional_unique: Vec<OptionalUnique>,
    preserved_summaries: Vec<PreservedSummary>,
    problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionalUnique {
    #[serde(rename = "type")]
    type_name: String,
    fields: Vec<String>,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreservedSummary {
    path: String,
    sha256: String,
    reason: String,
}

const MAX_ENTRIES: usize = 256;

fn token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn reason(s: &str) -> bool {
    !s.trim().is_empty() && s.len() <= 1000 && !s.chars().any(char::is_control)
}

fn source_path(s: &str) -> bool {
    s.starts_with("sources/")
        && s.ends_with(".md")
        && !s
            .chars()
            .any(|c| c.is_control() || "\\:*?[]{}|".contains(c))
        && s.split('/').all(|p| !p.is_empty() && !p.starts_with('.'))
        && !matches!(s.rsplit('/').next(), Some("index.md"))
}

impl ValidationPolicy {
    pub(crate) fn add_log_kind(&mut self, raw: &str) {
        if !token(raw) || self.log_kinds.len() >= MAX_ENTRIES {
            self.problems
                .push("Validation log kinds: invalid token or too many entries".into());
        } else if !self.log_kinds.insert(raw.to_string()) {
            self.problems
                .push(format!("Validation log kinds: duplicate `{raw}`"));
        }
    }

    pub(crate) fn add_optional_unique(&mut self, raw: &str) {
        let entry = serde_json::from_str::<OptionalUnique>(raw);
        match entry {
            Ok(e)
                if token(&e.type_name)
                    && !e.fields.is_empty()
                    && e.fields.iter().all(|s| token(s))
                    && e.fields.iter().collect::<BTreeSet<_>>().len() == e.fields.len()
                    && reason(&e.reason)
                    && self.optional_unique.len() < MAX_ENTRIES =>
            {
                if self
                    .optional_unique
                    .iter()
                    .any(|old| old.type_name == e.type_name && old.fields == e.fields)
                {
                    self.problems
                        .push("Optional unique keys: duplicate declaration".into());
                } else {
                    self.optional_unique.push(e);
                }
            }
            _ => self.problems.push(
                "Optional unique keys: expected JSON with type, distinct fields and a reason"
                    .into(),
            ),
        }
    }

    pub(crate) fn add_preserved_summary(&mut self, raw: &str) {
        let entry = serde_json::from_str::<PreservedSummary>(raw);
        match entry {
            Ok(e) if source_path(&e.path)
                && e.sha256.len() == 64
                && e.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && reason(&e.reason)
                && self.preserved_summaries.len() < MAX_ENTRIES => {
                    if self.preserved_summaries.iter().any(|old| old.path == e.path) {
                        self.problems.push("Preserved source summaries: duplicate path".into());
                    } else {
                        self.preserved_summaries.push(e);
                    }
                }
            _ => self.problems.push("Preserved source summaries: expected JSON with an exact sources/*.md path, lowercase SHA-256 and a reason".into()),
        }
    }

    /// Additional exact, case-sensitive log vocabulary; no timestamps or log
    /// ordering checks are affected.
    pub fn recognizes_log_kind(&self, kind: &str) -> bool {
        self.log_kinds.contains(kind)
    }

    /// Opts into the existing NULLS DISTINCT behavior for this exact declared
    /// unique key. It never removes the duplicate-value check.
    pub fn allows_optional_unique(&self, type_name: &str, fields: &[String]) -> bool {
        self.optional_unique
            .iter()
            .any(|e| e.type_name == type_name && e.fields == fields)
    }

    /// Validate every bounded declaration on both full and working-set checks.
    /// Only an existing SUMMARY_TOO_LONG *warning* can become informational;
    /// the original code and message remain visible. Errors are never changed.
    pub(crate) fn apply(&self, store: &Store, issues: &mut Vec<Issue>) {
        for problem in &self.problems {
            invalid(issues, problem.clone());
        }
        for entry in &self.optional_unique {
            let valid = store
                .config
                .schemas
                .get(&entry.type_name)
                .is_some_and(|schema| {
                    schema.unique_keys.contains(&entry.fields)
                        && entry
                            .fields
                            .iter()
                            .all(|name| schema.fields.iter().any(|f| f.name == *name))
                });
            if !valid {
                invalid(issues, format!("Optional unique keys: `{}` must name an existing unique key with declared fields", entry.type_name));
            }
        }
        for entry in &self.preserved_summaries {
            // Capability-relative reads reject symlinks, traversal and nested
            // stores. Never hash through an unchecked filesystem join.
            let text = match store
                .read_text_bounded(Path::new(&entry.path), crate::parser::MAX_DBMD_FILE_BYTES)
            {
                Ok(text) => text,
                Err(_) => {
                    invalid(
                        issues,
                        format!(
                            "Preserved source summary `{}` is missing or unreadable",
                            entry.path
                        ),
                    );
                    continue;
                }
            };
            if format!("{:x}", Sha256::digest(text.as_bytes())) != entry.sha256 {
                invalid(
                    issues,
                    format!(
                        "Preserved source summary `{}` changed bytes; review the exception",
                        entry.path
                    ),
                );
                continue;
            }
            let overlong = crate::parser::split_frontmatter(&text, Path::new(&entry.path))
                .ok()
                .and_then(|p| {
                    serde_norway::from_str::<serde_norway::Value>(&p.frontmatter_yaml).ok()
                })
                .and_then(|v| {
                    v.get("summary")
                        .and_then(|s| s.as_str())
                        .map(|s| s.chars().count() > 200)
                })
                .unwrap_or(false);
            if !overlong {
                invalid(issues, format!("Preserved source summary `{}` no longer has an overlong summary; remove the stale exception", entry.path));
                continue;
            }
            for issue in issues.iter_mut() {
                if issue.code == codes::SUMMARY_TOO_LONG
                    && issue.severity == Severity::Warning
                    && issue.file == Path::new(&entry.path)
                    && issue.key.as_deref() == Some("summary")
                {
                    issue.severity = Severity::Info;
                    issue.message.push_str(&format!(
                        "; exact preserved source acknowledged: {}",
                        entry.reason
                    ));
                    issue.suggestion = Some("Keep preserved source bytes unchanged; new summaries must be at most 200 characters".into());
                }
            }
        }
    }
}

fn invalid(issues: &mut Vec<Issue>, message: String) {
    issues.push(Issue {
        severity: Severity::Error,
        code: codes::VALIDATION_POLICY_INVALID,
        file: "DB.md".into(),
        line: None,
        key: None,
        message,
        suggestion: Some(
            "Correct the explicit validation policy; never broaden it to hide an integrity failure"
                .into(),
        ),
        related: vec![],
    });
}
