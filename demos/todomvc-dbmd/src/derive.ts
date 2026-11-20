// SPDX-License-Identifier: Apache-2.0

import type { ChecklistItem, Todo } from "./types";

// Views derived from data the page already holds: one `emit` carries every
// record's body and wiki-link edges, so the checklist rollup and the link
// graph are pure client-side projections — no extra requests, always in
// sync with the rendered list.

const CHECKLIST_LINE = /^(\s*)- \[([ xX])\] (.*)$/;

/** Every `- [ ]` / `- [x]` line across all todo descriptions, in list order. */
export function parseChecklists(todos: Todo[]): ChecklistItem[] {
  const items: ChecklistItem[] = [];
  for (const todo of todos) {
    const description = todo.description ?? "";
    if (description === "") continue;
    description.split("\n").forEach((text, line) => {
      const match = CHECKLIST_LINE.exec(text);
      if (!match) return;
      items.push({
        todoId: todo.id,
        line,
        text: match[3] ?? "",
        checked: match[2] !== " ",
        indent: Math.floor((match[1] ?? "").length / 2),
      });
    });
  }
  return items;
}

/**
 * Flip one checklist line of a description. Returns the new description, or
 * null when that line no longer holds a checklist item (state went stale).
 */
export function toggleChecklistLine(description: string, line: number): string | null {
  const lines = description.split("\n");
  const current = lines[line];
  if (current === undefined) return null;
  const match = CHECKLIST_LINE.exec(current);
  if (!match) return null;
  const box = match[2] === " " ? "x" : " ";
  lines[line] = `${match[1]}- [${box}] ${match[3]}`;
  return lines.join("\n");
}

export type TodoLinks = { out: Todo[]; in: Todo[] };

/**
 * The wiki-link graph between todos, both directions, from the `links`
 * edges the emit already carries (canonical store-root targets, no `.md`).
 */
export function buildLinkIndex(todos: Todo[]): Map<string, TodoLinks> {
  const byKey = new Map<string, Todo>();
  for (const todo of todos) {
    if (todo.path) byKey.set(todo.path.replace(/\.md$/, "").toLowerCase(), todo);
  }
  const index = new Map<string, TodoLinks>();
  const entry = (id: string): TodoLinks => {
    let links = index.get(id);
    if (!links) {
      links = { out: [], in: [] };
      index.set(id, links);
    }
    return links;
  };
  for (const todo of todos) {
    for (const target of todo.links ?? []) {
      const other = byKey.get(target.replace(/\.md$/, "").toLowerCase());
      if (!other || other.id === todo.id) continue;
      entry(todo.id).out.push(other);
      entry(other.id).in.push(todo);
    }
  }
  return index;
}
