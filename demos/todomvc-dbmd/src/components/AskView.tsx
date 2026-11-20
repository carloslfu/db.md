// SPDX-License-Identifier: Apache-2.0

import { useEffect, useRef, useState } from "react";
import * as api from "../apiClient";

// The sixth view: natural language over the same store. `dbmd api` runs the
// toolkit's embedded harness — a stateless tool-calling loop whose tools ARE
// the dbmd verbs the other five views mirror — against whatever model the
// server is pointed at (a local Ollama, a preset + key, a vendor CLI).
// This view renders the flat event feed exactly as the CLI does: each tool
// call as the dbmd one-liner it executed, then the streamed answer.

type ToolItem = {
  kind: "tool";
  display: string;
  status: "running" | "ok" | "error";
  note: string;
};
type AskItem =
  | { kind: "prompt"; text: string }
  | ToolItem
  | { kind: "answer"; text: string; done: boolean }
  | { kind: "notice"; text: string };

export function AskView() {
  const [prompt, setPrompt] = useState("");
  const [allowWrites, setAllowWrites] = useState(false);
  const [items, setItems] = useState<AskItem[]>([]);
  const [running, setRunning] = useState(false);
  const scroller = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    const el = scroller.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [items]);

  const submit = async () => {
    const text = prompt.trim();
    if (text === "" || running) return;
    setPrompt("");
    setRunning(true);
    setItems((previous) => [...previous, { kind: "prompt", text }]);

    const push = (item: AskItem) => setItems((previous) => [...previous, item]);
    const patchLast = (update: (item: AskItem) => AskItem) =>
      setItems((previous) => {
        if (previous.length === 0) return previous;
        const next = previous.slice();
        next[next.length - 1] = update(next[next.length - 1] as AskItem);
        return next;
      });

    try {
      await api.askStream(text, allowWrites, (event) => {
        switch (event.event) {
          case "tool_call":
            push({ kind: "tool", display: event.display, status: "running", note: "" });
            break;
          case "tool_result":
            patchLast((item) =>
              item.kind === "tool"
                ? {
                    ...item,
                    status: event.is_error ? "error" : "ok",
                    note: event.is_error
                      ? (event.content.split("\n")[0] ?? "error")
                      : `${event.content.length} bytes`,
                  }
                : item,
            );
            break;
          case "text_delta":
            setItems((previous) => {
              const last = previous[previous.length - 1];
              if (last?.kind === "answer" && !last.done) {
                const next = previous.slice();
                next[next.length - 1] = { ...last, text: last.text + event.text };
                return next;
              }
              return [...previous, { kind: "answer", text: event.text, done: false }];
            });
            break;
          case "done":
            setItems((previous) => {
              const next = previous.slice();
              const last = next[next.length - 1];
              if (last?.kind === "answer") {
                next[next.length - 1] = { kind: "answer", text: event.text, done: true };
                return next;
              }
              return [...next, { kind: "answer", text: event.text, done: true }];
            });
            break;
          // A downgrade the harness made on our behalf. Swallowing it is the
          // one thing the event exists to prevent, so it rides the same
          // notice line as an error.
          case "notice":
          case "error":
            push({ kind: "notice", text: event.message });
            break;
          default:
            break; // thinking_delta / usage / turn_end: quiet, like the CLI
        }
      });
    } catch (error) {
      push({
        kind: "notice",
        text:
          error instanceof api.ApiError && error.code === "ASK_DISABLED"
            ? "The ask route is off. Start the server with `dbmd api --ask --do --dir db` (npm run dev does)."
            : error instanceof api.ApiError && error.code === "ASK_CONFIG"
              ? `No model reachable: ${error.message}`
              : error instanceof Error
                ? error.message
                : "The run failed.",
      });
    } finally {
      setRunning(false);
    }
  };

  return (
    <div className="view-panel">
      <form
        className="ask-form"
        onSubmit={(event) => {
          event.preventDefault();
          void submit();
        }}
      >
        <input
          className="search-input"
          placeholder={allowWrites ? "Tell the store what to do…" : "Ask the store anything…"}
          autoFocus
          value={prompt}
          onChange={(event) => setPrompt(event.target.value)}
          disabled={running}
          aria-label="Natural-language request"
        />
        <label className="ask-writes">
          <input
            type="checkbox"
            checked={allowWrites}
            onChange={(event) => setAllowWrites(event.target.checked)}
          />
          allow changes
        </label>
      </form>
      {items.length === 0 ? (
        <div className="view-empty">
          Natural language over the same files: the toolkit's embedded harness runs YOUR model
          (a local Ollama is autodetected; presets + <code>DBMD_LLM_KEY</code> for the cloud)
          with the dbmd verbs as its only tools. Ask is read-only by construction; tick “allow
          changes” to let it write — every mutation rides the store contract and lands in the
          activity log.
        </div>
      ) : (
        <div className="ask-transcript" ref={scroller} aria-live="polite">
          {items.map((item, index) => {
            switch (item.kind) {
              case "prompt":
                return (
                  <div key={index} className="ask-prompt">
                    {item.text}
                  </div>
                );
              case "tool":
                return (
                  <div key={index} className={`ask-tool ${item.status}`}>
                    <code>→ {item.display}</code>
                    <span className="ask-tool-note">
                      {item.status === "running" ? "…" : item.status === "ok" ? `← ok (${item.note})` : `← ${item.note}`}
                    </span>
                  </div>
                );
              case "answer":
                return (
                  <div key={index} className={`ask-answer${item.done ? "" : " streaming"}`}>
                    {item.text}
                  </div>
                );
              case "notice":
                return (
                  <div key={index} className="ask-notice">
                    {item.text}
                  </div>
                );
              default:
                return null;
            }
          })}
          {running && <div className="ask-running">thinking…</div>}
        </div>
      )}
      <div className="view-hint">
        the same thing in a shell:{" "}
        <code>
          dbmd {allowWrites ? "do" : "ask"} '{prompt.trim() || "…"}'
        </code>
      </div>
    </div>
  );
}
