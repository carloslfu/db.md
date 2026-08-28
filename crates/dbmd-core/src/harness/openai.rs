// SPDX-License-Identifier: Apache-2.0

//! The OpenAI-compatible Chat Completions adapter — hand-rolled request
//! builder + streaming parser over `ureq`, written against real servers, not
//! the spec. Chat Completions is the de-facto compat standard (Ollama, LM
//! Studio, llama.cpp, vLLM, OpenRouter, Groq); the tolerance contract below
//! is the reason this is not a typed SDK:
//!
//! - tool-call arguments may arrive fragmented across chunks (OpenAI, vLLM)
//!   or complete in one chunk (Ollama's parser emits whole calls);
//! - `index` may be missing, or stuck at 0 for parallel calls (shipped
//!   Ollama bug) — calls are correlated by `id` when present, else a chunk
//!   carrying a `function.name` starts a new call;
//! - `id` may be missing entirely — one is synthesized for the result
//!   round-trip;
//! - `finish_reason` may say `"stop"` even though tool calls were emitted —
//!   "run tools" is decided from the accumulated calls, never finish_reason;
//! - `usage` may be absent, or arrive in a final chunk whose `choices` is
//!   empty — nothing indexes `choices[0]` unguarded;
//! - `tool_choice` / `parallel_tool_calls` / `strict` are never sent (Ollama
//!   documents tool_choice unsupported); `max_tokens` is sent rather than
//!   `max_completion_tokens` (the field local servers universally know).

use serde_json::{json, Value};

use super::sse::read_events;
use super::tools::ToolSpec;
use super::{Block, Event, HarnessError, Msg, Provider, Role, RunOptions, Stop, Turn};

/// Build the wire `messages` array from the internal conversation.
fn wire_messages(system: &str, messages: &[Msg]) -> Vec<Value> {
    let mut wire = vec![json!({ "role": "system", "content": system })];
    for msg in messages {
        match msg.role {
            Role::User => {
                // Tool results ride as individual `role:"tool"` messages (in
                // order); plain text as user messages.
                for block in &msg.blocks {
                    match block {
                        Block::Text(text) => {
                            wire.push(json!({ "role": "user", "content": text }));
                        }
                        Block::ToolResult {
                            id,
                            name,
                            content,
                            is_error,
                        } => {
                            let content = if *is_error {
                                format!("ERROR: {content}")
                            } else {
                                content.clone()
                            };
                            wire.push(json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "name": name,
                                "content": content,
                            }));
                        }
                        _ => {}
                    }
                }
            }
            Role::Assistant => {
                let text: String = msg
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let calls: Vec<Value> = msg
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse {
                            id,
                            name,
                            args,
                            raw_args,
                        } => {
                            let arguments = raw_args.clone().unwrap_or_else(|| args.to_string());
                            Some(json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": arguments },
                            }))
                        }
                        _ => None,
                    })
                    .collect();
                if text.is_empty() && calls.is_empty() {
                    continue; // some providers reject empty assistant turns
                }
                let mut entry = serde_json::Map::new();
                entry.insert("role".into(), json!("assistant"));
                entry.insert(
                    "content".into(),
                    if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    },
                );
                if !calls.is_empty() {
                    entry.insert("tool_calls".into(), json!(calls));
                }
                wire.push(Value::Object(entry));
            }
        }
    }
    wire
}

/// One in-flight tool call being accumulated from stream deltas.
struct PendingCall {
    index: Option<u64>,
    id: String,
    name: String,
    args_buf: String,
}

/// Route one tool-call delta onto the pending set, tolerating every observed
/// server shape (see the module doc).
fn accept_delta(pending: &mut Vec<PendingCall>, delta: &Value) {
    let index = delta.get("index").and_then(|i| i.as_u64());
    let id = delta.get("id").and_then(|i| i.as_str()).unwrap_or("");
    let name = delta
        .pointer("/function/name")
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let args = delta
        .pointer("/function/arguments")
        .and_then(|a| a.as_str())
        .unwrap_or("");

    // 1) `id` is the strongest key.
    if !id.is_empty() {
        if let Some(call) = pending.iter_mut().find(|c| c.id == id) {
            if !name.is_empty() && call.name.is_empty() {
                call.name = name.to_string();
            }
            call.args_buf.push_str(args);
            return;
        }
    }
    // 2) A named delta targeting an index whose slot already holds a
    //    *different, completed-looking* call is the Ollama repeated-index bug:
    //    start a new call instead of merging.
    if !name.is_empty() {
        let reuse = pending
            .iter_mut()
            .find(|c| index.is_some() && c.index == index && (c.name.is_empty() || c.name == name));
        if let Some(call) = reuse {
            if call.name.is_empty() {
                call.name = name.to_string();
            }
            if !id.is_empty() {
                call.id = id.to_string();
            }
            call.args_buf.push_str(args);
        } else {
            pending.push(PendingCall {
                index,
                id: id.to_string(),
                name: name.to_string(),
                args_buf: args.to_string(),
            });
        }
        return;
    }
    // 3) Unnamed continuation: by index when it matches, else the last call.
    let slot = match index {
        Some(_) => pending.iter_mut().rev().find(|c| c.index == index),
        None => pending.last_mut(),
    };
    if let Some(call) = slot {
        call.args_buf.push_str(args);
    } else if !args.is_empty() {
        pending.push(PendingCall {
            index,
            id: id.to_string(),
            name: String::new(),
            args_buf: args.to_string(),
        });
    }
}

/// Statuses worth one retry round (pre-stream only — a request that already
/// streamed bytes is never retried).
fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 529)
}

pub(super) fn send_with_retries(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(&str, String)],
    body: &str,
) -> Result<ureq::Response, HarnessError> {
    let backoff_ms = [500u64, 1500u64];
    let mut attempt = 0usize;
    loop {
        let mut request = agent.post(url).set("content-type", "application/json");
        for (name, value) in headers {
            request = request.set(name, value);
        }
        match request.send_string(body) {
            Ok(response) => return Ok(response),
            Err(ureq::Error::Status(status, response)) => {
                let detail = response
                    .into_string()
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        let mut s = s;
                        s.truncate(600);
                        s
                    })
                    .unwrap_or_default();
                if retryable_status(status) && attempt < backoff_ms.len() {
                    std::thread::sleep(std::time::Duration::from_millis(backoff_ms[attempt]));
                    attempt += 1;
                    continue;
                }
                return Err(HarnessError::Provider(format!(
                    "provider returned HTTP {status}: {detail}"
                )));
            }
            Err(ureq::Error::Transport(transport)) => {
                if attempt < backoff_ms.len() {
                    std::thread::sleep(std::time::Duration::from_millis(backoff_ms[attempt]));
                    attempt += 1;
                    continue;
                }
                return Err(HarnessError::Provider(format!(
                    "cannot reach the provider: {transport}"
                )));
            }
        }
    }
}

/// The streaming agent: connect + idle-read timeouts only — an agent-level
/// overall timeout would kill long generations mid-stream.
pub(super) fn streaming_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(300))
        .timeout_write(std::time::Duration::from_secs(60))
        .build()
}

/// Stream one assistant turn.
pub fn stream_turn(
    provider: &Provider,
    opts: &RunOptions,
    system: &str,
    messages: &[Msg],
    tools: &[ToolSpec],
    emit: &mut dyn FnMut(Event),
) -> Result<Turn, HarnessError> {
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(provider.model));
    body.insert("messages".into(), json!(wire_messages(system, messages)));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    body.insert("max_tokens".into(), json!(opts.max_tokens));
    if !tools.is_empty() {
        let tools: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
        body.insert("tools".into(), json!(tools));
    }

    let mut headers: Vec<(&str, String)> = vec![("accept", "text/event-stream".to_string())];
    if let Some(key) = &provider.key {
        headers.push(("authorization", format!("Bearer {key}")));
    }

    let agent = streaming_agent();
    let response = send_with_retries(&agent, &url, &headers, &Value::Object(body).to_string())?;

    let mut text = String::new();
    let mut thinking = String::new();
    let mut pending: Vec<PendingCall> = Vec::new();
    let mut finish: Option<String> = None;
    let mut usage: Option<(u64, u64)> = None;
    let mut stream_error: Option<String> = None;

    let reader = response.into_reader();
    read_events(reader, |event| {
        if event.data == "[DONE]" {
            return false;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(&event.data) else {
            return true; // tolerate non-JSON frames
        };
        if let Some(error) = chunk.get("error") {
            let message = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("provider stream error");
            stream_error = Some(message.to_string());
            return false;
        }
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            let input = u.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
            let output = u
                .get("completion_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            usage = Some((input, output));
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
        else {
            return true; // usage-only chunk: choices empty or absent
        };
        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            finish = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return true;
        };
        if let Some(fragment) = delta.get("content").and_then(|c| c.as_str()) {
            if !fragment.is_empty() {
                text.push_str(fragment);
                emit(Event::TextDelta {
                    text: fragment.to_string(),
                });
            }
        }
        for field in ["reasoning_content", "reasoning", "reasoning_text"] {
            if let Some(fragment) = delta.get(field).and_then(|c| c.as_str()) {
                if !fragment.is_empty() {
                    thinking.push_str(fragment);
                    emit(Event::ThinkingDelta {
                        text: fragment.to_string(),
                    });
                    break;
                }
            }
        }
        if let Some(deltas) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for delta in deltas {
                accept_delta(&mut pending, delta);
            }
        }
        true
    })
    .map_err(|error| HarnessError::Provider(format!("stream read failed: {error}")))?;

    if let Some(message) = stream_error {
        return Err(HarnessError::Provider(format!(
            "provider stream error: {message}"
        )));
    }

    let mut blocks: Vec<Block> = Vec::new();
    if !thinking.is_empty() {
        blocks.push(Block::Thinking {
            text: thinking,
            signature: None,
        });
    }
    if !text.is_empty() {
        blocks.push(Block::Text(text));
    }
    let mut synthesized = 0usize;
    for call in pending {
        if call.name.is_empty() {
            continue; // an argument fragment that never gained a name
        }
        let id = if call.id.is_empty() {
            synthesized += 1;
            format!("call_{synthesized}")
        } else {
            call.id
        };
        let raw = call.args_buf.trim().to_string();
        let args = if raw.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&raw).unwrap_or(json!({}))
        };
        blocks.push(Block::ToolUse {
            id,
            name: call.name,
            args,
            raw_args: if raw.is_empty() { None } else { Some(raw) },
        });
    }

    let has_calls = blocks.iter().any(|b| matches!(b, Block::ToolUse { .. }));
    // "Run tools" is decided from the accumulated calls — finish_reason lies
    // on real servers (see module doc).
    let stop = if has_calls {
        Stop::ToolUse
    } else {
        match finish.as_deref() {
            Some("length") => Stop::Length,
            _ => Stop::EndTurn,
        }
    };
    if !has_calls && finish.is_none() && blocks.is_empty() {
        return Err(HarnessError::Provider(
            "the stream ended without content or a finish reason".to_string(),
        ));
    }

    Ok(Turn {
        blocks,
        stop,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_arguments_accumulate_by_index() {
        let mut pending = Vec::new();
        accept_delta(
            &mut pending,
            &json!({"index": 0, "id": "call_a", "function": {"name": "query", "arguments": ""}}),
        );
        accept_delta(
            &mut pending,
            &json!({"index": 0, "function": {"arguments": "{\"ty"}}),
        );
        accept_delta(
            &mut pending,
            &json!({"index": 0, "function": {"arguments": "pe\":\"todo\"}"}}),
        );
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].args_buf, "{\"type\":\"todo\"}");
    }

    #[test]
    fn ollama_repeated_index_zero_starts_new_calls() {
        let mut pending = Vec::new();
        accept_delta(
            &mut pending,
            &json!({"index": 0, "function": {"name": "query", "arguments": "{}"}}),
        );
        accept_delta(
            &mut pending,
            &json!({"index": 0, "function": {"name": "search", "arguments": "{\"pattern\":\"x\"}"}}),
        );
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].name, "query");
        assert_eq!(pending[1].name, "search");
    }

    #[test]
    fn missing_index_and_id_appends_to_last() {
        let mut pending = Vec::new();
        accept_delta(
            &mut pending,
            &json!({"function": {"name": "show", "arguments": "{\"fi"}}),
        );
        accept_delta(
            &mut pending,
            &json!({"function": {"arguments": "le\":\"a\"}"}}),
        );
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].args_buf, "{\"file\":\"a\"}");
    }

    #[test]
    fn assistant_history_round_trips_tool_calls() {
        let wire = wire_messages(
            "sys",
            &[
                Msg::user("hi"),
                Msg {
                    role: Role::Assistant,
                    blocks: vec![Block::ToolUse {
                        id: "call_1".into(),
                        name: "query".into(),
                        args: json!({"type": "todo"}),
                        raw_args: Some("{\"type\":\"todo\"}".into()),
                    }],
                },
                Msg {
                    role: Role::User,
                    blocks: vec![Block::ToolResult {
                        id: "call_1".into(),
                        name: "query".into(),
                        content: "[]".into(),
                        is_error: false,
                    }],
                },
            ],
        );
        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(wire[3]["role"], "tool");
        assert_eq!(wire[3]["tool_call_id"], "call_1");
    }
}
