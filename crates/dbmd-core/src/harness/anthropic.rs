// SPDX-License-Identifier: Apache-2.0

//! The Anthropic Messages adapter — hand-rolled request builder + streaming
//! parser over `ureq`, against the documented event grammar:
//!
//! `message_start` → per block `content_block_start` /
//! `content_block_delta` (`text_delta` | `thinking_delta` |
//! `input_json_delta` | `signature_delta`) / `content_block_stop` →
//! `message_delta` (stop_reason + cumulative output usage) → `message_stop`,
//! with `ping` anywhere, in-stream `error` events on HTTP 200 (handled as a
//! turn failure, not a parse failure), and unknown event types skipped by
//! versioning policy.
//!
//! Contract details the loop depends on: tool results for a turn ride in ONE
//! user message ([`super::run`] builds messages that way); assistant content
//! is appended back verbatim — thinking blocks with their signatures intact —
//! so the next turn round-trips; `max_tokens` is required; the system prompt
//! is a top-level parameter. `temperature` is never sent (the newest models
//! reject sampling params). Also reaches llama.cpp's native `/v1/messages`.

use serde_json::{json, Value};

use super::openai::{send_with_variants, streaming_agent, Variant};
use super::sse::read_events;
use super::tools::ToolSpec;
use super::{Block, Event, HarnessError, Msg, Provider, Role, RunOptions, Stop, Turn};

/// Build the wire `messages` array (system rides separately).
fn wire_messages(messages: &[Msg]) -> Vec<Value> {
    let mut wire: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        let mut content: Vec<Value> = Vec::new();
        for block in &msg.blocks {
            match (msg.role, block) {
                (_, Block::Text(text)) => {
                    content.push(json!({ "type": "text", "text": text }));
                }
                (
                    Role::User,
                    Block::ToolResult {
                        id,
                        content: result,
                        is_error,
                        ..
                    },
                ) => {
                    content.push(json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": result,
                        "is_error": is_error,
                    }));
                }
                (Role::Assistant, Block::ToolUse { id, name, args, .. }) => {
                    content.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": args,
                    }));
                }
                // A thinking block without its signature cannot be resent.
                (
                    Role::Assistant,
                    Block::Thinking {
                        text,
                        signature: Some(signature),
                    },
                ) => {
                    content.push(json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": signature,
                    }));
                }
                _ => {}
            }
        }
        if content.is_empty() {
            continue;
        }
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        wire.push(json!({ "role": role, "content": content }));
    }
    wire
}

/// One in-flight content block being accumulated by stream index.
enum PendingBlock {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    ToolUse {
        id: String,
        name: String,
        json_buf: String,
    },
    Other,
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
    let url = format!("{}/v1/messages", provider.base_url.trim_end_matches('/'));
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(provider.model));
    body.insert("max_tokens".into(), json!(opts.max_tokens));
    body.insert("stream".into(), json!(true));
    body.insert("system".into(), json!(system));
    body.insert("messages".into(), json!(wire_messages(messages)));
    if !tools.is_empty() {
        let tools: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        body.insert("tools".into(), json!(tools));
    }

    let mut headers: Vec<(&str, String)> = vec![
        ("anthropic-version", "2023-06-01".to_string()),
        ("accept", "text/event-stream".to_string()),
    ];
    if let Some(key) = &provider.key {
        if provider.oauth {
            // An OAuth token goes on Authorization: Bearer, and /v1/messages
            // rejects it without this beta header. Never both headers at once
            // — the API refuses a request carrying an API key AND a token.
            headers.push(("authorization", format!("Bearer {key}")));
            headers.push(("anthropic-beta", super::ant::OAUTH_BETA.to_string()));
        } else {
            headers.push(("x-api-key", key.clone()));
        }
    }

    // Reasoning depth, in three descending shapes.
    //
    // Current models take `output_config.effort` alongside adaptive thinking;
    // `budget_tokens` is *rejected with a 400* on them. Models older than 4.6
    // are the mirror image: no `output_config`, thinking only via
    // `budget_tokens`. Rather than pin a model list that goes stale with every
    // release — the thing this toolkit refuses to do everywhere else — the
    // request degrades on the wire: modern shape, then legacy shape, then no
    // reasoning at all. `--effort off` disables thinking outright instead.
    let mut variants = Vec::new();
    if let Some(effort) = opts.effort {
        match effort.anthropic() {
            Some(level) => {
                let mut modern = body.clone();
                modern.insert(
                    "thinking".into(),
                    json!({ "type": "adaptive", "display": "summarized" }),
                );
                modern.insert("output_config".into(), json!({ "effort": level }));
                variants.push(Variant {
                    body: Value::Object(modern).to_string(),
                    label: format!("output_config.effort={level}"),
                });
                if let Some(budget) = effort.anthropic_budget() {
                    let mut legacy = body.clone();
                    legacy.insert(
                        "thinking".into(),
                        json!({ "type": "enabled", "budget_tokens": budget }),
                    );
                    // A thinking budget must leave room for an answer, so the
                    // ceiling rises with it rather than starving the response.
                    legacy.insert(
                        "max_tokens".into(),
                        json!(opts.max_tokens.saturating_add(budget)),
                    );
                    variants.push(Variant {
                        body: Value::Object(legacy).to_string(),
                        label: format!("thinking.budget_tokens={budget}"),
                    });
                }
            }
            None => {
                let mut disabled = body.clone();
                disabled.insert("thinking".into(), json!({ "type": "disabled" }));
                variants.push(Variant {
                    body: Value::Object(disabled).to_string(),
                    label: "thinking disabled".to_string(),
                });
            }
        }
    }
    variants.push(Variant {
        body: Value::Object(body).to_string(),
        label: "no reasoning parameter".to_string(),
    });

    let agent = streaming_agent();
    let response = send_with_variants(&agent, &url, &headers, &variants, emit)?;

    let mut open: Vec<(u64, PendingBlock)> = Vec::new();
    let mut blocks: Vec<Block> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;
    let mut stream_error: Option<String> = None;
    let mut synthesized = 0usize;

    let reader = response.into_reader();
    read_events(reader, |event| {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            return true;
        };
        let kind = event
            .event
            .as_deref()
            .or_else(|| data.get("type").and_then(|t| t.as_str()))
            .unwrap_or("");
        match kind {
            "message_start" => {
                input_tokens = data
                    .pointer("/message/usage/input_tokens")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
            }
            "content_block_start" => {
                let index = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let block = data.get("content_block").unwrap_or(&Value::Null);
                let pending = match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => PendingBlock::Text(String::new()),
                    Some("thinking") => PendingBlock::Thinking {
                        text: String::new(),
                        signature: String::new(),
                    },
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string();
                        let id = if id.is_empty() {
                            synthesized += 1;
                            format!("toolu_local_{synthesized}")
                        } else {
                            id
                        };
                        PendingBlock::ToolUse {
                            id,
                            name: block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string(),
                            json_buf: String::new(),
                        }
                    }
                    // redacted_thinking, future block kinds: opaque.
                    _ => PendingBlock::Other,
                };
                open.push((index, pending));
            }
            "content_block_delta" => {
                let index = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let delta = data.get("delta").unwrap_or(&Value::Null);
                let slot = open.iter_mut().rev().find(|(i, _)| *i == index);
                let Some((_, pending)) = slot else {
                    return true;
                };
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        if let Some(fragment) = delta.get("text").and_then(|t| t.as_str()) {
                            if let PendingBlock::Text(buffer) = pending {
                                buffer.push_str(fragment);
                            }
                            emit(Event::TextDelta {
                                text: fragment.to_string(),
                            });
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(fragment) = delta.get("thinking").and_then(|t| t.as_str()) {
                            if let PendingBlock::Thinking { text, .. } = pending {
                                text.push_str(fragment);
                            }
                            emit(Event::ThinkingDelta {
                                text: fragment.to_string(),
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(fragment) = delta.get("partial_json").and_then(|p| p.as_str()) {
                            if let PendingBlock::ToolUse { json_buf, .. } = pending {
                                json_buf.push_str(fragment);
                            }
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(fragment) = delta.get("signature").and_then(|s| s.as_str()) {
                            if let PendingBlock::Thinking { signature, .. } = pending {
                                signature.push_str(fragment);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = data.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                if let Some(position) = open.iter().rposition(|(i, _)| *i == index) {
                    let (_, pending) = open.remove(position);
                    match pending {
                        PendingBlock::Text(text) => {
                            if !text.is_empty() {
                                blocks.push(Block::Text(text));
                            }
                        }
                        PendingBlock::Thinking { text, signature } => {
                            blocks.push(Block::Thinking {
                                text,
                                signature: if signature.is_empty() {
                                    None
                                } else {
                                    Some(signature)
                                },
                            });
                        }
                        PendingBlock::ToolUse { id, name, json_buf } => {
                            let raw = json_buf.trim().to_string();
                            let args = if raw.is_empty() {
                                json!({})
                            } else {
                                serde_json::from_str(&raw).unwrap_or(json!({}))
                            };
                            blocks.push(Block::ToolUse {
                                id,
                                name,
                                args,
                                raw_args: if raw.is_empty() { None } else { Some(raw) },
                            });
                        }
                        PendingBlock::Other => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(reason) = data.pointer("/delta/stop_reason").and_then(|r| r.as_str()) {
                    stop_reason = Some(reason.to_string());
                }
                if let Some(tokens) = data
                    .pointer("/usage/output_tokens")
                    .and_then(|t| t.as_u64())
                {
                    output_tokens = tokens;
                }
            }
            "message_stop" => return false,
            "error" => {
                let message = data
                    .pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("provider stream error");
                stream_error = Some(message.to_string());
                return false;
            }
            // ping, and any future event type: skipped by versioning policy.
            _ => {}
        }
        true
    })
    .map_err(|error| HarnessError::Provider(format!("stream read failed: {error}")))?;

    if let Some(message) = stream_error {
        return Err(HarnessError::Provider(format!(
            "provider stream error: {message}"
        )));
    }
    // Blocks the stream never closed (torn stream): flush what accumulated.
    for (_, pending) in open {
        match pending {
            PendingBlock::Text(text) if !text.is_empty() => blocks.push(Block::Text(text)),
            PendingBlock::ToolUse { id, name, json_buf } if !name.is_empty() => {
                let raw = json_buf.trim().to_string();
                let args = serde_json::from_str(&raw).unwrap_or(json!({}));
                blocks.push(Block::ToolUse {
                    id,
                    name,
                    args,
                    raw_args: if raw.is_empty() { None } else { Some(raw) },
                });
            }
            _ => {}
        }
    }

    if blocks.is_empty() && stop_reason.is_none() {
        return Err(HarnessError::Provider(
            "the stream ended without content or a stop reason".to_string(),
        ));
    }

    let has_calls = blocks.iter().any(|b| matches!(b, Block::ToolUse { .. }));
    let stop = if has_calls {
        Stop::ToolUse
    } else {
        match stop_reason.as_deref() {
            Some("max_tokens") => Stop::Length,
            _ => Stop::EndTurn,
        }
    };

    Ok(Turn {
        blocks,
        stop,
        usage: Some((input_tokens, output_tokens)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_ride_one_user_message() {
        let wire = wire_messages(&[Msg {
            role: Role::User,
            blocks: vec![
                Block::ToolResult {
                    id: "a".into(),
                    name: "query".into(),
                    content: "[]".into(),
                    is_error: false,
                },
                Block::ToolResult {
                    id: "b".into(),
                    name: "show".into(),
                    content: "x".into(),
                    is_error: true,
                },
            ],
        }]);
        assert_eq!(wire.len(), 1);
        let content = wire[0]["content"].as_array().expect("blocks");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[1]["is_error"], true);
    }

    #[test]
    fn unsigned_thinking_is_not_resent() {
        let wire = wire_messages(&[Msg {
            role: Role::Assistant,
            blocks: vec![
                Block::Thinking {
                    text: "hmm".into(),
                    signature: None,
                },
                Block::Text("answer".into()),
            ],
        }]);
        let content = wire[0]["content"].as_array().expect("blocks");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }
}
