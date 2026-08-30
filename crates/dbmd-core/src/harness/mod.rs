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
//! Anthropic is reached through **Anthropic's own CLI** ([`ant`]): `ant auth
//! login` mints an OAuth profile, `ant auth print-credentials --access-token`
//! hands the short-lived token to a third-party process, and [`anthropic`]
//! spends it as `Authorization: Bearer` plus the documented
//! `anthropic-beta: oauth-2025-04-20`. That is the vendor's published
//! handoff for raw-HTTP clients, so no client id is borrowed and nothing
//! pretends to be Claude Code. What stays unimplemented is the *other*
//! Anthropic OAuth path — the one that works only by injecting a "You are
//! Claude Code" system block and `claude-code-*` beta headers so a
//! subscription token is accepted. Impersonating a vendor's first-party
//! client is not something this toolkit does, whichever vendor it is.
//!
//! Copilot, and a Claude Pro/Max subscription that the user would rather
//! drive through its own agent, are reached by *delegation* — spawning the
//! vendor's logged-in CLI headless ([`delegate`]).

pub mod ant;
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

/// Which spelling of `reasoning_effort` an OpenAI-compatible server accepts.
///
/// The field name is standard; the accepted *values* are not. Ollama takes
/// `none|low|medium|high|max`, OpenAI takes `minimal|low|medium|high`, and an
/// arbitrary compat server may take neither — which is why every mapping is
/// paired with the drop-and-retry recovery in [`openai::stream_turn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortDialect {
    /// OpenAI proper and most hosted compat servers.
    Standard,
    /// Ollama, which added `max` and `none` and has no `minimal`.
    Ollama,
}

/// How hard the model should think before answering.
///
/// One ladder for the whole toolkit, translated per protocol because no two
/// vendors spell the rungs the same way. `dbmd ask --effort max` means "the
/// most this provider offers", not a literal string on the wire.
///
/// Nothing here is a default: an unset effort sends **no** reasoning field at
/// all, leaving each provider on its own default (which for Ollama and for
/// Qwen3.8 means thinking already on, at `xhigh`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effort {
    /// Think as little as the provider allows; on backends with a true switch
    /// this disables reasoning outright.
    Off,
    /// The shortest real reasoning pass.
    Minimal,
    /// Brief reasoning.
    Low,
    /// Balanced.
    Medium,
    /// Thorough; the Anthropic API's own default.
    High,
    /// Above `high`. The best setting for most coding and agentic work on the
    /// providers that expose it.
    Xhigh,
    /// The most a provider offers, cost be damned.
    Max,
}

impl Effort {
    /// Every rung, lowest first — the order `--effort` documents and tests
    /// iterate.
    pub const ALL: [Effort; 7] = [
        Effort::Off,
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
    ];

    /// The canonical name, as accepted by `--effort` and printed back.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Off => "off",
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// Parse a user-supplied level. Accepts each vendor's spelling as an
    /// alias so a value copied out of `~/.codex/config.toml` or an Ollama
    /// flag just works.
    pub fn parse(raw: &str) -> Result<Effort, HarnessError> {
        let normalized = raw.trim().to_ascii_lowercase().replace(['-', '_'], "");
        Ok(match normalized.as_str() {
            "off" | "none" | "disabled" | "false" => Effort::Off,
            "minimal" | "min" => Effort::Minimal,
            "low" => Effort::Low,
            "medium" | "med" | "default" => Effort::Medium,
            "high" => Effort::High,
            "xhigh" | "extrahigh" | "veryhigh" => Effort::Xhigh,
            "max" | "maximum" | "highest" => Effort::Max,
            _ => {
                let names: Vec<&str> = Effort::ALL.iter().map(|e| e.as_str()).collect();
                return Err(HarnessError::Config(format!(
                    "unknown effort `{raw}` — use one of: {}",
                    names.join(", ")
                )));
            }
        })
    }

    /// The value for an OpenAI-compatible `reasoning_effort` field.
    ///
    /// Rungs a dialect lacks collapse onto its nearest neighbour rather than
    /// erroring, so `--effort max` against OpenAI proper means `high` instead
    /// of a 400.
    pub fn openai(self, dialect: EffortDialect) -> &'static str {
        match dialect {
            // Probed live against Ollama 0.32.15, whose validator names its
            // own set: minimal, low, medium, high, xhigh, ultra, max, none.
            // That is a superset of this ladder, so every rung passes through
            // by name. (`ultra` sits between xhigh and max there; the ladder
            // does not expose it, because `max` already means "the most this
            // provider offers" on every other backend.)
            EffortDialect::Ollama => match self {
                Effort::Off => "none",
                Effort::Minimal => "minimal",
                Effort::Low => "low",
                Effort::Medium => "medium",
                Effort::High => "high",
                Effort::Xhigh => "xhigh",
                Effort::Max => "max",
            },
            EffortDialect::Standard => match self {
                // `none` is the only value that actually DISABLES reasoning:
                // llama.cpp documents it exactly that way, vLLM accepts it,
                // and GPT-5.1 added it. `minimal` is a short think, not off —
                // sending it for `--effort off` would leave thinking on for
                // the local models that default it on, which is the main
                // reason to reach for `off` at all.
                Effort::Off => "none",
                Effort::Minimal => "minimal",
                Effort::Low => "low",
                Effort::Medium => "medium",
                Effort::High | Effort::Xhigh | Effort::Max => "high",
            },
        }
    }

    /// A second spelling to try when [`Effort::openai`] is refused, before
    /// giving up on the field entirely.
    ///
    /// Older Ollama builds accepted only `none|low|medium|high|max`, so a
    /// rung outside that set retries as its nearest member instead of losing
    /// the setting. `None` means the primary spelling is the only one worth
    /// trying.
    pub fn openai_fallback(self, dialect: EffortDialect) -> Option<&'static str> {
        match dialect {
            EffortDialect::Ollama => match self {
                Effort::Minimal => Some("low"),
                Effort::Xhigh => Some("max"),
                _ => None,
            },
            // Endpoints predating `none` still take `minimal`, the closest
            // thing to off they have.
            EffortDialect::Standard => match self {
                Effort::Off => Some("minimal"),
                _ => None,
            },
        }
    }

    /// The value for the ChatGPT Responses `reasoning.effort` field.
    ///
    /// The backend rejected `minimal` live on `gpt-5.6-sol` and named its own
    /// set in the 400: `none, low, medium, high, xhigh, max`. So this is the
    /// one dialect that carries the whole ladder except `minimal`, and `max`
    /// is a real rung above `xhigh` rather than an alias for it.
    pub fn codex(self) -> &'static str {
        match self {
            Effort::Off => "none",
            Effort::Minimal | Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// The value for Anthropic's `output_config.effort`, or `None` for
    /// [`Effort::Off`] (which disables thinking instead of setting a level).
    pub fn anthropic(self) -> Option<&'static str> {
        match self {
            Effort::Off => None,
            Effort::Minimal | Effort::Low => Some("low"),
            Effort::Medium => Some("medium"),
            Effort::High => Some("high"),
            Effort::Xhigh => Some("xhigh"),
            Effort::Max => Some("max"),
        }
    }

    /// The `thinking.budget_tokens` value for pre-4.6 Anthropic models, which
    /// predate `output_config.effort`. Only used by the legacy retry.
    pub fn anthropic_budget(self) -> Option<u32> {
        match self {
            Effort::Off => None,
            Effort::Minimal => Some(1024),
            Effort::Low => Some(2048),
            Effort::Medium => Some(8192),
            Effort::High => Some(16384),
            Effort::Xhigh => Some(24576),
            Effort::Max => Some(32768),
        }
    }
}

/// Per-run memory of which request variant an endpoint actually accepted.
///
/// Without this, a server that refuses `reasoning_effort` pays the rejected
/// round-trip on *every* turn of a run — up to `max_turns` wasted requests,
/// and up to twice that on Anthropic, which has two shapes to try. The
/// harness stays stateless across runs; this is scoped to one [`run`] call.
#[derive(Debug, Default)]
pub struct Negotiated {
    /// Index of the first variant worth trying, learned from earlier turns.
    first: std::cell::Cell<usize>,
}

impl Negotiated {
    /// The variant index to start from.
    pub fn start(&self) -> usize {
        self.first.get()
    }

    /// Record the variant the endpoint accepted, so later turns skip the
    /// ones it already refused.
    pub fn accepted(&self, index: usize) {
        self.first.set(index);
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
    /// Bearer / x-api-key credential. Environment-only by construction,
    /// except the two subscription paths ([`oauth`] for ChatGPT, [`ant`] for
    /// Anthropic), which mint their own short-lived tokens.
    pub key: Option<String>,
    /// Whether `key` is an OAuth bearer token rather than an API key. Selects
    /// the auth header shape, and on Anthropic also the required beta header.
    pub oauth: bool,
    /// One-line provenance note ("preset ollama, autodetected", …) for
    /// diagnostics; never contains the key.
    pub source: String,
    /// Which `reasoning_effort` vocabulary this endpoint accepts. Only
    /// consulted by the OpenAI-compatible adapter.
    pub effort_dialect: EffortDialect,
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
    /// How hard the model should think. `None` sends no reasoning field at
    /// all, leaving the provider on its own default.
    pub effort: Option<Effort>,
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
            effort: None,
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
    /// A non-fatal adjustment the harness made on the user's behalf — a
    /// reasoning parameter the provider refused and we dropped, say. Surfaced
    /// so a silently-downgraded run never looks like a clean one.
    Notice {
        /// Human-readable note.
        message: String,
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
    // One negotiation memo for the whole run: what the endpoint refused on
    // turn one is not offered again on turn two.
    let negotiated = Negotiated::default();
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
            Protocol::OpenAi => {
                openai::stream_turn(provider, opts, system, &messages, specs, &negotiated, emit)
            }
            Protocol::Anthropic => {
                anthropic::stream_turn(provider, opts, system, &messages, specs, &negotiated, emit)
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
mod effort_tests {
    use super::*;

    #[test]
    fn parse_accepts_each_vendors_spelling() {
        // A level copied out of ~/.codex/config.toml, an Ollama flag, or an
        // Anthropic doc must all land on the same rung.
        assert_eq!(Effort::parse("xhigh").unwrap(), Effort::Xhigh);
        assert_eq!(Effort::parse("x-high").unwrap(), Effort::Xhigh);
        assert_eq!(Effort::parse("  HIGH ").unwrap(), Effort::High);
        assert_eq!(Effort::parse("none").unwrap(), Effort::Off);
        assert_eq!(Effort::parse("maximum").unwrap(), Effort::Max);
        assert_eq!(Effort::parse("min").unwrap(), Effort::Minimal);
    }

    #[test]
    fn parse_rejects_unknown_and_lists_the_rungs() {
        let error = Effort::parse("turbo").expect_err("turbo is not a level");
        let message = error.to_string();
        for rung in Effort::ALL {
            assert!(
                message.contains(rung.as_str()),
                "error should list `{}`: {message}",
                rung.as_str()
            );
        }
    }

    #[test]
    fn ollama_dialect_passes_every_rung_through_by_name() {
        // Ollama 0.32.15's validator accepts a superset of this ladder, so
        // nothing is lost in translation. `off` is `none`, not a dropped
        // field, because Ollama enables thinking on its own otherwise.
        assert_eq!(Effort::Off.openai(EffortDialect::Ollama), "none");
        assert_eq!(Effort::Minimal.openai(EffortDialect::Ollama), "minimal");
        assert_eq!(Effort::Xhigh.openai(EffortDialect::Ollama), "xhigh");
        assert_eq!(Effort::Max.openai(EffortDialect::Ollama), "max");
    }

    #[test]
    fn ollama_rungs_outside_the_older_set_have_a_fallback() {
        // Older builds took only none|low|medium|high|max. A refused rung
        // retries as its nearest member rather than losing the setting.
        assert_eq!(
            Effort::Minimal.openai_fallback(EffortDialect::Ollama),
            Some("low")
        );
        assert_eq!(
            Effort::Xhigh.openai_fallback(EffortDialect::Ollama),
            Some("max")
        );
        assert_eq!(Effort::High.openai_fallback(EffortDialect::Ollama), None);
        assert_eq!(Effort::Max.openai_fallback(EffortDialect::Standard), None);
    }

    #[test]
    fn standard_dialect_clamps_rungs_openai_lacks() {
        // OpenAI proper has no xhigh/max on chat completions, so the top of
        // the ladder collapses onto `high` rather than erroring.
        assert_eq!(Effort::Xhigh.openai(EffortDialect::Standard), "high");
        assert_eq!(Effort::Max.openai(EffortDialect::Standard), "high");
    }

    #[test]
    fn off_means_off_on_local_servers_not_a_short_think() {
        // The bug this pins: `minimal` is a short think, not off. llama.cpp
        // documents `none` as the value that disables reasoning, vLLM takes
        // it, and local reasoning models default thinking ON — so sending
        // `minimal` for `--effort off` would leave it on for exactly the
        // models people reach for `off` to quiet.
        assert_eq!(Effort::Off.openai(EffortDialect::Standard), "none");
        assert_eq!(Effort::Off.openai(EffortDialect::Ollama), "none");
        // Endpoints predating `none` still get the nearest thing they have.
        assert_eq!(
            Effort::Off.openai_fallback(EffortDialect::Standard),
            Some("minimal")
        );
    }

    #[test]
    fn the_negotiation_memo_starts_at_zero_and_remembers() {
        let memo = Negotiated::default();
        assert_eq!(memo.start(), 0);
        memo.accepted(1);
        assert_eq!(memo.start(), 1, "a refused shape is not re-offered");
    }

    #[test]
    fn codex_and_anthropic_expose_their_top_rungs() {
        // Verified against the live ChatGPT backend, which rejected `minimal`
        // and listed: none, low, medium, high, xhigh, max.
        assert_eq!(Effort::Max.codex(), "max");
        assert_eq!(Effort::Xhigh.codex(), "xhigh");
        assert_eq!(Effort::Off.codex(), "none");
        assert_ne!(
            Effort::Minimal.codex(),
            "minimal",
            "the Responses backend has no `minimal` rung"
        );
        assert_eq!(Effort::Max.anthropic(), Some("max"));
        assert_eq!(Effort::Xhigh.anthropic(), Some("xhigh"));
        // Off is the one rung with no Anthropic effort value: it disables
        // thinking instead of naming a level.
        assert_eq!(Effort::Off.anthropic(), None);
        assert_eq!(Effort::Off.anthropic_budget(), None);
    }

    #[test]
    fn budgets_rise_with_the_ladder() {
        let budgets: Vec<u32> = Effort::ALL
            .iter()
            .filter_map(|e| e.anthropic_budget())
            .collect();
        assert!(
            budgets.windows(2).all(|w| w[0] < w[1]),
            "legacy budgets must increase monotonically: {budgets:?}"
        );
        // The Anthropic minimum for an enabled thinking budget is 1024.
        assert!(budgets.iter().all(|b| *b >= 1024));
    }

    #[test]
    fn default_run_options_send_no_reasoning_field() {
        // The whole point of `None`: an unset effort must not silently pick a
        // level, because each provider's own default differs (Ollama already
        // thinks; OpenAI does not).
        assert_eq!(RunOptions::default().effort, None);
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
