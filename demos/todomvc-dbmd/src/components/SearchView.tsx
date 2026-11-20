// SPDX-License-Identifier: Apache-2.0

import { useEffect, useRef, useState } from "react";
import * as api from "../apiClient";
import type { SearchHit, Todo } from "../types";

// `dbmd search` is embedded ripgrep over the store, db.md-aware
// (`--type todo` narrows via the sidecar). The query is a regex —
// alternation like (milk|demo) is the expansion path — and all-lowercase
// queries match case-insensitively (smart case).

function Highlighted({ text, query }: { text: string; query: string }) {
  try {
    const re = new RegExp(query, /[A-Z]/.test(query) ? "" : "i");
    const match = re.exec(text);
    if (match && match[0].length > 0) {
      const start = match.index;
      const end = start + match[0].length;
      return (
        <>
          {text.slice(0, start)}
          <mark>{text.slice(start, end)}</mark>
          {text.slice(end)}
        </>
      );
    }
  } catch {
    /* regex feature JS lacks — render plain */
  }
  return <>{text}</>;
}

// A hit line is a raw file line: frontmatter (`summary: …`) or body.
function hitOrigin(text: string): { label: string; shown: string } {
  const fm = /^([a-z][a-z0-9-]*): (.*)$/.exec(text);
  if (fm?.[1] !== undefined) {
    return { label: fm[1] === "summary" ? "title" : fm[1], shown: fm[2] ?? "" };
  }
  return { label: "description", shown: text };
}

export function SearchView({
  todos,
  onReveal,
}: {
  todos: Todo[];
  onReveal: (id: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [settled, setSettled] = useState(true);
  const seq = useRef(0);

  useEffect(() => {
    const trimmed = query.trim();
    const mine = ++seq.current;
    if (trimmed === "") {
      setHits([]);
      setError(null);
      setSettled(true);
      return;
    }
    setSettled(false);
    const timer = window.setTimeout(() => {
      api.searchStore(trimmed).then(
        (found) => {
          if (seq.current !== mine) return;
          setHits(found);
          setError(null);
          setSettled(true);
        },
        (searchError: unknown) => {
          if (seq.current !== mine) return;
          setHits([]);
          setError(searchError instanceof Error ? searchError.message : "search failed");
          setSettled(true);
        },
      );
    }, 250);
    return () => window.clearTimeout(timer);
  }, [query]);

  const byPath = new Map(todos.filter((todo) => todo.path).map((todo) => [todo.path, todo]));
  const groups: { todo: Todo; hits: SearchHit[] }[] = [];
  for (const hit of hits) {
    const todo = byPath.get(hit.file);
    if (!todo) continue;
    const group = groups.find((candidate) => candidate.todo.id === todo.id);
    if (group) group.hits.push(hit);
    else groups.push({ todo, hits: [hit] });
  }
  const trimmed = query.trim();

  return (
    <div className="view-panel">
      <input
        className="search-input"
        placeholder="Search the store…"
        autoFocus
        value={query}
        onChange={(event) => setQuery(event.target.value)}
        aria-label="Search query (regex)"
      />
      {trimmed === "" ? (
        <div className="view-empty">
          Titles and descriptions, straight off the files. Regex works —{" "}
          <button className="linkish" onClick={() => setQuery("(milk|demo)")}>
            (milk|demo)
          </button>{" "}
          expands a query without embeddings.
        </div>
      ) : error !== null ? (
        <div className="view-empty">{error}</div>
      ) : groups.length === 0 && settled ? (
        <div className="view-empty">No matches.</div>
      ) : (
        <>
          <div className="view-caption">
            {groups.reduce((n, group) => n + group.hits.length, 0)}{" "}
            {groups.reduce((n, group) => n + group.hits.length, 0) === 1 ? "match" : "matches"} in{" "}
            {groups.length} {groups.length === 1 ? "todo" : "todos"}
          </div>
          <ul className="result-list">
            {groups.map(({ todo, hits: todoHits }) => (
              <li key={todo.id}>
                <button className="result-row" onClick={() => onReveal(todo.id)}>
                  <span className={`result-title${todo.completed ? " done" : ""}`}>
                    <Highlighted text={todo.title} query={trimmed} />
                  </span>
                  {todoHits.map((hit) => {
                    const { label, shown } = hitOrigin(hit.text.trim());
                    return (
                      <span key={hit.line} className="result-hit">
                        <span className="result-origin">{label}</span>
                        <Highlighted text={shown} query={trimmed} />
                      </span>
                    );
                  })}
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
      <div className="view-hint">
        the same search in a shell: <code>dbmd search '{api.searchPattern(trimmed || "…")}' --type todo</code>
      </div>
    </div>
  );
}
