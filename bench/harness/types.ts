// SPDX-License-Identifier: Apache-2.0

// bench/harness/types.ts — the cross-system contract (see bench/PROTOCOL.md §10).

export type DatasetName = "longmemeval" | "musique";

/** One unit of history handed to a system at ingestion time (never contains the question). */
export type IngestUnit = {
  kind: "session" | "paragraph";
  id: string;
  /** ISO-ish date string when the source carries one (LongMemEval session dates). */
  date?: string;
  title?: string;
  /** Verbatim rendered text of the unit. */
  text: string;
};

export type EvalItem = {
  id: string;
  dataset: DatasetName;
  /** LongMemEval question_type, or "musique-<n>hop". */
  type: string;
  question: string;
  questionDate?: string;
  gold: string;
  goldAliases: string[];
  /** false = the correct behavior is abstention ("I don't know"). */
  answerable: boolean;
  units: IngestUnit[];
};

export type Usage = {
  inputTokens: number;
  outputTokens: number;
  apiCalls: number;
  toolCalls: number;
  costUsd: number;
};

export type ToolTraceEntry = {
  tool: string;
  input: unknown;
  outputPreview: string;
};

export type IngestResult = {
  units: number;
  /** LLM tokens spent at ingestion time (0 for db.md — mechanical; nonzero for extraction pipelines). */
  ingestLlmTokens: number;
  notes?: string;
};

export type AnswerResult = {
  hypothesis: string;
  usage: Usage;
  toolTrace: ToolTraceEntry[];
  /** Full message log (auditable transcript — published per PROTOCOL §9.1). */
  messages: unknown[];
};

/**
 * The system-under-test contract. External systems (mem0, Letta, Zep) can implement this in
 * TypeScript or run out-of-process and emit the same per-item JSONL — the contract is pinned,
 * not the language.
 */
export interface SystemAdapter {
  readonly name: string;
  /** Build the system's memory for one item under workDir. Must not see the question. */
  ingest(item: EvalItem, workDir: string): Promise<IngestResult>;
  /** Answer the item's question from the ingested memory. */
  answer(item: EvalItem, workDir: string): Promise<AnswerResult>;
}

export type JudgeVerdict = {
  correct: boolean;
  method: "llm-rubric-interim" | "abstention-heuristic" | "em-f1";
  detail: string;
  /** MuSiQue only. */
  f1?: number;
  em?: boolean;
};

export type ItemRecord = {
  id: string;
  type: string;
  question: string;
  questionDate?: string;
  gold: string;
  answerable: boolean;
  hypothesis: string;
  verdict: JudgeVerdict;
  ingest: IngestResult;
  usage: Usage;
  toolTrace: ToolTraceEntry[];
  messages: unknown[];
  wallClockMs: number;
};

export type RunSummary = {
  runId: string;
  startedAt: string;
  finishedAt: string;
  protocol: "DRAFT-2026-07-13 (pre-freeze; not citable)";
  smoke: boolean;
  dataset: DatasetName;
  datasetFile: string;
  adapter: string;
  model: string;
  judgeModel: string;
  maxRounds: number;
  n: number;
  correct: number;
  accuracy: number;
  byType: Record<string, { n: number; correct: number }>;
  totals: Usage;
  wallClockMs: number;
  aborted?: string;
  items: Array<{ id: string; type: string; correct: boolean; costUsd: number }>;
};
