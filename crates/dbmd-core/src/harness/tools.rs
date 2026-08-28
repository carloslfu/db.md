// SPDX-License-Identifier: Apache-2.0

//! The harness tool registry: which tools each [`Mask`](super::Mask) exposes,
//! their JSON-Schema parameter contracts, and the planner that turns one
//! model tool call into either `dbmd` verb invocations (executed by the
//! caller as `current_exe()` spawns — the `dbmd api` ONE SEMANTICS rule) or a
//! workspace file operation ([`super::files`]).
//!
//! Scope rules, per the doctrine:
//! - Cross-party verbs (sync, grants, proposals, keys, mirror) are NOT tools —
//!   the same exclusion list as `dbmd api`.
//! - Free-fire sweep ops are NOT tools: `validate --all`, `index rebuild`,
//!   and `stats` are O(store) and stay off the model's loop (AGENTS.md hard
//!   rule). The `query`/`search` tools carry `limit` parameters instead.
//! - `rm` never force-deletes: the link-aware refusal rides back to the model
//!   as an error result, exactly as it rides to an app as a 409.
//! - There is no shell tool at any mask.

use serde_json::{json, Value};

use super::files::FileOp;
use super::Mask;

/// One tool the model may call: name, description, and a JSON-Schema object
/// for its parameters (sent verbatim to OpenAI-compat `function.parameters`
/// and lifted into Anthropic `input_schema`).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// Tool name on the wire.
    pub name: &'static str,
    /// One-paragraph description the model reads.
    pub description: &'static str,
    /// JSON Schema for the arguments object.
    pub parameters: Value,
}

/// What one tool call maps to.
#[derive(Debug, Clone)]
pub enum ToolAction {
    /// One or more `dbmd` invocations, run in order, stopping on the first
    /// failure. Each argv EXCLUDES the binary name.
    Verbs(Vec<Vec<String>>),
    /// A workspace file operation (`build` mask only).
    File(FileOp),
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn string_prop(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn int_prop(description: &str) -> Value {
    json!({ "type": "integer", "description": description })
}

/// The tool registry for a mask. Order is stable (byte-stable tool arrays are
/// prompt-cache-friendly).
pub fn registry(mask: Mask) -> Vec<ToolSpec> {
    let mut tools = vec![
        ToolSpec {
            name: "query",
            description: "Query records by frontmatter. Returns the matching records as JSON \
                          (path, summary, type, timestamps, fields). Use `where` clauses like \
                          `status=active`. Always prefer a `limit` on broad queries.",
            parameters: schema(
                json!({
                    "type": string_prop("filter to this frontmatter `type`"),
                    "where": { "type": "array", "items": { "type": "string" },
                               "description": "frontmatter filters, each `key=value`" },
                    "limit": int_prop("cap the number of results"),
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "search",
            description: "Full-text regex search over record bodies (embedded ripgrep). The \
                          pattern is a regex; use alternation `(a|b)` to broaden and `(?i)` \
                          for case-insensitive. Returns matching files with line hits.",
            parameters: schema(
                json!({
                    "pattern": string_prop("the regex to search for"),
                    "type": string_prop("filter to this frontmatter `type`"),
                    "limit": int_prop("cap the number of matches"),
                }),
                &["pattern"],
            ),
        },
        ToolSpec {
            name: "show",
            description: "Read one file as its full structured record: frontmatter, derived \
                          fields, verbatim body, and wiki-link targets.",
            parameters: schema(
                json!({ "file": string_prop("store-relative path, e.g. records/todos/x.md") }),
                &["file"],
            ),
        },
        ToolSpec {
            name: "schema",
            description: "The store's declared type contracts (DB.md ## Schemas), parsed: each \
                          field with its modifiers (required, enum, defaults).",
            parameters: schema(
                json!({ "type": string_prop("narrow to one declared type") }),
                &[],
            ),
        },
        ToolSpec {
            name: "tree",
            description: "The store's layout: layers, type folders, and files.",
            parameters: schema(
                json!({
                    "layer": string_prop("restrict to `sources` or `records`"),
                    "type": string_prop("restrict to one frontmatter `type`"),
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "log_tail",
            description: "The last N entries of the store's chronological log (who did what, \
                          when), oldest to newest.",
            parameters: schema(
                json!({ "n": int_prop("how many entries (default 20)") }),
                &[],
            ),
        },
    ];

    if matches!(mask, Mask::Write | Mask::Build) {
        tools.extend([
            ToolSpec {
                name: "write",
                description: "Create a new record file with canonical frontmatter. Refuses on \
                              path collision. `fm` entries are `key=value`. The optional `body` \
                              becomes the markdown body below the frontmatter.",
                parameters: schema(
                    json!({
                        "path": string_prop("store-relative path to create, e.g. records/todos/buy-milk.md"),
                        "type": string_prop("the frontmatter `type`"),
                        "summary": string_prop("the canonical one-line summary"),
                        "fm": { "type": "array", "items": { "type": "string" },
                                "description": "additional frontmatter, each `key=value`" },
                        "body": string_prop("optional markdown body"),
                    }),
                    &["path", "type", "summary"],
                ),
            },
            ToolSpec {
                name: "fm_set",
                description: "Set one frontmatter value on an existing file (`key` + `value`). \
                              Atomic; keeps indexes write-through.",
                parameters: schema(
                    json!({
                        "file": string_prop("store-relative path"),
                        "key": string_prop("frontmatter key"),
                        "value": string_prop("new value"),
                    }),
                    &["file", "key", "value"],
                ),
            },
            ToolSpec {
                name: "body_set",
                description: "Replace a file's whole markdown body (below the frontmatter), \
                              stored verbatim. Re-stamps `updated`.",
                parameters: schema(
                    json!({
                        "file": string_prop("store-relative path"),
                        "content": string_prop("the new body markdown"),
                    }),
                    &["file", "content"],
                ),
            },
            ToolSpec {
                name: "rm",
                description: "Delete one record file, link-aware: refuses (as an error result) \
                              while other files still wiki-link to it. There is no force \
                              override through this tool.",
                parameters: schema(
                    json!({ "path": string_prop("store-relative path to delete") }),
                    &["path"],
                ),
            },
            ToolSpec {
                name: "log",
                description: "Append one entry to the store log — do this after finishing a \
                              set of changes, with a short note.",
                parameters: schema(
                    json!({
                        "kind": string_prop("create | update | delete | note"),
                        "object": string_prop("the store-relative path acted on, or `-`"),
                        "message": string_prop("short human note"),
                    }),
                    &["kind", "object", "message"],
                ),
            },
        ]);
    }

    if matches!(mask, Mask::Build) {
        tools.extend([
            ToolSpec {
                name: "list_files",
                description: "List files under the app workspace (the code around the store). \
                              Paths are workspace-relative. The store's own files are managed \
                              through the store tools, never through file tools.",
                parameters: schema(
                    json!({ "dir": string_prop("workspace-relative directory (default: the root)") }),
                    &[],
                ),
            },
            ToolSpec {
                name: "read_file",
                description: "Read one workspace file (source code, config, docs).",
                parameters: schema(
                    json!({ "path": string_prop("workspace-relative file path") }),
                    &["path"],
                ),
            },
            ToolSpec {
                name: "write_file",
                description: "Create or replace one workspace file with the given content.",
                parameters: schema(
                    json!({
                        "path": string_prop("workspace-relative file path"),
                        "content": string_prop("the full new file content"),
                    }),
                    &["path", "content"],
                ),
            },
            ToolSpec {
                name: "edit_file",
                description: "Edit one workspace file by exact-string replacement: `old_text` \
                              must occur exactly once and is replaced by `new_text`.",
                parameters: schema(
                    json!({
                        "path": string_prop("workspace-relative file path"),
                        "old_text": string_prop("the exact text to replace (must match once)"),
                        "new_text": string_prop("the replacement text"),
                    }),
                    &["path", "old_text", "new_text"],
                ),
            },
        ]);
    }

    tools
}

fn req_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing required string argument `{key}`"))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

fn opt_int(args: &Value, key: &str) -> Option<i64> {
    let value = args.get(key)?;
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// A path argument that will ride an argv: refuse flag-looking values (the
/// same guard `dbmd api` applies to its path params).
fn path_str(args: &Value, key: &str) -> Result<String, String> {
    let value = req_str(args, key)?;
    if value.starts_with('-') {
        return Err(format!("`{key}` must not start with '-'"));
    }
    Ok(value)
}

/// Plan one tool call. Unknown tools and bad arguments come back as `Err`
/// with a message the model can act on (the executor feeds it back as an
/// error tool result — never a crash).
pub fn plan(mask: Mask, name: &str, args: &Value) -> Result<ToolAction, String> {
    let known: Vec<&'static str> = registry(mask).iter().map(|t| t.name).collect();
    if !known.contains(&name) {
        return Err(format!(
            "unknown tool `{name}` — available tools: {}",
            known.join(", ")
        ));
    }
    let j = |s: &str| s.to_string();
    match name {
        "query" => {
            let mut argv = vec![j("--json"), j("query")];
            if let Some(t) = opt_str(args, "type") {
                argv.push(j("--type"));
                argv.push(t);
            }
            if let Some(clauses) = args.get("where").and_then(|w| w.as_array()) {
                for clause in clauses {
                    if let Some(clause) = clause.as_str() {
                        argv.push(j("--where"));
                        argv.push(clause.to_string());
                    }
                }
            }
            if let Some(limit) = opt_int(args, "limit") {
                argv.push(j("--limit"));
                argv.push(limit.to_string());
            }
            Ok(ToolAction::Verbs(vec![argv]))
        }
        "search" => {
            let pattern = req_str(args, "pattern")?;
            if pattern.starts_with('-') {
                return Err("`pattern` must not start with '-'".to_string());
            }
            let mut argv = vec![j("--json"), j("search"), pattern];
            if let Some(t) = opt_str(args, "type") {
                argv.push(j("--type"));
                argv.push(t);
            }
            let limit = opt_int(args, "limit").unwrap_or(50);
            argv.push(j("--limit"));
            argv.push(limit.to_string());
            Ok(ToolAction::Verbs(vec![argv]))
        }
        "show" => Ok(ToolAction::Verbs(vec![vec![
            j("--json"),
            j("show"),
            path_str(args, "file")?,
        ]])),
        "schema" => {
            let mut argv = vec![j("--json"), j("schema")];
            if let Some(t) = opt_str(args, "type") {
                argv.push(t);
            }
            Ok(ToolAction::Verbs(vec![argv]))
        }
        "tree" => {
            let mut argv = vec![j("--json"), j("tree")];
            if let Some(layer) = opt_str(args, "layer") {
                argv.push(j("--layer"));
                argv.push(layer);
            }
            if let Some(t) = opt_str(args, "type") {
                argv.push(j("--type"));
                argv.push(t);
            }
            Ok(ToolAction::Verbs(vec![argv]))
        }
        "log_tail" => {
            let n = opt_int(args, "n").unwrap_or(20).clamp(1, 500);
            Ok(ToolAction::Verbs(vec![vec![
                j("--json"),
                j("log"),
                j("tail"),
                n.to_string(),
            ]]))
        }
        "write" => {
            let path = path_str(args, "path")?;
            let mut argv = vec![
                j("--json"),
                j("write"),
                path.clone(),
                j("--type"),
                req_str(args, "type")?,
                j("--summary"),
                req_str(args, "summary")?,
            ];
            if let Some(entries) = args.get("fm").and_then(|f| f.as_array()) {
                for entry in entries {
                    if let Some(entry) = entry.as_str() {
                        argv.push(j("--fm"));
                        argv.push(entry.to_string());
                    }
                }
            }
            let mut steps = vec![argv];
            if let Some(body) = opt_str(args, "body") {
                steps.push(vec![
                    j("--json"),
                    j("body"),
                    j("set"),
                    path,
                    j("--text"),
                    body,
                ]);
            }
            Ok(ToolAction::Verbs(steps))
        }
        "fm_set" => {
            let file = path_str(args, "file")?;
            let key = req_str(args, "key")?;
            let value = req_str(args, "value")?;
            Ok(ToolAction::Verbs(vec![vec![
                j("--json"),
                j("fm"),
                j("set"),
                file,
                format!("{key}={value}"),
            ]]))
        }
        "body_set" => Ok(ToolAction::Verbs(vec![vec![
            j("--json"),
            j("body"),
            j("set"),
            path_str(args, "file")?,
            j("--text"),
            args.get("content")
                .and_then(|c| c.as_str())
                .ok_or("missing required string argument `content`")?
                .to_string(),
        ]])),
        "rm" => Ok(ToolAction::Verbs(vec![vec![
            j("--json"),
            j("rm"),
            path_str(args, "path")?,
        ]])),
        "log" => Ok(ToolAction::Verbs(vec![vec![
            j("--json"),
            j("log"),
            req_str(args, "kind")?,
            path_str(args, "object")?,
            j("-m"),
            req_str(args, "message")?,
        ]])),
        "list_files" => Ok(ToolAction::File(FileOp::List {
            dir: opt_str(args, "dir"),
        })),
        "read_file" => Ok(ToolAction::File(FileOp::Read {
            path: req_str(args, "path")?,
        })),
        "write_file" => Ok(ToolAction::File(FileOp::Write {
            path: req_str(args, "path")?,
            content: args
                .get("content")
                .and_then(|c| c.as_str())
                .ok_or("missing required string argument `content`")?
                .to_string(),
        })),
        "edit_file" => Ok(ToolAction::File(FileOp::Edit {
            path: req_str(args, "path")?,
            old_text: req_str(args, "old_text")?,
            new_text: args
                .get("new_text")
                .and_then(|c| c.as_str())
                .ok_or("missing required string argument `new_text`")?
                .to_string(),
        })),
        _ => unreachable!("membership checked above"),
    }
}

/// The human-readable one-liner for a planned action — what the demo renders
/// beside each tool call ("every view ends with the CLI one-liner it
/// mirrors", and now every tool call carries the CLI one-liner it executes).
pub fn display(action: &ToolAction) -> String {
    match action {
        ToolAction::Verbs(steps) => steps
            .iter()
            .map(|argv| {
                let mut line = String::from("dbmd");
                for arg in argv {
                    line.push(' ');
                    if arg.contains(' ') || arg.contains('\n') {
                        line.push('\'');
                        line.push_str(&arg.replace('\'', "'\\''").replace('\n', "\\n"));
                        line.push('\'');
                    } else {
                        line.push_str(arg);
                    }
                }
                if line.len() > 160 {
                    let cut = line
                        .char_indices()
                        .take_while(|(i, _)| *i < 157)
                        .last()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(line.len());
                    line.truncate(cut);
                    line.push_str("...");
                }
                line
            })
            .collect::<Vec<_>>()
            .join(" && "),
        ToolAction::File(op) => op.display(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_mask_excludes_writes() {
        let names: Vec<&str> = registry(Mask::Read).iter().map(|t| t.name).collect();
        assert!(names.contains(&"query"));
        assert!(!names.contains(&"write"));
        assert!(!names.contains(&"read_file"));
    }

    #[test]
    fn write_mask_excludes_file_tools() {
        let names: Vec<&str> = registry(Mask::Write).iter().map(|t| t.name).collect();
        assert!(names.contains(&"write"));
        assert!(!names.contains(&"write_file"));
    }

    #[test]
    fn unknown_tool_names_available_ones() {
        let error = plan(Mask::Read, "bash", &json!({})).expect_err("no shell tool, ever");
        assert!(error.contains("unknown tool"));
        assert!(error.contains("query"));
    }

    #[test]
    fn write_tool_refused_on_read_mask() {
        let error = plan(Mask::Read, "write", &json!({"path": "a.md"})).expect_err("masked out");
        assert!(error.contains("unknown tool"));
    }

    #[test]
    fn write_with_body_plans_two_steps() {
        let action = plan(
            Mask::Write,
            "write",
            &json!({"path": "records/todos/x.md", "type": "todo",
                    "summary": "X", "body": "- [ ] a"}),
        )
        .expect("plan");
        match action {
            ToolAction::Verbs(steps) => assert_eq!(steps.len(), 2),
            ToolAction::File(_) => panic!("verb plan expected"),
        }
    }

    #[test]
    fn flag_smuggling_is_refused() {
        let error = plan(Mask::Write, "rm", &json!({"path": "--force"})).expect_err("refused");
        assert!(error.contains("must not start with '-'"));
    }

    #[test]
    fn display_renders_cli_line() {
        let action = plan(Mask::Read, "search", &json!({"pattern": "(?i)milk"})).expect("plan");
        let line = display(&action);
        assert!(line.starts_with("dbmd --json search"));
        assert!(line.contains("(?i)milk"));
    }
}
