// SPDX-License-Identifier: Apache-2.0

//! The ChatGPT (Codex) backend adapter — the OpenAI **Responses** wire format
//! against `https://chatgpt.com/backend-api/codex/responses`, driven by a
//! subscription token from [`super::oauth`]. Ported from pi's
//! `openai-codex-responses` provider.
//!
//! This is a third wire protocol, deliberately scoped: it exists only because
//! the ChatGPT backend speaks Responses rather than Chat Completions, and it
//! is reached only by an explicit `dbmd login codex`. The two neutral
//! protocols ([`super::openai`], [`super::anthropic`]) remain the general
//! path for API keys and local servers.
//!
//! Request shape (pi's `buildRequestBody`): `{model, store: false, stream:
//! true, instructions: <system prompt>, input: [...], tools: [...],
//! tool_choice: "auto", parallel_tool_calls: true, include:
//! ["reasoning.encrypted_content"], text: {verbosity}}`. The system prompt
//! rides as `instructions`, never as an input message.
//!
//! Headers: `Authorization: Bearer <access>`, `chatgpt-account-id` from the
//! token's JWT claim, `OpenAI-Beta: responses=experimental`, a `session_id`,
//! and `originator: dbmd` — this toolkit identifies itself honestly and never
//! poses as another vendor's client.
//!
//! Stream events consumed (`response.*`): `output_item.added` opens a
//! reasoning / message / function_call item, `output_text.delta` and
//! `reasoning_summary_text.delta` carry text, `function_call_arguments.delta`
//! accumulates tool arguments, `output_item.done` finalizes each item, and
//! `response.completed` carries usage. Unknown event types are ignored by
//! design, exactly as with the other adapters.

use serde_json::{json, Value};

use super::openai::streaming_agent;
use super::sse::read_events;
use super::tools::ToolSpec;
use super::{Block, Event, HarnessError, Msg, Provider, Role, RunOptions, Stop, Turn};

/// The ChatGPT backend base (overridable for tests via the provider's
/// `base_url`).
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// Resolve the responses endpoint from a base URL, tolerating a base that
/// already names `/codex` or `/codex/responses` (pi's `resolveCodexUrl`).
pub fn responses_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() {
        DEFAULT_BASE_URL
    } else {
        base_url
    };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

/// Build the `input` array: user text, assistant text, tool calls, and tool
/// results in the Responses item vocabulary.
fn wire_input(messages: &[Msg]) -> Vec<Value> {
    let mut input: Vec<Value> = Vec::new();
    for msg in messages {
        match msg.role {
            Role::User => {
                for block in &msg.blocks {
                    match block {
                        Block::Text(text) => input.push(json!({
                            "type": "message",
                            "role": "user",
                            "content": [{ "type": "input_text", "text": text }],
                        })),
                        Block::ToolResult {
                            id,
                            content,
                            is_error,
                            ..
                        } => {
                            // The engine's call id is the Responses `call_id`.
                            let output = if *is_error {
                                format!("ERROR: {content}")
                            } else {
                                content.clone()
                            };
                            input.push(json!({
                                "type": "function_call_output",
                                "call_id": id,
                                "output": output,
                            }));
                        }
                        _ => {}
                    }
                }
            }
            Role::Assistant => {
                for block in &msg.blocks {
                    match block {
                        Block::Text(text) if !text.is_empty() => input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                        })),
                        Block::ToolUse {
                            id,
                            name,
                            args,
                            raw_args,
                        } => input.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": raw_args.clone().unwrap_or_else(|| args.to_string()),
                        })),
                        // Reasoning items are not replayed: their encrypted
                        // content is bound to the response that produced it.
                        _ => {}
                    }
                }
            }
        }
    }
    input
}

fn wire_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": false,
            })
        })
        .collect()
}

/// One in-flight output item, keyed by its stream `output_index`.
enum Pending {
    Text(String),
    Thinking(String),
    ToolCall {
        call_id: String,
        name: String,
        args_buf: String,
    },
    Other,
}

/// Stream one assistant turn from the ChatGPT backend.
pub fn stream_turn(
    provider: &Provider,
    opts: &RunOptions,
    system: &str,
    messages: &[Msg],
    tools: &[ToolSpec],
    emit: &mut dyn FnMut(Event),
) -> Result<Turn, HarnessError> {
    let access = provider.key.as_deref().ok_or_else(|| {
        HarnessError::Config(
            "no ChatGPT credentials — run `dbmd login codex` to use your subscription".to_string(),
        )
    })?;
    let account = super::oauth::account_id(access).ok_or_else(|| {
        HarnessError::Config(
            "the stored ChatGPT token carries no account id — run `dbmd login codex` again"
                .to_string(),
        )
    })?;
    let session_id = super::oauth::create_state()?;

    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(provider.model));
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    body.insert("instructions".into(), json!(system));
    body.insert("input".into(), json!(wire_input(messages)));
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    body.insert("text".into(), json!({ "verbosity": "low" }));
    body.insert("tool_choice".into(), json!("auto"));
    body.insert("parallel_tool_calls".into(), json!(true));
    body.insert("prompt_cache_key".into(), json!(session_id));
    if !tools.is_empty() {
        body.insert("tools".into(), json!(wire_tools(tools)));
    }
    // `max_tokens` has no Responses equivalent the backend accepts here; the
    // engine's turn cap and the account's own limits bound a run instead.
    let _ = opts.max_tokens;
    // Reasoning depth. `summary: "auto"` is what makes the backend stream
    // reasoning summaries at all — without it the thinking deltas this
    // adapter already parses simply never arrive.
    if let Some(effort) = opts.effort {
        body.insert(
            "reasoning".into(),
            json!({ "effort": effort.codex(), "summary": "auto" }),
        );
    }

    let url = responses_url(&provider.base_url);
    let agent = streaming_agent();
    let request = agent
        .post(&url)
        .set("authorization", &format!("Bearer {access}"))
        .set("chatgpt-account-id", &account)
        .set("originator", super::oauth::ORIGINATOR)
        .set("openai-beta", "responses=experimental")
        .set("session_id", &session_id)
        .set("x-client-request-id", &session_id)
        .set("accept", "text/event-stream")
        .set("content-type", "application/json")
        .set(
            "user-agent",
            &format!(
                "dbmd/{} ({} {})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        );
    let response = match request.send_string(&Value::Object(body).to_string()) {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let mut detail = response.into_string().unwrap_or_default();
            detail.truncate(600);
            let hint = match status {
                400 if detail.contains("reasoning") || detail.contains("effort") => {
                    " — this model or plan does not accept that reasoning \
                     effort; try `--effort high` or drop `--effort`"
                }
                400 if detail.contains("not supported") => {
                    " — pick one your plan exposes with `--model` (or \
                     `llm_model_codex` in .dbmd/config); `codex --help` and \
                     ~/.codex/config.toml name what this account uses"
                }
                401 => " — the login may have expired; run `dbmd login codex` again",
                403 => " — this ChatGPT account may not include Codex access",
                429 => " — the account's rate or usage limit was reached",
                _ => "",
            };
            return Err(HarnessError::Provider(format!(
                "ChatGPT backend returned HTTP {status}{hint}: {detail}"
            )));
        }
        Err(error) => {
            return Err(HarnessError::Provider(format!(
                "cannot reach the ChatGPT backend: {error}"
            )))
        }
    };

    let mut open: Vec<(u64, Pending)> = Vec::new();
    let mut blocks: Vec<Block> = Vec::new();
    let mut usage: Option<(u64, u64)> = None;
    let mut stream_error: Option<String> = None;
    let mut incomplete = false;

    read_events(response.into_reader(), |event| {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            return true;
        };
        let kind = data
            .get("type")
            .and_then(|t| t.as_str())
            .or(event.event.as_deref())
            .unwrap_or("");
        let index = data
            .get("output_index")
            .and_then(|i| i.as_u64())
            .unwrap_or(0);
        match kind {
            "response.output_item.added" => {
                let item = data.get("item").unwrap_or(&Value::Null);
                let pending = match item.get("type").and_then(|t| t.as_str()) {
                    Some("reasoning") => Pending::Thinking(String::new()),
                    Some("message") => Pending::Text(String::new()),
                    Some("function_call") => Pending::ToolCall {
                        call_id: item
                            .get("call_id")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: item
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string(),
                        args_buf: String::new(),
                    },
                    _ => Pending::Other,
                };
                open.push((index, pending));
            }
            "response.output_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta" => {
                let Some(fragment) = data.get("delta").and_then(|d| d.as_str()) else {
                    return true;
                };
                if fragment.is_empty() {
                    return true;
                }
                match open.iter_mut().rev().find(|(i, _)| *i == index) {
                    Some((_, Pending::Text(buffer))) => {
                        buffer.push_str(fragment);
                        emit(Event::TextDelta {
                            text: fragment.to_string(),
                        });
                    }
                    Some((_, Pending::Thinking(buffer))) => {
                        buffer.push_str(fragment);
                        emit(Event::ThinkingDelta {
                            text: fragment.to_string(),
                        });
                    }
                    _ => {}
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(fragment) = data.get("delta").and_then(|d| d.as_str()) {
                    if let Some((_, Pending::ToolCall { args_buf, .. })) =
                        open.iter_mut().rev().find(|(i, _)| *i == index)
                    {
                        args_buf.push_str(fragment);
                    }
                }
            }
            "response.function_call_arguments.done" => {
                // The terminal event carries the COMPLETE argument string.
                if let Some(complete) = data.get("arguments").and_then(|a| a.as_str()) {
                    if let Some((_, Pending::ToolCall { args_buf, .. })) =
                        open.iter_mut().rev().find(|(i, _)| *i == index)
                    {
                        *args_buf = complete.to_string();
                    }
                }
            }
            "response.output_item.done" => {
                let item = data.get("item").unwrap_or(&Value::Null);
                let position = open.iter().rposition(|(i, _)| *i == index);
                let pending = position.map(|position| open.remove(position).1);
                match pending {
                    Some(Pending::Text(buffer)) => {
                        // The done item is authoritative over the deltas.
                        let text = item
                            .get("content")
                            .and_then(|c| c.as_array())
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|part| {
                                        part.get("text")
                                            .or_else(|| part.get("refusal"))
                                            .and_then(|t| t.as_str())
                                    })
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .filter(|text| !text.is_empty())
                            .unwrap_or(buffer);
                        if !text.is_empty() {
                            blocks.push(Block::Text(text));
                        }
                    }
                    Some(Pending::Thinking(buffer)) => {
                        let summary = item
                            .get("summary")
                            .and_then(|s| s.as_array())
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .filter(|text| !text.is_empty())
                            .unwrap_or(buffer);
                        if !summary.is_empty() {
                            blocks.push(Block::Thinking {
                                text: summary,
                                signature: None,
                            });
                        }
                    }
                    Some(Pending::ToolCall {
                        call_id,
                        name,
                        args_buf,
                    }) => {
                        let call_id = if call_id.is_empty() {
                            item.get("call_id")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string()
                        } else {
                            call_id
                        };
                        let name = if name.is_empty() {
                            item.get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string()
                        } else {
                            name
                        };
                        let raw = if args_buf.trim().is_empty() {
                            item.get("arguments")
                                .and_then(|a| a.as_str())
                                .unwrap_or("")
                                .to_string()
                        } else {
                            args_buf
                        };
                        if !name.is_empty() {
                            let args = if raw.trim().is_empty() {
                                json!({})
                            } else {
                                serde_json::from_str(&raw).unwrap_or(json!({}))
                            };
                            blocks.push(Block::ToolUse {
                                id: call_id,
                                name,
                                args,
                                raw_args: if raw.trim().is_empty() {
                                    None
                                } else {
                                    Some(raw)
                                },
                            });
                        }
                    }
                    Some(Pending::Other) | None => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = data.get("response").unwrap_or(&Value::Null);
                let input = response
                    .pointer("/usage/input_tokens")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                let output = response
                    .pointer("/usage/output_tokens")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                if input > 0 || output > 0 {
                    usage = Some((input, output));
                }
                if kind == "response.incomplete" {
                    incomplete = true;
                }
                return false;
            }
            "response.failed" | "error" => {
                let message = data
                    .pointer("/response/error/message")
                    .or_else(|| data.pointer("/error/message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("the ChatGPT backend reported a stream error");
                stream_error = Some(message.to_string());
                return false;
            }
            _ => {}
        }
        true
    })
    .map_err(|error| HarnessError::Provider(format!("stream read failed: {error}")))?;

    if let Some(message) = stream_error {
        return Err(HarnessError::Provider(format!(
            "ChatGPT backend error: {message}"
        )));
    }
    if blocks.is_empty() {
        return Err(HarnessError::Provider(
            "the ChatGPT backend returned no content".to_string(),
        ));
    }

    let has_calls = blocks.iter().any(|b| matches!(b, Block::ToolUse { .. }));
    let stop = if has_calls {
        Stop::ToolUse
    } else if incomplete {
        Stop::Length
    } else {
        Stop::EndTurn
    };
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
    fn endpoint_resolution_tolerates_every_base_shape() {
        assert_eq!(
            responses_url(""),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            responses_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            responses_url("http://127.0.0.1:9/codex"),
            "http://127.0.0.1:9/codex/responses"
        );
        assert_eq!(
            responses_url("http://127.0.0.1:9/codex/responses/"),
            "http://127.0.0.1:9/codex/responses"
        );
    }

    #[test]
    fn tool_results_ride_as_function_call_output() {
        let input = wire_input(&[
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
        ]);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
    }

    #[test]
    fn error_results_are_marked_for_the_model() {
        let input = wire_input(&[Msg {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                id: "c".into(),
                name: "write".into(),
                content: "unknown tool".into(),
                is_error: true,
            }],
        }]);
        assert!(input[0]["output"]
            .as_str()
            .expect("output")
            .starts_with("ERROR:"));
    }
}
