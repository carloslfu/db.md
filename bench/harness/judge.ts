// SPDX-License-Identifier: Apache-2.0

// bench/harness/judge.ts — scoring.
// LongMemEval: INTERIM LLM rubric (pre-registration gap G1: the official per-type judge
// prompts from xiaowu0162/LongMemEval must be vendored verbatim before freeze; this interim
// rubric is faithful in spirit and clearly labeled in every verdict).
// MuSiQue: mechanical EM/F1 with answer aliases (SQuAD normalization) — no judge surface.
import { callClaude, textOf, type SpendMeter } from "./anthropic";
import type { EvalItem, JudgeVerdict } from "./types";

// ---------- MuSiQue: EM / F1 ----------
export function normalizeAnswer(s: string): string {
  return s
    .toLowerCase()
    .replace(/\b(a|an|the)\b/g, " ")
    .replace(/[^\p{L}\p{N} ]+/gu, " ")
    .replace(/\s+/g, " ")
    .trim();
}

export function f1Score(prediction: string, gold: string): number {
  const predTokens = normalizeAnswer(prediction).split(" ").filter(Boolean);
  const goldTokens = normalizeAnswer(gold).split(" ").filter(Boolean);
  if (predTokens.length === 0 || goldTokens.length === 0) {
    return predTokens.length === goldTokens.length ? 1 : 0;
  }
  const goldCounts = new Map<string, number>();
  for (const t of goldTokens) goldCounts.set(t, (goldCounts.get(t) ?? 0) + 1);
  let overlap = 0;
  for (const t of predTokens) {
    const c = goldCounts.get(t) ?? 0;
    if (c > 0) {
      overlap++;
      goldCounts.set(t, c - 1);
    }
  }
  if (overlap === 0) return 0;
  const precision = overlap / predTokens.length;
  const recall = overlap / goldTokens.length;
  return (2 * precision * recall) / (precision + recall);
}

export function musiqueVerdict(item: EvalItem, hypothesis: string): JudgeVerdict {
  const candidates = [item.gold, ...item.goldAliases];
  let bestF1 = 0;
  let em = false;
  for (const gold of candidates) {
    bestF1 = Math.max(bestF1, f1Score(hypothesis, gold));
    if (normalizeAnswer(hypothesis) === normalizeAnswer(gold)) em = true;
  }
  return {
    correct: em || bestF1 >= 0.5,
    method: "em-f1",
    detail: `em=${em} f1=${bestF1.toFixed(3)}`,
    f1: bestF1,
    em,
  };
}

// ---------- Abstention (both datasets) ----------
const ABSTAIN_RE =
  /\b(i don'?t know|don'?t have (that|this|enough) information|no (such )?information|not (mentioned|provided|available|found|in the store)|cannot (find|answer)|unknown)\b/i;

export function abstentionVerdict(hypothesis: string): JudgeVerdict {
  const abstained = ABSTAIN_RE.test(hypothesis);
  return {
    correct: abstained,
    method: "abstention-heuristic",
    detail: abstained ? "model abstained (correct)" : "model answered instead of abstaining",
  };
}

// ---------- LongMemEval: interim LLM rubric ----------
const JUDGE_SYSTEM = `You are grading the answer of a memory-augmented assistant against a gold answer.
Rules:
- CORRECT means the model answer contains the gold answer's information, allowing paraphrase, extra wording, and formatting differences.
- Numbers, names, and dates must match the gold answer in substance (for temporal questions, the time/date/duration must be equivalent).
- For preference questions, the answer must reflect the same preference as the gold.
- An answer that hedges but states the gold content is CORRECT. An answer that states something else, or only says it does not know, is INCORRECT.
Reply with exactly one word: CORRECT or INCORRECT.`;

export async function longmemVerdict(opts: {
  apiKey: string;
  judgeModel: string;
  item: EvalItem;
  hypothesis: string;
  meter: SpendMeter;
}): Promise<JudgeVerdict> {
  if (!opts.item.answerable) return abstentionVerdict(opts.hypothesis);
  const user = [
    `Question type: ${opts.item.type}`,
    `Question: ${opts.item.question}`,
    `Gold answer: ${opts.item.gold}`,
    `Model answer: ${opts.hypothesis || "(empty)"}`,
  ].join("\n");
  const res = await callClaude({
    apiKey: opts.apiKey,
    model: opts.judgeModel,
    system: JUDGE_SYSTEM,
    messages: [{ role: "user", content: user }],
    maxTokens: 8,
    temperature: 0,
    meter: opts.meter,
  });
  const word = textOf(res.content).toUpperCase();
  return {
    correct: word.startsWith("CORRECT"),
    method: "llm-rubric-interim",
    detail: `judge=${opts.judgeModel} said ${word || "(empty)"}`,
  };
}
