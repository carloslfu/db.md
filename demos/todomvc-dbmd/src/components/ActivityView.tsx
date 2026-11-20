// SPDX-License-Identifier: Apache-2.0

import { useEffect, useState } from "react";
import * as api from "../apiClient";
import type { LogEntry, Todo } from "../types";

// The store's own timeline: log.md, read back through `dbmd log tail`.
// Every writer appends here — this page logs each action, and an agent
// working the store from a shell lands on the same feed, interleaved.

const KIND_CLASS: Record<string, string> = {
  create: "kind-create",
  update: "kind-update",
  delete: "kind-delete",
};

export function ActivityView({
  todos,
  onReveal,
}: {
  todos: Todo[];
  onReveal: (id: string) => void;
}) {
  const [entries, setEntries] = useState<LogEntry[] | null>(null);

  useEffect(() => {
    const load = () => api.fetchLog(200).then(setEntries, () => {});
    load();
    // The change feed watches the semantic store (DB.md + records), not
    // log.md itself — but every mutating op that logs also touches a
    // record, so refreshing on any batch shows those entries within a
    // second. A PURE `dbmd log` append travels outside the feed; a slow
    // poll while this view is open keeps the timeline honest for that
    // case too.
    const offChange = api.onStoreChange(() => load());
    const timer = window.setInterval(load, 5000);
    const onFocus = () => load();
    window.addEventListener("focus", onFocus);
    return () => {
      offChange();
      window.clearInterval(timer);
      window.removeEventListener("focus", onFocus);
    };
  }, []);

  if (entries === null) return <div className="view-panel" />;
  if (entries.length === 0) {
    return (
      <div className="view-panel">
        <div className="view-empty">Nothing logged yet — every action lands here.</div>
        <div className="view-hint">
          the same timeline in a shell: <code>dbmd log tail</code>
        </div>
      </div>
    );
  }

  const byPath = new Map(todos.filter((todo) => todo.path).map((todo) => [todo.path, todo]));
  const newestFirst = [...entries].reverse();
  const rows: { date: string; items: LogEntry[] }[] = [];
  for (const entry of newestFirst) {
    const date = entry.timestamp.split(" ")[0] ?? entry.timestamp;
    const group = rows[rows.length - 1];
    if (group && group.date === date) group.items.push(entry);
    else rows.push({ date, items: [entry] });
  }

  return (
    <div className="view-panel">
      {rows.map(({ date, items }) => (
        <section key={date}>
          <div className="log-date" title="store time (UTC)">
            {date}
          </div>
          <ul className="log-list">
            {items.map((entry, index) => {
              const todo = byPath.get(entry.object);
              const time = entry.timestamp.split(" ")[1] ?? "";
              const label = todo
                ? todo.title
                : entry.object.replace(/^records\/todos\/?/, "").replace(/\.md$/, "") ||
                  entry.object;
              return (
                <li key={`${entry.timestamp}-${index}`}>
                  <button
                    className="log-row"
                    onClick={todo ? () => onReveal(todo.id) : undefined}
                    disabled={!todo}
                  >
                    <span className={`log-kind ${KIND_CLASS[entry.kind] ?? "kind-other"}`} />
                    <span className="log-body">
                      <span className="log-object">{label}</span>
                      {entry.note && <span className="log-note">{entry.note}</span>}
                    </span>
                    <span className="log-time" title={`${entry.timestamp} UTC`}>
                      {time}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        </section>
      ))}
      <div className="view-hint">
        log.md, the store's append-only timeline — read it with <code>dbmd log tail</code>, add to
        it with <code>dbmd log update &lt;object&gt; -m "note"</code>
      </div>
    </div>
  );
}
