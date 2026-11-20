// SPDX-License-Identifier: Apache-2.0

import type { Filter, Todo } from "../types";
import type { TodoAction } from "../useTodos";

const FILTERS: { value: Filter; label: string; href: string }[] = [
  { value: "all", label: "All", href: "#/" },
  { value: "active", label: "Active", href: "#/active" },
  { value: "completed", label: "Completed", href: "#/completed" },
];

export function Footer({
  todos,
  filter,
  dispatch,
}: {
  todos: Todo[];
  filter: Filter;
  dispatch: (action: TodoAction) => void;
}) {
  if (todos.length === 0) return null;

  const activeCount = todos.filter((todo) => !todo.completed).length;
  const completedCount = todos.length - activeCount;

  return (
    <footer className="footer">
      <span className="todo-count">
        <strong>{activeCount}</strong> {activeCount === 1 ? "item" : "items"} left
      </span>
      <ul className="filters">
        {FILTERS.map(({ value, label, href }) => (
          <li key={value}>
            <a className={filter === value ? "selected" : ""} href={href}>
              {label}
            </a>
          </li>
        ))}
      </ul>
      {completedCount > 0 && (
        <button
          className="clear-completed"
          onClick={() => dispatch({ type: "REMOVE_COMPLETED_ITEMS" })}
        >
          Clear completed
        </button>
      )}
    </footer>
  );
}
