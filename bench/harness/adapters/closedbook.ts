// SPDX-License-Identifier: Apache-2.0

// bench/harness/adapters/closedbook.ts — the contamination-control lane (PROTOCOL §6.1).
// No store, no history: the model answers from parametric knowledge alone. Any item it gets
// right here is parametric-answerable for this model and is excluded from the
// contamination-adjusted headline.
import { callClaude, textOf, type SpendMeter } from "../anthropic";
import type { AnswerResult, EvalItem, IngestResult, SystemAdapter } from "../types";

const SYSTEM = `Answer the question from your own knowledge in a single short factual answer (no preamble). If you do not know, reply exactly: I don't know.`;

export class ClosedBookAdapter implements SystemAdapter {
  readonly name = "closedbook";

  constructor(
    private readonly opts: { apiKey: string; model: string; meter: SpendMeter },
  ) {}

  async ingest(_item: EvalItem, _workDir: string): Promise<IngestResult> {
    return { units: 0, ingestLlmTokens: 0, notes: "closed-book: nothing ingested" };
  }

  async answer(item: EvalItem, _workDir: string): Promise<AnswerResult> {
    const before = {
      in: this.opts.meter.inputTokens,
      out: this.opts.meter.outputTokens,
      calls: this.opts.meter.apiCalls,
      usd: this.opts.meter.totalUsd,
    };
    const res = await callClaude({
      apiKey: this.opts.apiKey,
      model: this.opts.model,
      system: SYSTEM,
      messages: [{ role: "user", content: item.question }],
      maxTokens: 256,
      temperature: 0,
      meter: this.opts.meter,
    });
    return {
      hypothesis: textOf(res.content),
      usage: {
        inputTokens: this.opts.meter.inputTokens - before.in,
        outputTokens: this.opts.meter.outputTokens - before.out,
        apiCalls: this.opts.meter.apiCalls - before.calls,
        toolCalls: 0,
        costUsd: this.opts.meter.totalUsd - before.usd,
      },
      toolTrace: [],
      messages: [{ role: "user", content: item.question }, { role: "assistant", content: res.content }],
    };
  }
}
