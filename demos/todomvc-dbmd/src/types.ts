// SPDX-License-Identifier: Apache-2.0

export type Todo = {
  id: string;
  title: string;
  completed: boolean;
  /** The record body: the todo's optional long description (checklists, notes). */
  description?: string;
  /** Store-relative record path (e.g. records/todos/buy-groceries.md). */
  path?: string;
  /** Manual sort rank (integer, ascending); records without one sort last by created. */
  order?: number;
  /** The record's full frontmatter — schema-declared enum fields render from this. */
  fm?: Record<string, unknown>;
  /** Outgoing wiki-link targets, canonical store-root form without `.md`. */
  links?: string[];
};

export type Filter = "all" | "active" | "completed";

/** The page is several views over one store; the todo list is the first. */
export type View = "todos" | "search" | "checklist" | "activity" | "ask" | "store";

/** One field of a store-declared schema (`GET /v1/schema`). */
export type SchemaField = {
  name: string;
  required: boolean;
  enum: string[] | null;
  shape: string | null;
};

/** One entry of the store's append-only timeline (`GET /v1/log/tail`). */
export type LogEntry = {
  kind: string;
  object: string;
  note: string | null;
  timestamp: string;
};

/** One ripgrep hit from `GET /v1/search`. */
export type SearchHit = {
  file: string;
  line: number;
  text: string;
};

/** One issue from `GET /v1/validate?all=1`. */
export type ValidateIssue = {
  code: string;
  file: string;
  key: string | null;
  line: number | null;
  message: string;
  severity: "error" | "warning" | "info";
  suggestion: string | null;
};

export type ValidateReport = {
  issues: ValidateIssue[];
  summary: { errors: number; warnings: number; info: number; total: number };
};

/** `GET /v1/stats`, the store-shape summary. */
export type StoreStats = {
  total_files: number;
  total_size_bytes: number;
  type_distribution: Record<string, number>;
  broken_link_count: number;
  orphan_count: number;
};

/** One `- [ ]` line parsed out of a record body. */
export type ChecklistItem = {
  todoId: string;
  /** Line index into the description (for write-back). */
  line: number;
  text: string;
  checked: boolean;
  indent: number;
};
