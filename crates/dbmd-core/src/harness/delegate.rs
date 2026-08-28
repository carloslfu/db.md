// SPDX-License-Identifier: Apache-2.0

//! Delegation backends: subscription logins (Claude Pro/Max, ChatGPT/Codex)
//! reached by spawning the vendor's OWN logged-in CLI headless — never by
//! reimplementing a vendor OAuth flow in this repo. The vendor client makes
//! the API call under its own identity; dbmd ships zero OAuth code, and every
//! logged-in agent CLI on the machine becomes a provider.
//!
//! Semantics differ from the wire protocols by design: the vendor agent runs
//! its OWN loop with its own tools in the given working directory (the store
//! root, or the workspace for `build`) — the mask maps onto the vendor's
//! sandbox/permission flags rather than onto our tool registry:
//!
//! - `claude -p` — `ask` runs in plan mode (read-only); `do`/`build` run with
//!   `--permission-mode acceptEdits`.
//! - `codex exec` — `ask` runs `--sandbox read-only`; `do`/`build` run
//!   `--sandbox workspace-write`.
//!
//! Both are marked experimental: headless flags drift across vendor
//! releases, and the JSON event stream is parsed defensively (text is
//! collected from the shapes the CLIs are known to emit; unknown lines are
//! skipped). Binaries resolve from `PATH`, overridable with
//! `DBMD_ASK_CLAUDE_BIN` / `DBMD_ASK_CODEX_BIN` (also the test seam).

use std::io::{BufRead, BufReader};
use std::process::Stdio;

use serde_json::Value;

use super::{Block, Event, HarnessError, Mask, Msg, Protocol, Provider, Role, RunOptions};

/// Env override for the `claude` binary (test seam included).
pub const CLAUDE_BIN_ENV: &str = "DBMD_ASK_CLAUDE_BIN";
/// Env override for the `codex` binary (test seam included).
pub const CODEX_BIN_ENV: &str = "DBMD_ASK_CODEX_BIN";

fn binary(env: &str, default: &str) -> String {
    std::env::var(env)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The single prompt a delegated run hands the vendor agent: the last user
/// text (delegates are one-shot; caller-managed histories are flattened).
fn flatten_prompt(messages: &[Msg]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in messages {
        for block in &msg.blocks {
            if let Block::Text(text) = block {
                let speaker = match msg.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                parts.push(format!("[{speaker}] {text}"));
            }
        }
    }
    if parts.len() == 1 {
        parts
            .pop()
            .map(|p| p.trim_start_matches("[user] ").to_string())
            .unwrap_or_default()
    } else {
        parts.join("\n\n")
    }
}

/// Run one delegated conversation. Streams whatever text the vendor CLI
/// reports and returns the final answer.
pub fn run(
    provider: &Provider,
    opts: &RunOptions,
    system: &str,
    messages: &[Msg],
    emit: &mut dyn FnMut(Event),
) -> Result<String, HarnessError> {
    let prompt = flatten_prompt(messages);
    let cwd = opts
        .delegate_cwd
        .clone()
        .ok_or_else(|| HarnessError::Config("delegation needs a working directory".to_string()))?;

    let mut command = match provider.protocol {
        Protocol::ClaudeCli => {
            let mut command = std::process::Command::new(binary(CLAUDE_BIN_ENV, "claude"));
            command
                .arg("-p")
                .arg(&prompt)
                .arg("--output-format")
                .arg("stream-json")
                .arg("--verbose")
                .arg("--append-system-prompt")
                .arg(system);
            match opts.mask {
                Mask::Read => {
                    command.arg("--permission-mode").arg("plan");
                }
                Mask::Write | Mask::Build => {
                    command.arg("--permission-mode").arg("acceptEdits");
                }
            }
            command
        }
        Protocol::CodexCli => {
            let mut command = std::process::Command::new(binary(CODEX_BIN_ENV, "codex"));
            // A db.md store need not be a git repository; without this flag
            // headless codex refuses any untrusted (non-repo) directory.
            command
                .arg("exec")
                .arg("--json")
                .arg("--skip-git-repo-check");
            match opts.mask {
                Mask::Read => {
                    command.arg("--sandbox").arg("read-only");
                }
                Mask::Write | Mask::Build => {
                    command.arg("--sandbox").arg("workspace-write");
                }
            }
            // Codex has no append-system flag; the operating contract rides
            // ahead of the prompt.
            command.arg(format!("{system}\n\n---\n\n{prompt}"));
            command
        }
        _ => {
            return Err(HarnessError::Config(
                "not a delegation provider".to_string(),
            ))
        }
    };

    let child = command
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            let name = match provider.protocol {
                Protocol::ClaudeCli => "claude",
                _ => "codex",
            };
            return Err(HarnessError::Provider(format!(
                "cannot run the `{name}` CLI ({error}) — is it installed and \
                 logged in? Delegation uses the vendor's own CLI; install it, \
                 or point {CLAUDE_BIN_ENV}/{CODEX_BIN_ENV} at it"
            )));
        }
    };

    let stdout = child.stdout.take();
    let mut final_text = String::new();
    let mut streamed = String::new();
    if let Some(stdout) = stdout {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue; // non-JSON noise (banners, progress)
            };
            for text in extract_texts(&event) {
                streamed.push_str(&text);
                emit(Event::TextDelta { text });
            }
            if let Some(result) = extract_result(&event) {
                final_text = result;
            }
        }
    }
    let status = child.wait();
    let ok = status.as_ref().map(|s| s.success()).unwrap_or(false);
    if !ok && final_text.is_empty() && streamed.is_empty() {
        let mut stderr_text = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut stderr_text);
        }
        stderr_text.truncate(600);
        return Err(HarnessError::Provider(format!(
            "the delegated CLI failed: {}",
            if stderr_text.trim().is_empty() {
                "no output".to_string()
            } else {
                stderr_text
            }
        )));
    }
    let answer = if final_text.is_empty() {
        streamed.trim().to_string()
    } else {
        final_text
    };
    emit(Event::Done {
        text: answer.clone(),
        turns: 1,
    });
    Ok(answer)
}

/// Assistant-text fragments from one vendor event line, across the shapes
/// the CLIs are known to emit.
fn extract_texts(event: &Value) -> Vec<String> {
    let mut out = Vec::new();
    // claude stream-json: {"type":"assistant","message":{"content":[{"type":"text","text":…}]}}
    if event.get("type").and_then(|t| t.as_str()) == Some("assistant") {
        if let Some(content) = event.pointer("/message/content").and_then(|c| c.as_array()) {
            for block in content {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        out.push(text.to_string());
                    }
                }
            }
        }
    }
    // codex --json: {"type":"item.completed","item":{"item_type":"agent_message","text":…}}
    if event.get("type").and_then(|t| t.as_str()) == Some("item.completed") {
        let item = event.get("item").unwrap_or(&Value::Null);
        let kind = item
            .get("item_type")
            .or_else(|| item.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if kind.contains("message") {
            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                out.push(text.to_string());
            }
        }
    }
    // older codex: {"msg":{"type":"agent_message","message":…}}
    if event.pointer("/msg/type").and_then(|t| t.as_str()) == Some("agent_message") {
        if let Some(text) = event.pointer("/msg/message").and_then(|m| m.as_str()) {
            out.push(text.to_string());
        }
    }
    out
}

/// The final-result field, when the vendor CLI reports one.
fn extract_result(event: &Value) -> Option<String> {
    if event.get("type").and_then(|t| t.as_str()) == Some("result") {
        if let Some(result) = event.get("result").and_then(|r| r.as_str()) {
            return Some(result.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_stream_json_text_is_extracted() {
        let event = json!({"type":"assistant","message":{"content":[
            {"type":"text","text":"hello"},{"type":"tool_use","id":"x"}]}});
        assert_eq!(extract_texts(&event), vec!["hello".to_string()]);
    }

    #[test]
    fn codex_item_completed_text_is_extracted() {
        let event =
            json!({"type":"item.completed","item":{"item_type":"agent_message","text":"done"}});
        assert_eq!(extract_texts(&event), vec!["done".to_string()]);
    }

    #[test]
    fn result_event_wins_as_final() {
        let event = json!({"type":"result","subtype":"success","result":"the answer"});
        assert_eq!(extract_result(&event).as_deref(), Some("the answer"));
    }
}
