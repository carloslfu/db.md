// SPDX-License-Identifier: Apache-2.0

//! A minimal, tolerant Server-Sent-Events *client* parser.
//!
//! Nothing else in the tree consumes SSE (the toolkit's servers only produce
//! it), so this is written fresh — against the full grammar, because real
//! providers exercise all of it: comment keep-alives (`: OPENROUTER
//! PROCESSING`), `id:`/`retry:` fields, CRLF line endings, multi-line `data:`
//! events, named `event:` types (Anthropic), and anonymous `data:` frames
//! ending in `data: [DONE]` (OpenAI-compat). Feeding a comment line to a JSON
//! parser is a documented crash class in official SDKs; this parser skips
//! everything it does not understand.

use std::io::{BufRead, BufReader, Read};

/// One dispatched SSE event: the optional `event:` name and the joined
/// `data:` payload (multiple `data:` lines joined with `\n`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, when the stream names its events.
    pub event: Option<String>,
    /// The concatenated `data:` payload.
    pub data: String,
}

/// Total bytes any single stream may deliver before the parser refuses —
/// a runaway-provider guard, far above any real completion.
const MAX_STREAM_BYTES: u64 = 64 * 1024 * 1024;

/// Read `reader` to end-of-stream, invoking `on_event` per dispatched event.
/// `on_event` returns `false` to stop consuming (the caller saw a terminal
/// event). I/O errors after at least one event are swallowed as end-of-stream
/// (providers routinely drop the socket after the final frame); an error
/// before any event is returned.
pub fn read_events<R: Read>(
    reader: R,
    mut on_event: impl FnMut(SseEvent) -> bool,
) -> std::io::Result<()> {
    let mut lines = BufReader::new(reader.take(MAX_STREAM_BYTES)).lines();
    let mut event_name: Option<String> = None;
    let mut data: Vec<String> = Vec::new();
    let mut dispatched_any = false;
    loop {
        let line = match lines.next() {
            None => break,
            Some(Ok(line)) => line,
            Some(Err(error)) => {
                if dispatched_any {
                    break;
                }
                return Err(error);
            }
        };
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if line.is_empty() {
            if !data.is_empty() {
                let event = SseEvent {
                    event: event_name.take(),
                    data: data.join("\n"),
                };
                data.clear();
                dispatched_any = true;
                if !on_event(event) {
                    return Ok(());
                }
            } else {
                event_name = None;
            }
            continue;
        }
        if line.starts_with(':') {
            continue; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => event_name = Some(value.to_string()),
            "data" => data.push(value.to_string()),
            // `id`, `retry`, and any future field: ignored by design.
            _ => {}
        }
    }
    // A final event unterminated by a blank line still counts.
    if !data.is_empty() {
        on_event(SseEvent {
            event: event_name,
            data: data.join("\n"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(input: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();
        read_events(input.as_bytes(), |event| {
            events.push(event);
            true
        })
        .expect("parse");
        events
    }

    #[test]
    fn anonymous_data_frames() {
        let events = collect("data: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].data, "[DONE]");
        assert!(events[0].event.is_none());
    }

    #[test]
    fn named_events_and_crlf() {
        let events = collect("event: message_start\r\ndata: {}\r\n\r\n");
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{}");
    }

    #[test]
    fn comments_ids_and_retries_are_skipped() {
        let events = collect(": OPENROUTER PROCESSING\n\nid: 7\nretry: 100\ndata: x\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn multiline_data_joins_with_newline() {
        let events = collect("data: a\ndata: b\n\n");
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn unterminated_final_event_dispatches() {
        let events = collect("data: tail");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "tail");
    }

    #[test]
    fn early_stop_is_honored() {
        let mut seen = 0;
        read_events("data: 1\n\ndata: 2\n\n".as_bytes(), |_| {
            seen += 1;
            false
        })
        .expect("parse");
        assert_eq!(seen, 1);
    }
}
