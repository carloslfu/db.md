// SPDX-License-Identifier: Apache-2.0

// bench/harness/adapters/dbmd.ts — the db.md lane.
// Ingestion is MECHANICAL (0 LLM tokens): each history unit becomes a verbatim source file in
// a fresh db.md store via the dbmd CLI. Answering is agentic: a tool loop over lexical
// search (dbmd search = embedded ripgrep; recall = the model's own query expansion — the
// no-vectors thesis), file reads, and a listing. See bench/PROTOCOL.md §10.
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import {
  callClaude,
  textOf,
  type ApiMessage,
  type ContentBlock,
  type SpendMeter,
  type ToolDef,
  type ToolUseBlock,
} from "../anthropic";
import type {
  AnswerResult,
  EvalItem,
  IngestResult,
  SystemAdapter,
  ToolTraceEntry,
} from "../types";

const DBMD_BIN = process.env.DBMD_BIN || join(homedir(), ".dbmd", "bin", "dbmd");
const SEARCH_CAP = 4000; // chars of search output returned to the model
const READ_CAP = 8000; // chars of file content returned to the model
const LIST_CAP = 3000;

const TOOLS: ToolDef[] = [
  {
    name: "search_store",
    description:
      "Lexical search over the memory store (ripgrep regex, case-insensitive). There are no embeddings: expand the query yourself with alternations — synonyms, word forms, names, numbers, dates — e.g. (adopt|adopted|rescued|got) or (dog|puppy|retriever). Returns matching lines as path:line:text.",
    input_schema: {
      type: "object",
      properties: { query: { type: "string", description: "ripgrep-style regex" } },
      required: ["query"],
      additionalProperties: false,
    },
  },
  {
    name: "read_file",
    description:
      "Read one file from the store by its store-relative path (as printed by search_store or list_store).",
    input_schema: {
      type: "object",
      properties: { path: { type: "string" } },
      required: ["path"],
      additionalProperties: false,
    },
  },
  {
    name: "list_store",
    description: "List every file in the store as a tree.",
    input_schema: { type: "object", properties: {}, additionalProperties: false },
  },
];

const ANSWER_SYSTEM = `You are a memory recall agent operating a db.md store: a plain-file database holding a user's past chat sessions with an assistant (or a set of documents). Answer the question using ONLY what the store contains.

Method:
1. Search first (search_store), read what matters (read_file), then answer. Multiple searches with different expansions are encouraged; multi-part questions usually need several searches.
2. The search is lexical (ripgrep). YOU are the query expansion: try synonyms, alternate word forms, names, numbers, and date formats, joined with | alternation.
3. Dates matter. Each session file starts with "Session date: ...". For questions about when/how long/what order, compare session dates and dates stated in the text.
4. When you have the answer, reply with a single short factual answer — no preamble, no explanation.
5. If the store does not contain the answer, reply exactly: I don't know.`;

function run(cmd: string[], cwd?: string): string {
  return execFileSync(DBMD_BIN, cmd, {
    encoding: "utf8",
    cwd,
    timeout: 30_000,
    maxBuffer: 16 * 1024 * 1024,
  });
}

function storeDirFor(workDir: string): string {
  return join(workDir, "store");
}

export class DbmdAdapter implements SystemAdapter {
  readonly name = "dbmd";

  constructor(
    private readonly opts: {
      apiKey: string;
      model: string;
      meter: SpendMeter;
      maxRounds: number;
    },
  ) {
    if (!existsSync(DBMD_BIN)) {
      throw new Error(`dbmd binary not found at ${DBMD_BIN} (set DBMD_BIN)`);
    }
  }

  async ingest(item: EvalItem, workDir: string): Promise<IngestResult> {
    const store = storeDirFor(workDir);
    rmSync(store, { recursive: true, force: true });
    mkdirSync(join(store, "sources"), { recursive: true });
    mkdirSync(join(store, "records"), { recursive: true });
    writeFileSync(
      join(store, "DB.md"),
      [
        "---",
        "type: db-md",
        "scope: research",
        "owner: bench-harness",
        "---",
        "",
        `# Memory store for eval item ${item.id}`,
        "",
        "Verbatim history units ingested by the benchmark harness (PROTOCOL §2.3).",
        "",
      ].join("\n"),
    );
    const bodyTmp = join(workDir, "ingest-body.tmp");
    let n = 0;
    for (const unit of item.units) {
      n++;
      writeFileSync(bodyTmp, unit.text);
      const isSession = unit.kind === "session";
      const summary = isSession
        ? `Chat session ${n} (${unit.date ?? "undated"})`
        : `Document: ${(unit.title ?? unit.id).slice(0, 80)}`;
      const args = [
        "write",
        `sources/${isSession ? "note" : "document"}/${unit.kind}-${String(n).padStart(3, "0")}.md`,
        "--type",
        isSession ? "note" : "document",
        "--summary",
        summary,
        "--body-file",
        bodyTmp,
        "--dir",
        store,
        "--json",
      ];
      if (unit.date) args.push("--fm", `session-date=${unit.date}`);
      run(args);
    }
    rmSync(bodyTmp, { force: true });
    return { units: n, ingestLlmTokens: 0, notes: "mechanical ingestion (no LLM)" };
  }

  async answer(item: EvalItem, workDir: string): Promise<AnswerResult> {
    const store = storeDirFor(workDir);
    const toolTrace: ToolTraceEntry[] = [];
    let toolCalls = 0;
    const meterBefore = {
      in: this.opts.meter.inputTokens,
      out: this.opts.meter.outputTokens,
      calls: this.opts.meter.apiCalls,
      usd: this.opts.meter.totalUsd,
    };

    const userPrompt = [
      item.questionDate ? `Question date: ${item.questionDate}` : null,
      `Question: ${item.question}`,
    ]
      .filter(Boolean)
      .join("\n");
    const messages: ApiMessage[] = [{ role: "user", content: userPrompt }];

    let hypothesis = "";
    for (let round = 0; round < this.opts.maxRounds; round++) {
      const res = await callClaude({
        apiKey: this.opts.apiKey,
        model: this.opts.model,
        system: ANSWER_SYSTEM,
        messages,
        tools: TOOLS,
        toolChoice: "auto",
        maxTokens: 1024,
        temperature: 0,
        meter: this.opts.meter,
      });
      messages.push({ role: "assistant", content: res.content });
      if (res.stopReason !== "tool_use") {
        hypothesis = textOf(res.content);
        break;
      }
      const toolUses = res.content.filter((b): b is ToolUseBlock => b.type === "tool_use");
      const results: ContentBlock[] = [];
      for (const tu of toolUses) {
        toolCalls++;
        const { output, isError } = this.runTool(tu, store);
        toolTrace.push({
          tool: tu.name,
          input: tu.input,
          outputPreview: output.slice(0, 500),
        });
        results.push({
          type: "tool_result",
          tool_use_id: tu.id,
          content: output,
          ...(isError ? { is_error: true } : {}),
        });
      }
      messages.push({ role: "user", content: results });
    }

    if (!hypothesis) {
      // Tool budget exhausted: force a final answer with no tools.
      messages.push({
        role: "user",
        content:
          "Tool budget exhausted. Based on what you found, give your final short answer now — or exactly: I don't know.",
      });
      const res = await callClaude({
        apiKey: this.opts.apiKey,
        model: this.opts.model,
        system: ANSWER_SYSTEM,
        messages,
        maxTokens: 256,
        temperature: 0,
        meter: this.opts.meter,
      });
      messages.push({ role: "assistant", content: res.content });
      hypothesis = textOf(res.content);
    }

    return {
      hypothesis,
      usage: {
        inputTokens: this.opts.meter.inputTokens - meterBefore.in,
        outputTokens: this.opts.meter.outputTokens - meterBefore.out,
        apiCalls: this.opts.meter.apiCalls - meterBefore.calls,
        toolCalls,
        costUsd: this.opts.meter.totalUsd - meterBefore.usd,
      },
      toolTrace,
      messages: messages as unknown[],
    };
  }

  private runTool(tu: ToolUseBlock, store: string): { output: string; isError: boolean } {
    const input = (tu.input ?? {}) as Record<string, unknown>;
    try {
      if (tu.name === "search_store") {
        const query = String(input.query ?? "");
        if (!query) return { output: "error: empty query", isError: true };
        let out = "";
        try {
          out = run(["search", query, "--dir", store]);
        } catch (e) {
          const msg = e instanceof Error ? e.message : String(e);
          return { output: `search error (check regex syntax): ${msg.slice(0, 300)}`, isError: true };
        }
        if (!out.trim()) return { output: "(no matches — try a broader alternation)", isError: false };
        return { output: cap(out, SEARCH_CAP), isError: false };
      }
      if (tu.name === "read_file") {
        const rel = String(input.path ?? "");
        const abs = resolve(store, rel);
        if (!abs.startsWith(resolve(store))) {
          return { output: "error: path escapes the store", isError: true };
        }
        if (!existsSync(abs)) return { output: `error: no such file: ${rel}`, isError: true };
        return { output: cap(readFileSync(abs, "utf8"), READ_CAP), isError: false };
      }
      if (tu.name === "list_store") {
        return { output: cap(run(["tree", "--dir", store]), LIST_CAP), isError: false };
      }
      return { output: `error: unknown tool ${tu.name}`, isError: true };
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      return { output: `tool error: ${msg.slice(0, 300)}`, isError: true };
    }
  }
}

function cap(s: string, max: number): string {
  if (s.length <= max) return s;
  return `${s.slice(0, max)}\n…(truncated ${s.length - max} chars)`;
}
