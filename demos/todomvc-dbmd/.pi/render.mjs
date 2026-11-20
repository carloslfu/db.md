#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Renders pi's `--mode json` event stream as a readable transcript:
// thinking (dim), tool calls with their commands (cyan), tool output
// (indented, truncated), and the assistant's prose as it streams.
import { createInterface } from "node:readline";

const out = process.stdout;
const ansi = (code) => (s) => `\x1b[${code}m${s}\x1b[0m`;
const gray = ansi(90);
const cyan = ansi(36);
const red = ansi(31);
const bold = ansi(1);

const MAX_RESULT_LINES = 30;
const MAX_RESULT_CHARS = 4000;

// Tracks which kind of stream is currently being written so labels and
// separators appear only on transitions.
let mode = null;

function transition(next, header) {
  if (mode === next) return;
  if (mode !== null) out.write("\n\n");
  if (header) out.write(header + "\n");
  mode = next;
}

function describeCall(toolName, args) {
  if (toolName === "bash") return `$ ${args?.command ?? ""}`;
  const target = args?.path ?? args?.file_path ?? args?.pattern ?? args?.url ?? "";
  return `${toolName} ${typeof target === "string" ? target : JSON.stringify(target)}`.trim();
}

function resultText(result) {
  if (result == null) return "";
  if (typeof result === "string") return result;
  if (typeof result.output === "string") return result.output;
  if (Array.isArray(result.content))
    return result.content
      .map((block) => (typeof block === "string" ? block : (block?.text ?? "")))
      .join("");
  if (typeof result.text === "string") return result.text;
  try {
    return JSON.stringify(result).slice(0, 500);
  } catch {
    return "";
  }
}

function printResult(text, isError) {
  let body = text.trimEnd();
  if (!body) return;
  if (body.length > MAX_RESULT_CHARS) body = body.slice(0, MAX_RESULT_CHARS);
  let lines = body.split("\n");
  const dropped = lines.length - MAX_RESULT_LINES;
  if (dropped > 0) {
    lines = lines.slice(0, MAX_RESULT_LINES);
    lines.push(`… (+${dropped} more lines)`);
  }
  const paint = isError ? red : gray;
  out.write(paint(lines.map((line) => `  ${line}`).join("\n")) + "\n");
}

function handle(event) {
  switch (event.type) {
    case "message_update": {
      const delta = event.assistantMessageEvent;
      if (!delta) return;
      if (delta.type === "thinking_delta") {
        transition("thinking", gray("● thinking"));
        out.write(gray(delta.delta ?? ""));
      } else if (delta.type === "text_delta") {
        transition("text", bold("● response"));
        out.write(delta.delta ?? "");
      }
      return;
    }
    case "tool_execution_start":
      transition(null, null);
      mode = "tool-block";
      out.write(cyan(describeCall(event.toolName, event.args)) + "\n");
      mode = null;
      return;
    case "tool_execution_end":
      printResult(resultText(event.result), event.isError === true);
      return;
    case "auto_retry_start":
      transition(null, null);
      out.write(red(`retrying (${event.attempt}/${event.maxAttempts}): ${event.errorMessage}`) + "\n");
      return;
    case "agent_end":
      transition(null, null);
      return;
    default:
      return;
  }
}

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on("line", (line) => {
  if (!line.trim()) return;
  let event;
  try {
    event = JSON.parse(line);
  } catch {
    out.write(gray(line) + "\n");
    return;
  }
  try {
    handle(event);
  } catch {
    // never let a rendering hiccup kill the stream
  }
});
rl.on("close", () => {
  if (mode !== null) out.write("\n");
});
