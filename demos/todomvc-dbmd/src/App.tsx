// SPDX-License-Identifier: Apache-2.0

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ApiError } from "./apiClient";
import { ActivityView } from "./components/ActivityView";
import { AskView } from "./components/AskView";
import { ChecklistView } from "./components/ChecklistView";
import { Footer } from "./components/Footer";
import { Header } from "./components/Header";
import { Main } from "./components/Main";
import { SearchView } from "./components/SearchView";
import { StoreView } from "./components/StoreView";
import { Toast } from "./components/Toast";
import type { ToastState } from "./components/Toast";
import { ViewNav } from "./components/ViewNav";
import { buildLinkIndex, toggleChecklistLine } from "./derive";
import type { Todo } from "./types";
import { useRoute } from "./useRoute";
import { useConnection, useSchema } from "./useStoreMeta";
import { useTodos } from "./useTodos";
import type { TodoAction } from "./useTodos";

const shortName = (path: string) => path.replace(/^records\/todos\//, "").replace(/\.md$/, "");

// One store, several views. The todo list is the classic TodoMVC lens;
// search / checklist / activity / store are other lenses over the same
// db.md store, each one the same-named dbmd verb an agent would run.
export function App() {
  const { view, filter } = useRoute();
  const schema = useSchema();
  const connection = useConnection();
  const [toast, setToast] = useState<ToastState | null>(null);
  const [reveal, setReveal] = useState<{ id: string; nonce: number } | null>(null);
  const todosRef = useRef<Todo[]>([]);
  const dispatchRef = useRef<(action: TodoAction) => void>(() => {});

  // Persistence failures surface here after the hook rolls the UI back.
  // RM_LINKED is the interesting one: the store REFUSED a delete because
  // other records still wiki-link to this one — turn that into a confirm.
  const onError = useCallback((error: unknown, action: TodoAction, before: Todo[]) => {
    if (error instanceof ApiError && error.code === "RM_LINKED") {
      const backlinks = (error.details?.backlinks as string[] | undefined) ?? [];
      const names = backlinks
        .map((path) => before.find((todo) => todo.path === path)?.title ?? shortName(path))
        .join(", ");
      if (action.type === "REMOVE_ITEM") {
        const title = before.find((todo) => todo.id === action.id)?.title ?? "this todo";
        setToast({
          message: `The store refused: “${title}” is still linked from ${names}.`,
          detail: "Deleting anyway breaks those links (dbmd rm --force); validate will flag them.",
          actions: [
            {
              label: "delete anyway",
              danger: true,
              onClick: () => {
                setToast(null);
                dispatchRef.current({ type: "REMOVE_ITEM", id: action.id, force: true });
              },
            },
          ],
        });
      } else {
        const kept = ((error.details?.kept as string[] | undefined) ?? [])
          .map((path) => before.find((todo) => todo.path === path)?.title ?? shortName(path))
          .join(", ");
        setToast({
          message: `Still linked, so kept: ${kept || names}. Delete those individually to confirm.`,
        });
      }
      return;
    }
    setToast({
      message:
        error instanceof ApiError
          ? error.message
          : "Couldn't reach the store — is `dbmd api` running?",
    });
  }, []);

  const { todos, dispatch, persistDescription } = useTodos(onError);
  todosRef.current = todos;
  dispatchRef.current = dispatch;

  const linkIndex = useMemo(() => buildLinkIndex(todos), [todos]);
  // Every schema enum field except `status` (the checkbox owns that one)
  // becomes a control on the rows — the store's contract drives the UI.
  const enumFields = useMemo(
    () => schema.filter((field) => field.enum !== null && field.name !== "status"),
    [schema],
  );

  // Plain notices dismiss themselves; confirms wait for a click.
  useEffect(() => {
    if (toast === null || toast.actions !== undefined) return;
    const timer = window.setTimeout(() => setToast(null), 6000);
    return () => window.clearTimeout(timer);
  }, [toast]);

  // Any view can point at a todo (search hit, checklist group, log row,
  // link chip): jump to the list, scroll it into view, flash it once.
  const revealTodo = useCallback((id: string) => {
    window.location.hash = "#/";
    setReveal((previous) => ({ id, nonce: (previous?.nonce ?? 0) + 1 }));
  }, []);

  const onToggleLine = useCallback(
    (todoId: string, line: number) => {
      const todo = todosRef.current.find((candidate) => candidate.id === todoId);
      if (!todo) return;
      const next = toggleChecklistLine(todo.description ?? "", line);
      if (next !== null) persistDescription(todoId, next, { optimistic: true });
    },
    [persistDescription],
  );

  return (
    <>
      <section className="todoapp">
        <h1>todos</h1>
        <ViewNav view={view} />
        {connection === "offline" && (
          <div className="offline-banner">
            can't reach the store — run <code>dbmd api --dir db</code> (retrying…)
          </div>
        )}
        {view === "todos" && (
          <>
            <Header dispatch={dispatch} />
            <Main
              todos={todos}
              filter={filter}
              dispatch={dispatch}
              persistDescription={persistDescription}
              enumFields={enumFields}
              linkIndex={linkIndex}
              reveal={reveal}
              onReveal={revealTodo}
            />
            <Footer todos={todos} filter={filter} dispatch={dispatch} />
          </>
        )}
        {view === "search" && <SearchView todos={todos} onReveal={revealTodo} />}
        {view === "checklist" && (
          <ChecklistView todos={todos} onToggleLine={onToggleLine} onReveal={revealTodo} />
        )}
        {view === "activity" && <ActivityView todos={todos} onReveal={revealTodo} />}
        {view === "ask" && <AskView />}
        {view === "store" && <StoreView schema={schema} />}
      </section>
      <footer className="info">
        <p>Double-click to edit a todo</p>
        <p>
          One <a href="https://github.com/carloslfu/db.md">db.md</a> store, six views — every todo
          is a markdown record in <code>db/records/todos/</code>, and the store serves itself
          (<code>dbmd api</code>; this page has no backend)
        </p>
      </footer>
      <Toast toast={toast} onDismiss={() => setToast(null)} />
    </>
  );
}
