// SPDX-License-Identifier: Apache-2.0

// bench/harness/datasets.ts — loaders that normalize both datasets into EvalItem.
import { readFileSync } from "node:fs";
import type { EvalItem, IngestUnit } from "./types";

// ---------- LongMemEval ----------
// File schema (xiaowu0162/longmemeval-cleaned; oracle + S share it): one JSON array of
// questions; each question carries its own haystack of sessions with per-session dates.
type LmeTurn = { role: string; content: string; has_answer?: boolean };
type LmeQuestion = {
  question_id: string;
  question_type: string;
  question: string;
  answer: string | number;
  question_date?: string;
  haystack_dates?: string[];
  haystack_session_ids?: string[];
  haystack_sessions: LmeTurn[][];
  answer_session_ids?: string[];
};

export function loadLongMemEval(filePath: string): EvalItem[] {
  const raw = JSON.parse(readFileSync(filePath, "utf8")) as LmeQuestion[];
  return raw.map((q) => {
    const units: IngestUnit[] = q.haystack_sessions.map((session, i) => {
      const date = q.haystack_dates?.[i];
      const id = q.haystack_session_ids?.[i] ?? `session-${i + 1}`;
      const turns = session
        .map((t) => `[${t.role.toUpperCase()}]: ${t.content}`)
        .join("\n\n");
      return {
        kind: "session" as const,
        id,
        date,
        text: `Session date: ${date ?? "unknown"}\n\n${turns}`,
      };
    });
    return {
      id: q.question_id,
      dataset: "longmemeval" as const,
      type: q.question_type,
      question: q.question,
      questionDate: q.question_date,
      gold: String(q.answer),
      goldAliases: [],
      // Official convention: question_ids ending in _abs are abstention items.
      answerable: !q.question_id.endsWith("_abs"),
      units,
    };
  });
}

// ---------- MuSiQue ----------
// JSONL (musique_ans dev, official format / HF mirror rows): one item per line.
type MusiqueParagraph = { idx?: number; title: string; paragraph_text: string; is_supporting?: boolean };
type MusiqueItem = {
  id: string;
  question: string;
  answer: string;
  answer_aliases?: string[];
  answerable?: boolean;
  paragraphs: MusiqueParagraph[];
};

export function loadMusique(filePath: string): EvalItem[] {
  const lines = readFileSync(filePath, "utf8").split(/\r?\n/).filter((l) => l.trim());
  return lines.map((line) => {
    const q = JSON.parse(line) as MusiqueItem;
    const hops = q.id.match(/^(\d)hop/)?.[1] ?? "?";
    const units: IngestUnit[] = q.paragraphs.map((p, i) => ({
      kind: "paragraph" as const,
      id: `p${p.idx ?? i}`,
      title: p.title,
      text: `# ${p.title}\n\n${p.paragraph_text}`,
    }));
    return {
      id: q.id,
      dataset: "musique" as const,
      type: `musique-${hops}hop`,
      question: q.question,
      gold: q.answer,
      goldAliases: q.answer_aliases ?? [],
      answerable: q.answerable ?? true,
      units,
    };
  });
}

/**
 * Deterministic smoke slice: round-robin across question types (sorted), so ≤10 items still
 * touch every ability class. Never used for reported runs (PROTOCOL §2.1 uses seeded RNG
 * over the full set instead).
 */
export function smokeSlice(items: EvalItem[], limit: number): EvalItem[] {
  const byType = new Map<string, EvalItem[]>();
  for (const item of [...items].sort((a, b) => a.id.localeCompare(b.id))) {
    const bucket = byType.get(item.type) ?? [];
    bucket.push(item);
    byType.set(item.type, bucket);
  }
  const types = [...byType.keys()].sort();
  const picked: EvalItem[] = [];
  let round = 0;
  while (picked.length < limit) {
    let advanced = false;
    for (const type of types) {
      const bucket = byType.get(type)!;
      if (round < bucket.length && picked.length < limit) {
        picked.push(bucket[round]);
        advanced = true;
      }
    }
    if (!advanced) break;
    round++;
  }
  return picked;
}
