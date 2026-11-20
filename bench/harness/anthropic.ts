// SPDX-License-Identifier: Apache-2.0

// bench/harness/anthropic.ts — minimal Messages API client (raw fetch, no SDK dependency).
// Retries 429/5xx/529,
// meters usage, and enforces a hard spend ceiling so an unattended run can never surprise-bill.

export type TextBlock = { type: "text"; text: string };
export type ToolUseBlock = { type: "tool_use"; id: string; name: string; input: unknown };
export type ToolResultBlock = {
  type: "tool_result";
  tool_use_id: string;
  content: string;
  is_error?: boolean;
};
export type ContentBlock = TextBlock | ToolUseBlock | ToolResultBlock;
export type ApiMessage = { role: "user" | "assistant"; content: string | ContentBlock[] };

export type ToolDef = {
  name: string;
  description: string;
  input_schema: Record<string, unknown>;
};

export type ClaudeResponse = {
  content: ContentBlock[];
  stopReason: string;
  inputTokens: number;
  outputTokens: number;
};

// USD per 1M tokens (input, output) by model-id prefix; claude-api skill table, 2026-07.
const PRICING: Array<[string, number, number]> = [
  ["claude-haiku-4-5", 1, 5],
  ["claude-sonnet-4", 3, 15],
  ["claude-sonnet-5", 3, 15],
  ["claude-opus-4", 5, 25],
];
const FALLBACK_PRICING: [number, number] = [10, 50]; // unknown model: assume the most expensive tier

export function priceUsd(model: string, inputTokens: number, outputTokens: number): number {
  const row = PRICING.find(([prefix]) => model.startsWith(prefix));
  const [inPerM, outPerM] = row ? [row[1], row[2]] : FALLBACK_PRICING;
  return (inputTokens / 1_000_000) * inPerM + (outputTokens / 1_000_000) * outPerM;
}

export class SpendMeter {
  totalUsd = 0;
  inputTokens = 0;
  outputTokens = 0;
  apiCalls = 0;
  constructor(readonly maxUsd: number) {}
  record(model: string, inputTokens: number, outputTokens: number): void {
    this.inputTokens += inputTokens;
    this.outputTokens += outputTokens;
    this.apiCalls += 1;
    this.totalUsd += priceUsd(model, inputTokens, outputTokens);
  }
  assertUnderBudget(): void {
    if (this.totalUsd > this.maxUsd) {
      throw new BudgetExceededError(
        `spend ceiling hit: $${this.totalUsd.toFixed(4)} > $${this.maxUsd.toFixed(2)}`,
      );
    }
  }
}

export class BudgetExceededError extends Error {}

const MAX_RETRIES = 4;

export async function callClaude(opts: {
  apiKey: string;
  model: string;
  system: string;
  messages: ApiMessage[];
  tools?: ToolDef[];
  toolChoice?: "auto" | "none";
  maxTokens?: number;
  temperature?: number;
  meter: SpendMeter;
}): Promise<ClaudeResponse> {
  opts.meter.assertUnderBudget();
  const body: Record<string, unknown> = {
    model: opts.model,
    max_tokens: opts.maxTokens ?? 1024,
    system: opts.system,
    messages: opts.messages,
  };
  if (opts.tools && opts.tools.length > 0) {
    body.tools = opts.tools;
    if (opts.toolChoice) body.tool_choice = { type: opts.toolChoice };
  }
  if (opts.temperature !== undefined) body.temperature = opts.temperature;

  let lastError = "";
  for (let attempt = 0; attempt <= MAX_RETRIES; attempt++) {
    const res = await fetch("https://api.anthropic.com/v1/messages", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": opts.apiKey,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify(body),
    });
    if (res.ok) {
      const data = (await res.json()) as {
        content: ContentBlock[];
        stop_reason: string;
        usage: { input_tokens: number; output_tokens: number };
      };
      opts.meter.record(opts.model, data.usage.input_tokens, data.usage.output_tokens);
      return {
        content: data.content,
        stopReason: data.stop_reason,
        inputTokens: data.usage.input_tokens,
        outputTokens: data.usage.output_tokens,
      };
    }
    lastError = `${res.status}: ${(await res.text()).slice(0, 400)}`;
    const retryable = res.status === 429 || res.status >= 500;
    if (!retryable || attempt === MAX_RETRIES) break;
    const retryAfter = Number(res.headers.get("retry-after"));
    const delayMs = Number.isFinite(retryAfter) && retryAfter > 0
      ? retryAfter * 1000
      : 1000 * 2 ** attempt;
    await new Promise((r) => setTimeout(r, delayMs));
  }
  throw new Error(`Anthropic API error after retries: ${lastError}`);
}

export function textOf(content: ContentBlock[]): string {
  return content
    .filter((b): b is TextBlock => b.type === "text")
    .map((b) => b.text)
    .join("\n")
    .trim();
}
