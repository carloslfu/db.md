// SPDX-License-Identifier: Apache-2.0

//! The embedded micro-harness (feature `harness`): a stateless tool-calling
//! loop that runs the **user's own** model endpoint against the store's verb
//! surface — `dbmd ask` (read verbs), `dbmd do` (adds store writes), and
//! `dbmd build` (adds workspace-scoped file operations).
//!
//! Doctrine (AGENTS.md "Hard rules"): every dbmd verb stays deterministic
//! plumbing; this module adds a *client for user-supplied intelligence*, the
//! way a database ships a shell. The covenant, enforced by construction:
//!
//! - **Hand-rolled wire protocols, no SDK crates.** Two neutral ones —
//!   Anthropic Messages and OpenAI-compatible Chat Completions ([`openai`],
//!   [`anthropic`], [`sse`]) — plus the ChatGPT backend's Responses format
//!   ([`codex`]), which exists only to spend a ChatGPT subscription and is
//!   reached only after an explicit `dbmd login codex`. All raw JSON over the
//!   `ureq` already in the tree; the `deny.toml` bans now enforce this
//!   covenant rather than contradict it.
//! - **No default vendor or endpoint** ([`config`]) — like the hub client, a
//!   model is whatever the user points the toolkit at. The API key is read
//!   from the environment ONLY, never from a file inside the store, and an
//!   endpoint that came from store-local config cannot borrow an ambient key
//!   without an explicit origin binding (a cloned store must not be able to
//!   exfiltrate a key).
//! - **Tools are the verbs** ([`tools`]) — every tool call is planned here and
//!   executed by the caller as a `dbmd` subcommand invocation (the CLI spawns
//!   `current_exe()`, exactly like `dbmd api` routes), so schema enforcement,
//!   frozen pages, the store transaction flock, write-through indexes, and
//!   log.md all apply identically. The `build` mask adds file operations
//!   confined beneath a declared workspace root ([`files`]); there is no
//!   shell tool at any mask.
//! - **Stateless one-shot.** The loop holds messages in memory for one run and
//!   persists nothing. Multi-turn state belongs to the caller.
//! - **Caps are first-class.** A turn ceiling with a final no-tools "answer
//!   with what you have" call, per-tool output truncation, and a bounded
//!   response reader.
//!
//! **Subscriptions, and the identity line.** A ChatGPT subscription is used
//! natively: [`oauth`] runs OpenAI's public PKCE flow (the one endorsed for
//! third-party OSS clients), [`auth`] stores the tokens outside any store,
//! and [`codex`] spends them — always identifying this toolkit honestly
//! (`originator: dbmd`), never posing as another vendor's first-party client.
//! Every other subscription (Claude Pro/Max, Copilot) is reached by
//! *delegation* — spawning the vendor's own logged-in CLI headless
//! ([`delegate`]). Anthropic's OAuth path is deliberately NOT implemented:
//! it only works by injecting a "You are Claude Code" system block and
//! `claude-code-*` beta headers so a subscription token is accepted, and
//! impersonating a vendor's own client is not something this toolkit does.

pub mod anthropic;
pub mod auth;
pub mod codex;
pub mod config;
pub mod delegate;
pub mod files;
pub mod oauth;
pub mod openai;
pub mod sse;
pub mod tools;

use serde::Serialize;

/// Which tool registry a run exposes. Each mask is a strict superset of the
/// previous one; the verb the user typed IS the permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mask {
    /// `dbmd ask` — read verbs only. Safe on untrusted content: a prompt
    /// injection can at worst produce a wrong answer, never a write.
    Read,
    /// `dbmd do` — adds the store write verbs. Every mutation still rides the
    /// full store contract (schema checks, frozen pages, lock, indexes, log).
    Write,
    /// `dbmd build` — adds file operations confined beneath the declared
    /// workspace root. CLI-only by design; never exposed over `dbmd api`.
    Build,
}

/// The wire protocol (or delegation backend) a resolved provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// OpenAI-compatible Chat Completions (`POST <base>/chat/completions`).
    /// Covers OpenAI, Ollama, LM Studio, llama.cpp server, vLLM, OpenRouter,
    /// Groq, and every other compat server.
    OpenAi,
    /// Anthropic Messages (`POST <base>/v1/messages`). Covers the Claude API
    /// and llama.cpp's native Anthropic endpoint.
    Anthropic,
    /// The ChatGPT backend's Responses API, driven by a subscription token
    /// from `dbmd login codex` ([`oauth`]).
    Codex,
    /// Delegate the whole request to a logged-in `claude` CLI (headless).
    ClaudeCli,
    /// Delegate the whole request to a logged-in `codex` CLI (headless).
    CodexCli,
}

impl Protocol {
    /// Whether this backend is a delegation to a vendor CLI (no wire adapter,
    /// no key, no tool loop of ours — the vendor agent runs its own).
    pub fn is_delegate(self) -> bool {
        matches!(self, Protocol::ClaudeCli | Protocol::CodexCli)
    }
}

/// One resolved model endpoint. Produced by [`config::resolve`]; consumed by
/// [`run`].
#[derive(Debug, Clone)]
pub struct Provider {
    /// The wire protocol or delegation backend.
    pub protocol: Protocol,
    /// Endpoint base URL (no trailing slash; empty for delegates). For
    /// `OpenAi` it INCLUDES the `/v1`-style prefix (`…/v1`); for `Anthropic`
    /// it excludes it (`/v1/messages` is appended).
    pub base_url: String,
    /// Model id sent on the wire (may be empty for delegates).
    pub model: String,
    /// Bearer / x-api-key credential. Environment-only by construction.
    pub key: Option<String>,
    /// One-line provenance note ("preset ollama, autodetected", …) for
    /// diagnostics; never contains the key.
    pub source: String,
}

/// Run limits and mode for one conversation.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Maximum model round-trips before the forced final answer. Each turn is
    /// one streamed completion call.
    pub max_turns: usize,
    /// `max_tokens` per completion call.
    pub max_tokens: u32,
    /// The tool registry mask this run exposes.
    pub mask: Mask,
    /// Working directory for delegation backends (the store root, or the
    /// workspace for `build`). Unused by wire protocols.
    pub delegate_cwd: Option<std::path::PathBuf>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            max_turns: 15,
            max_tokens: 4096,
            mask: Mask::Read,
            delegate_cwd: None,
        }
    }
}

/// One content block of a conversation message. The internal model that both
/// adapters translate to and from; assistant blocks are appended back
/// verbatim on the next turn (thinking signatures intact) so either protocol
/// round-trips its own output.
#[derive(Debug, Clone)]
pub enum Block {
    /// Plain text.
    Text(String),
    /// Model thinking. `signature` rides only the Anthropic protocol.
    Thinking {
        /// The thinking text (may be empty when the provider omits display).
        text: String,
        /// Anthropic block signature, required to resend the block.
        signature: Option<String>,
    },
    /// A tool invocation the assistant requested.
    ToolUse {
        /// Call id (synthesized when the provider omits one).
        id: String,
        /// Tool name as requested (may be unknown to the registry).
        name: String,
        /// Parsed arguments (`{}` when unparsable — see `raw_args`).
        args: serde_json::Value,
        /// The verbatim argument string as received; what OpenAI-compat
        /// resends. `None` when `args` was constructed locally.
        raw_args: Option<String>,
    },
    /// A tool result (rides user-role messages).
    ToolResult {
        /// The call this result answers.
        id: String,
        /// Tool name (some compat servers require it on the result).
        name: String,
        /// Result content, already truncated to the per-tool cap.
        content: String,
        /// Whether the tool failed (refusal, bad args, unknown tool).
        is_error: bool,
    },
}

/// Message role. `System` never appears in [`Msg`] — the system prompt is a
/// separate parameter on both protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The caller (and tool results).
    User,
    /// The model.
    Assistant,
}

/// One conversation message.
#[derive(Debug, Clone)]
pub struct Msg {
    /// Who authored the message.
    pub role: Role,
    /// Ordered content blocks.
    pub blocks: Vec<Block>,
}

impl Msg {
    /// A plain user text message.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            blocks: vec![Block::Text(text.into())],
        }
    }
}

/// Why a streamed turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The model finished its answer.
    EndTurn,
    /// The model requested tool calls.
    ToolUse,
    /// The provider cut the output at `max_tokens`.
    Length,
}

/// One finished assistant turn as returned by a wire adapter.
#[derive(Debug)]
pub struct Turn {
    /// The assistant message blocks, in stream order.
    pub blocks: Vec<Block>,
    /// Why the turn ended.
    pub stop: Stop,
    /// (input, output) token counts when the provider reported them.
    pub usage: Option<(u64, u64)>,
}

/// The flat event stream a run emits — consumed by the CLI renderer and
/// serialized verbatim (one JSON object per event) by `--json` mode and the
/// `/v1/ask` SSE route. Deliberately delta-only: no accumulated snapshots.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A fragment of model thinking.
    ThinkingDelta {
        /// The fragment.
        text: String,
    },
    /// A fragment of the model's answer text.
    TextDelta {
        /// The fragment.
        text: String,
    },
    /// A tool call is complete and about to execute. `display` is the exact
    /// CLI one-liner (or file-op summary) it maps to.
    ToolCall {
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed arguments.
        args: serde_json::Value,
        /// Human-readable invocation, e.g. `dbmd --json query --type todo`.
        display: String,
    },
    /// A tool finished.
    ToolResult {
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
        /// Result content (truncated to the per-tool cap).
        content: String,
        /// Whether the tool failed.
        is_error: bool,
    },
    /// Token usage for one completed model turn, when reported.
    Usage {
        /// Prompt-side tokens.
        input: u64,
        /// Completion-side tokens.
        output: u64,
    },
    /// One model turn ended.
    TurnEnd {
        /// `"end_turn"`, `"tool_use"`, or `"length"`.
        stop: String,
    },
    /// The run finished; `text` is the final answer.
    Done {
        /// The final answer text.
        text: String,
        /// Model round-trips used.
        turns: usize,
    },
    /// The run failed. Emitted just before the error return.
    Error {
        /// Human-readable failure.
        message: String,
    },
}

/// The outcome of executing one tool call. Content is already truncated by
/// the executor.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    /// Result text handed back to the model.
    pub content: String,
    /// Whether this is an error result.
    pub is_error: bool,
}

/// A harness failure.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// Configuration is missing or refused (no endpoint, key origin
    /// unbound, insecure scheme, unknown preset…).
    #[error("{0}")]
    Config(String),
    /// The provider (or delegation backend) failed.
    #[error("{0}")]
    Provider(String),
}

/// Executes tool calls for the engine. The CLI implements this by planning
/// via [`tools::plan`] and spawning `current_exe()` per verb step (or running
/// the file op via [`files`]).
pub trait ToolExecutor {
    /// Run one tool call and return its outcome. Must not panic on unknown
    /// tools or bad arguments — return an error outcome instead.
    fn execute(&mut self, name: &str, args: &serde_json::Value) -> ToolOutcome;

    /// The human-readable one-liner for a call (the CLI invocation it maps
    /// to), shown in the [`Event::ToolCall`] event before execution.
    fn describe(&self, name: &str, args: &serde_json::Value) -> String {
        let _ = args;
        name.to_string()
    }
}

/// The per-tool-result truncation cap, in bytes (head + tail are kept around
/// an elision marker so the model can re-query with limits).
pub const TOOL_RESULT_CAP: usize = 24_000;

/// Truncate one tool result to [`TOOL_RESULT_CAP`], keeping head and tail
/// with an explicit marker so the model narrows instead of silently losing
/// the tail.
pub fn truncate_tool_result(content: &str) -> String {
    if content.len() <= TOOL_RESULT_CAP {
        return content.to_string();
    }
    let head_end = floor_char_boundary(content, TOOL_RESULT_CAP * 2 / 3);
    let tail_start = ceil_char_boundary(content, content.len() - TOOL_RESULT_CAP / 4);
    format!(
        "{}\n[... truncated {} bytes — narrow the request (limit/type/path filters) and retry ...]\n{}",
        &content[..head_end],
        content.len() - head_end - (content.len() - tail_start),
        &content[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut at: usize) -> usize {
    at = at.min(s.len());
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_char_boundary(s: &str, mut at: usize) -> usize {
    at = at.min(s.len());
    while at < s.len() && !s.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Run one conversation to completion: stream turns, execute tool calls,
/// loop until the model answers or the turn cap forces a final answer.
/// Returns the final answer text (also emitted as [`Event::Done`]).
///
/// `messages` seeds the conversation (the prompt, or a caller-managed
/// multi-turn history ending in a user message). Nothing is persisted.
pub fn run(
    provider: &Provider,
    opts: &RunOptions,
    system: &str,
    mut messages: Vec<Msg>,
    tool_specs: &[tools::ToolSpec],
    executor: &mut dyn ToolExecutor,
    emit: &mut dyn FnMut(Event),
) -> Result<String, HarnessError> {
    if provider.protocol.is_delegate() {
        return delegate::run(provider, opts, system, &messages, emit);
    }

    let mut turns = 0usize;
    let mut final_text = String::new();
    loop {
        turns += 1;
        let capped = turns > opts.max_turns;
        // Past the cap: one last call with no tools and an explicit note, so
        // the run always ends with an answer instead of a dangling loop.
        if capped {
            messages.push(Msg::user(
                "[The tool-call limit for this run was reached. Answer now with \
                 what you already have; do not request more tools.]",
            ));
        }
        let specs: &[tools::ToolSpec] = if capped { &[] } else { tool_specs };
        let turn = match provider.protocol {
            Protocol::OpenAi => openai::stream_turn(provider, opts, system, &messages, specs, emit),
            Protocol::Anthropic => {
                anthropic::stream_turn(provider, opts, system, &messages, specs, emit)
            }
            Protocol::Codex => codex::stream_turn(provider, opts, system, &messages, specs, emit),
            Protocol::ClaudeCli | Protocol::CodexCli => unreachable!("delegates handled above"),
        };
        let turn = match turn {
            Ok(turn) => turn,
            Err(error) => {
                emit(Event::Error {
                    message: error.to_string(),
                });
                return Err(error);
            }
        };
        if let Some((input, output)) = turn.usage {
            emit(Event::Usage { input, output });
        }
        emit(Event::TurnEnd {
            stop: match turn.stop {
                Stop::EndTurn => "end_turn",
                Stop::ToolUse => "tool_use",
                Stop::Length => "length",
            }
            .to_string(),
        });

        for block in &turn.blocks {
            if let Block::Text(text) = block {
                if !final_text.is_empty() {
                    final_text.push('\n');
                }
                final_text.push_str(text);
            }
        }

        let calls: Vec<(String, String, serde_json::Value, Option<String>)> = turn
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::ToolUse {
                    id,
                    name,
                    args,
                    raw_args,
                } => Some((id.clone(), name.clone(), args.clone(), raw_args.clone())),
                _ => None,
            })
            .collect();

        messages.push(Msg {
            role: Role::Assistant,
            blocks: turn.blocks,
        });

        if calls.is_empty() || capped {
            let text = final_text.trim().to_string();
            emit(Event::Done {
                text: text.clone(),
                turns,
            });
            return Ok(text);
        }
        // The model is mid-task; text so far was preamble, not the answer.
        final_text.clear();

        // Execute every call of the turn, results appended in source order in
        // ONE user message (the Anthropic contract; harmless on OpenAI).
        let mut results: Vec<Block> = Vec::with_capacity(calls.len());
        for (id, name, args, raw_args) in calls {
            emit(Event::ToolCall {
                id: id.clone(),
                name: name.clone(),
                args: args.clone(),
                display: executor.describe(&name, &args),
            });
            let outcome = if raw_args.as_deref().is_some_and(|raw| {
                !raw.trim().is_empty() && serde_json::from_str::<serde_json::Value>(raw).is_err()
            }) {
                ToolOutcome {
                    content: format!(
                        "tool arguments were not valid JSON; re-issue the call. raw: {}",
                        truncate_tool_result(raw_args.as_deref().unwrap_or_default())
                    ),
                    is_error: true,
                }
            } else {
                executor.execute(&name, &args)
            };
            emit(Event::ToolResult {
                id: id.clone(),
                name: name.clone(),
                content: outcome.content.clone(),
                is_error: outcome.is_error,
            });
            results.push(Block::ToolResult {
                id,
                name,
                content: outcome.content,
                is_error: outcome.is_error,
            });
        }
        messages.push(Msg {
            role: Role::User,
            blocks: results,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_head_and_tail_with_marker() {
        let long = "a".repeat(30_000) + &"z".repeat(10_000);
        let cut = truncate_tool_result(&long);
        assert!(cut.len() < long.len());
        assert!(cut.starts_with('a'));
        assert!(cut.ends_with('z'));
        assert!(cut.contains("truncated"));
    }

    #[test]
    fn short_results_ride_verbatim() {
        assert_eq!(truncate_tool_result("ok"), "ok");
    }
}
