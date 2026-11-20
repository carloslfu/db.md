// SPDX-License-Identifier: Apache-2.0

import { parseChecklists } from "../derive";
import type { Todo } from "../types";

// Every `- [ ]` line across every record body, rolled up in one place.
// Derived from the same emit payload the list renders from — the store
// already sent every body; no extra requests. Ticking a box rewrites that
// exact line in the record's body (`dbmd body set` under the hood).
export function ChecklistView({
  todos,
  onToggleLine,
  onReveal,
}: {
  todos: Todo[];
  onToggleLine: (todoId: string, line: number) => void;
  onReveal: (id: string) => void;
}) {
  const items = parseChecklists(todos);
  const open = items.filter((item) => !item.checked).length;

  if (items.length === 0) {
    return (
      <div className="view-panel">
        <div className="view-empty">
          No checklists yet. Open a todo's <span className="chevron-glyph">❯</span> description and
          add <code>- [ ]</code> lines — they roll up here, across every todo.
        </div>
        <div className="view-hint">
          an agent adds one with: <code>dbmd body append records/todos/….md --text "- [ ] task"</code>
        </div>
      </div>
    );
  }

  return (
    <div className="view-panel">
      <div className="view-caption">
        {open} open {open === 1 ? "item" : "items"} across{" "}
        {new Set(items.map((item) => item.todoId)).size}{" "}
        {new Set(items.map((item) => item.todoId)).size === 1 ? "todo" : "todos"}
      </div>
      {todos.map((todo) => {
        const mine = items.filter((item) => item.todoId === todo.id);
        if (mine.length === 0) return null;
        return (
          <section key={todo.id} className="checklist-group">
            <button
              className={`checklist-todo${todo.completed ? " done" : ""}`}
              onClick={() => onReveal(todo.id)}
              title="Show this todo"
            >
              {todo.title}
            </button>
            <ul className="checklist-items">
              {mine.map((item) => (
                <li key={item.line} style={{ paddingLeft: `${item.indent * 18}px` }}>
                  <label className={item.checked ? "done" : ""}>
                    <input
                      type="checkbox"
                      checked={item.checked}
                      onChange={() => onToggleLine(todo.id, item.line)}
                    />
                    <span>{item.text}</span>
                  </label>
                </li>
              ))}
            </ul>
          </section>
        );
      })}
      <div className="view-hint">
        checklists are plain <code>- [ ]</code> lines in the record bodies — any editor or agent
        writes them
      </div>
    </div>
  );
}
