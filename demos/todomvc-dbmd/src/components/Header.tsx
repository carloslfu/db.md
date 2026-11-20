// SPDX-License-Identifier: Apache-2.0

import type { KeyboardEvent } from "react";
import type { TodoAction } from "../useTodos";

export function Header({ dispatch }: { dispatch: (action: TodoAction) => void }) {
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key !== "Enter") return;
    event.preventDefault();
    const input = event.currentTarget;
    const title = input.value.trim();
    if (!title) return;
    dispatch({ type: "ADD_ITEM", title });
    input.value = "";
  };

  return (
    <header className="header">
      <input
        className="new-todo"
        placeholder="What needs to be done?"
        autoFocus
        onKeyDown={onKeyDown}
        aria-label="New todo input"
      />
    </header>
  );
}
