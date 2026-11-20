// SPDX-License-Identifier: Apache-2.0

import { useCallback, useEffect, useReducer, useRef } from "react";
import * as api from "./apiClient";
import type { ApiResponse } from "./apiClient";
import type { Todo } from "./types";

// The same action vocabulary as the official TodoMVC React example's
// reducer (plus SET_FIELD for schema-declared frontmatter fields).
// Components dispatch identical actions; the difference is where they
// land — the official app keeps them in memory, this one also persists
// every action to the db.md store, directly over `dbmd api` (the store's
// own HTTP surface; there is no backend in this repo).
export type TodoAction =
  | { type: "ADD_ITEM"; title: string; tempId?: string }
  | { type: "UPDATE_ITEM"; id: string; title: string }
  | { type: "UPDATE_DESCRIPTION"; id: string; description: string }
  | { type: "SET_FIELD"; id: string; key: string; value: string }
  | { type: "TOGGLE_ITEM"; id: string; completed?: boolean }
  | { type: "REORDER_ITEM"; id: string; prevId?: string; nextId?: string }
  | { type: "TOGGLE_ALL"; completed: boolean }
  | { type: "REMOVE_ITEM"; id: string; force?: boolean }
  | { type: "REMOVE_COMPLETED_ITEMS" }
  | { type: "SYNC"; todos: Todo[] };

function todosReducer(todos: Todo[], action: TodoAction): Todo[] {
  switch (action.type) {
    case "ADD_ITEM":
      return [...todos, { id: action.tempId ?? `tmp-${todos.length}`, title: action.title, completed: false }];
    case "UPDATE_ITEM":
      return todos.map((todo) => (todo.id === action.id ? { ...todo, title: action.title } : todo));
    case "UPDATE_DESCRIPTION":
      return todos.map((todo) =>
        todo.id === action.id ? { ...todo, description: action.description } : todo,
      );
    case "SET_FIELD":
      return todos.map((todo) =>
        todo.id === action.id ? { ...todo, fm: { ...todo.fm, [action.key]: action.value } } : todo,
      );
    case "TOGGLE_ITEM":
      return todos.map((todo) => (todo.id === action.id ? { ...todo, completed: !todo.completed } : todo));
    case "REORDER_ITEM": {
      const moved = todos.find((todo) => todo.id === action.id);
      if (!moved) return todos;
      const rest = todos.filter((todo) => todo.id !== action.id);
      let at = rest.length;
      if (action.prevId !== undefined) at = rest.findIndex((todo) => todo.id === action.prevId) + 1;
      else if (action.nextId !== undefined) at = rest.findIndex((todo) => todo.id === action.nextId);
      if (at < 0) at = rest.length;
      return [...rest.slice(0, at), moved, ...rest.slice(at)];
    }
    case "TOGGLE_ALL":
      return todos.map((todo) => ({ ...todo, completed: action.completed }));
    case "REMOVE_ITEM":
      return todos.filter((todo) => todo.id !== action.id);
    case "REMOVE_COMPLETED_ITEMS":
      return todos.filter((todo) => !todo.completed);
    case "SYNC":
      return action.todos;
  }
}

let tempCounter = 0;

export function useTodos(
  // `before` is the list as it stood when the action was dispatched — the
  // optimistic apply may have already dropped the record the error is about.
  onError?: (error: unknown, action: TodoAction, before: Todo[]) => void,
) {
  const [todos, apply] = useReducer(todosReducer, []);
  // Optimistic actions apply instantly; each is then persisted in dispatch
  // order, and the server's canonical list reconciles the state once no
  // more actions are in flight.
  const todosRef = useRef(todos);
  todosRef.current = todos;
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;
  const idMap = useRef(new Map<string, string>()); // temp id → dbmd-minted ULID
  const inFlight = useRef(0);
  const persistQueue = useRef<Promise<unknown>>(Promise.resolve());

  const refresh = useCallback(async () => {
    try {
      const { todos: fresh } = await api.listTodos();
      if (inFlight.current === 0) apply({ type: "SYNC", todos: fresh });
    } catch {
      // transient — a later event or action re-syncs
    }
  }, []);

  const persist = useCallback(async (action: TodoAction): Promise<ApiResponse | undefined> => {
    const real = (id: string) => idMap.current.get(id) ?? id;
    switch (action.type) {
      case "ADD_ITEM": {
        const result = await api.addTodo(action.title);
        if (action.tempId && result.created) idMap.current.set(action.tempId, result.created.id);
        return result;
      }
      case "UPDATE_ITEM":
        return api.patchTodo(real(action.id), { title: action.title });
      case "UPDATE_DESCRIPTION":
        return api.patchTodo(real(action.id), { description: action.description });
      case "SET_FIELD":
        return api.setField(real(action.id), action.key, action.value);
      case "TOGGLE_ITEM":
        return api.patchTodo(real(action.id), { completed: action.completed ?? false });
      case "REORDER_ITEM":
        return api.reorderTodo(
          real(action.id),
          action.prevId === undefined ? undefined : real(action.prevId),
          action.nextId === undefined ? undefined : real(action.nextId),
        );
      case "TOGGLE_ALL":
        return api.toggleAll(action.completed);
      case "REMOVE_ITEM":
        return api.removeTodo(real(action.id), { force: action.force ?? false });
      case "REMOVE_COMPLETED_ITEMS":
        return api.clearCompleted();
      case "SYNC":
        return undefined;
    }
  }, []);

  const dispatch = useCallback(
    (action: TodoAction) => {
      if (action.type === "SYNC") {
        apply(action);
        return;
      }
      let enriched = action;
      if (action.type === "ADD_ITEM") {
        enriched = { ...action, tempId: `tmp-${tempCounter++}` };
      } else if (action.type === "TOGGLE_ITEM") {
        const current = todosRef.current.find((todo) => todo.id === action.id);
        enriched = { ...action, completed: !(current?.completed ?? false) };
      }
      const before = todosRef.current;
      apply(enriched);
      inFlight.current += 1;
      const step = () => persist(enriched);
      persistQueue.current = persistQueue.current.then(step, step).then(
        (result) => {
          inFlight.current -= 1;
          if (inFlight.current === 0 && result) apply({ type: "SYNC", todos: result.todos });
        },
        (error) => {
          // The optimistic apply was wrong — roll the UI back to the
          // store's truth, then let the app explain (RM_LINKED becomes a
          // confirm, everything else a notice).
          inFlight.current -= 1;
          if (inFlight.current === 0) void refresh();
          onErrorRef.current?.(error, enriched, before);
        },
      );
    },
    [persist, refresh],
  );

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Live reload: the store's own change feed (`dbmd watch` as SSE) —
  // an agent running dbmd, an editor saving a record, another tab: any
  // writer shows up here within about a second. The feed is structured,
  // so only record (or DB.md) changes trigger a refetch — pure log/index
  // churn from this page's own writes doesn't.
  useEffect(() => {
    const revalidate = () => {
      if (inFlight.current === 0) void refresh();
    };
    const offChange = api.onStoreChange((paths) => {
      const relevant = api.batchTouches(
        paths,
        (path) => path.startsWith("records/") || path === "DB.md",
      );
      if (relevant) revalidate();
    });
    // Reconcile whenever the window regains focus too.
    window.addEventListener("focus", revalidate);
    return () => {
      window.removeEventListener("focus", revalidate);
      offChange();
    };
  }, [refresh]);

  // Body edits are the long pole — a multi-line markdown description — so
  // they get their own path: optimistic apply with snapshot rollback on
  // failure, kept out of the ordered queue so title/toggle ops never wait
  // on a body write (the store serializes writes anyway).
  const persistDescription = useCallback(
    async (id: string, description: string, options?: { optimistic?: boolean }) => {
      const todo = todosRef.current.find((t) => t.id === id);
      const previous = todo?.description;
      // The editor already renders its own draft; a checklist tick from the
      // rollup view has no editor, so it applies optimistically here.
      if (options?.optimistic) apply({ type: "UPDATE_DESCRIPTION", id, description });
      try {
        const { todos: fresh } = await api.patchTodo(id, { description });
        apply({ type: "SYNC", todos: fresh });
      } catch (error) {
        if (previous !== undefined)
          apply({ type: "UPDATE_DESCRIPTION", id, description: previous });
        onErrorRef.current?.(error, { type: "UPDATE_DESCRIPTION", id, description }, todosRef.current);
      }
    },
    [],
  );

  return { todos, dispatch, persistDescription };
}
