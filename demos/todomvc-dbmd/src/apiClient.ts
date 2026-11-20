// SPDX-License-Identifier: Apache-2.0

// The app's whole data layer — entirely CLIENT-side. Every operation is an
// HTTP call against `dbmd api`, the db.md toolkit's own local app server:
// the same verbs an agent runs in a shell, with the same schema
// enforcement, frozen-page policy, store transaction lock, and
// write-through indexes. There is no backend in this repo — vite only
// serves static files (and, in dev, spawns `dbmd api` as a convenience).
//
// Wire contract (see `dbmd api --help` / GET /v1): routes are named after
// verbs, `--json` output rides through verbatim, and CLI exit codes map to
// HTTP statuses — 409 is a collision (`PATH_COLLISION`, `RM_LINKED`), 403 a
// policy refusal, 422 a failed validation. Errors carry the CLI's
// structured `{"error":{code,message,details?}}` body, so this client
// branches on `error.code` exactly like a shell consumer.
import type {
  LogEntry,
  SchemaField,
  SearchHit,
  StoreStats,
  Todo,
  ValidateReport,
} from "./types";

// The store's API server. `npm run dev` spawns it on the default port;
// point the page elsewhere with `?api=http://127.0.0.1:PORT`.
export const API_BASE = (
  new URLSearchParams(window.location.search).get("api") ?? "http://127.0.0.1:3261"
).replace(/\/+$/, "");

export type ApiResponse = { todos: Todo[]; created?: { id: string; path: string } };

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly details?: Record<string, unknown>,
  ) {
    super(message);
  }
}

async function call<T>(
  method: string,
  route: string,
  body?: string,
  contentType?: string,
): Promise<T> {
  const response = await fetch(`${API_BASE}${route}`, {
    method,
    body,
    headers: contentType === undefined ? undefined : { "content-type": contentType },
  });
  const text = await response.text();
  const parsed = (() => {
    try {
      return JSON.parse(text) as Record<string, unknown>;
    } catch {
      return undefined;
    }
  })();
  if (!response.ok) {
    const error = parsed?.error as
      | { code?: string; message?: string; details?: Record<string, unknown> }
      | undefined;
    throw new ApiError(
      response.status,
      error?.code ?? `HTTP_${response.status}`,
      error?.message ?? `${method} ${route} → ${response.status}`,
      error?.details,
    );
  }
  return parsed as T;
}

// ── list: one `emit` — every record with frontmatter, body, AND wiki-link
// edges in a single request; the whole UI (list, checklist rollup, link
// chips) derives from this one payload.
type EmittedFile = {
  path: string;
  type: string | null;
  summary: string | null;
  body: string;
  created: string | null;
  frontmatter: Record<string, unknown>;
  links: string[];
};

const parseOrder = (value: unknown): number | undefined => {
  if (value === undefined || value === null) return undefined;
  const parsed = typeof value === "number" ? value : Number(value);
  return Number.isFinite(parsed) ? parsed : undefined;
};

type Listed = Todo & {
  created: string;
  path: string;
  description: string;
  fm: Record<string, unknown>;
  links: string[];
};

// Kept hot by every list() call (each mutation and each change-feed tick
// re-lists), so id → record resolution is usually a lookup, not a request.
let listed: Listed[] = [];

async function list(): Promise<Listed[]> {
  const dump = await call<{ files: EmittedFile[] }>("GET", "/v1/emit");
  const todos = dump.files
    .filter((file) => file.type === "todo")
    .map((file): Listed => {
      const fm = file.frontmatter ?? {};
      // Canonical bodies end the file with one trailing newline; the
      // description value itself must not carry it.
      let description = file.body;
      if (description.endsWith("\n")) description = description.slice(0, -1);
      return {
        id: typeof fm.id === "string" && fm.id !== "" ? fm.id : file.path,
        title: file.summary ?? "",
        completed: fm.status === "completed",
        description,
        path: file.path,
        order: parseOrder(fm.order),
        created: file.created ?? "",
        fm,
        links: file.links ?? [],
      };
    });
  // Ranked records first by rank; unranked ones keep insertion order at the
  // end (so a fresh store behaves exactly like classic TodoMVC).
  listed = todos.sort((a, b) => {
    const aRank = a.order ?? Number.POSITIVE_INFINITY;
    const bRank = b.order ?? Number.POSITIVE_INFINITY;
    if (aRank !== bRank) return aRank - bRank;
    return a.created === b.created ? a.id.localeCompare(b.id) : a.created.localeCompare(b.created);
  });
  return listed;
}

export async function listTodos(): Promise<ApiResponse> {
  return { todos: await list() };
}

async function resolveById(id: string): Promise<Listed> {
  const cached = listed.find((candidate) => candidate.id === id);
  if (cached) return cached;
  const todo = (await list()).find((candidate) => candidate.id === id);
  if (!todo) throw new ApiError(404, "NOT_FOUND", `no todo with id ${id}`);
  return todo;
}

// The store keeps its own timeline (log.md); a log failure never blocks the
// user-visible operation.
async function logAction(kind: string, object: string, note: string): Promise<void> {
  try {
    const query = new URLSearchParams({ kind, object, m: note.slice(0, 120) });
    await call("POST", `/v1/log?${query}`);
  } catch {
    /* best-effort */
  }
}

const fmRoute = (path: string, key: string) =>
  `/v1/fm?${new URLSearchParams({ file: path, key })}`;

const setFm = (path: string, key: string, value: string) =>
  call("PUT", fmRoute(path, key), value, "text/plain");

function slugify(title: string): string {
  const slug = title
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[̀-ͯ]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 60)
    .replace(/-+$/, "");
  return slug || "todo";
}

export async function addTodo(title: string): Promise<ApiResponse> {
  const trimmed = title.trim();
  if (!trimmed) throw new ApiError(400, "EMPTY_TITLE", "title is required");
  const base = slugify(trimmed);
  let written: string | undefined;
  for (let attempt = 0; written === undefined; attempt++) {
    if (attempt > 20) throw new ApiError(409, "PATH_COLLISION", `no free path for "${base}"`);
    const slug = attempt === 0 ? base : `${base}-${attempt + 1}`;
    try {
      const result = await call<{ written: string }>(
        "POST",
        "/v1/write",
        JSON.stringify({
          path: `records/todos/${slug}.md`,
          type: "todo",
          summary: trimmed,
          fm: { status: "active", "meta-type": "operational" },
        }),
        "application/json",
      );
      written = result.written;
    } catch (error) {
      if (!(error instanceof ApiError && error.code === "PATH_COLLISION")) throw error;
    }
  }
  // `write --json` reports the resolved path; the minted ULID is one
  // frontmatter read away.
  const { value: id } = await call<{ value: string }>("GET", fmRoute(written, "id"));
  await logAction("create", written, `add: ${trimmed}`);
  return { created: { id, path: written }, todos: await list() };
}

export async function patchTodo(
  id: string,
  patch: { title?: string; completed?: boolean; description?: string },
): Promise<ApiResponse> {
  const todo = await resolveById(id);
  const notes: string[] = [];
  if (patch.title !== undefined) {
    const title = patch.title.trim();
    if (!title) throw new ApiError(400, "EMPTY_TITLE", "title cannot be empty — delete instead");
    if (title !== todo.title) {
      await setFm(todo.path, "summary", title);
      notes.push(`edit: ${title}`);
    }
  }
  if (patch.completed !== undefined && patch.completed !== todo.completed) {
    await setFm(todo.path, "status", patch.completed ? "completed" : "active");
    notes.push(patch.completed ? "completed" : "reactivated");
  }
  if (patch.description !== undefined && patch.description !== todo.description) {
    // `body set` stores the content verbatim, re-stamps `updated`, and
    // updates the type-folder index write-through.
    const body = patch.description === "" ? "" : `${patch.description}\n`;
    await call(
      "PUT",
      `/v1/body?${new URLSearchParams({ file: todo.path })}`,
      body,
      "text/plain",
    );
    notes.push(
      patch.description === ""
        ? "description cleared"
        : `description updated (${patch.description.length} chars)`,
    );
  }
  if (notes.length > 0) await logAction("update", todo.path, notes.join("; "));
  return { todos: await list() };
}

// Any frontmatter field by name — the UI calls this for schema-declared
// enum fields (`priority` today; whatever the store declares tomorrow).
export async function setField(id: string, key: string, value: string): Promise<ApiResponse> {
  const todo = await resolveById(id);
  await setFm(todo.path, key, value);
  await logAction("update", todo.path, `${key} → ${value}`);
  return { todos: await list() };
}

// Move one todo between two others (either may be absent at the ends).
// Ranks are integers with gaps so a drop usually writes only the moved
// record; when the gap closes, every rank is renormalized to 100·i.
export async function reorderTodo(
  id: string,
  prevId?: string,
  nextId?: string,
): Promise<ApiResponse> {
  let todos = await list();
  if (!todos.some((todo) => todo.id === id)) {
    throw new ApiError(404, "NOT_FOUND", `no todo with id ${id}`);
  }

  const materialize = async (ordered: Listed[]) => {
    for (let i = 0; i < ordered.length; i++) {
      const record = ordered[i];
      if (!record) continue;
      const rank = (i + 1) * 100;
      if (record.order !== rank) {
        await setFm(record.path, "order", String(rank));
        record.order = rank;
      }
    }
  };

  // First reorder on a store without ranks: freeze the current sort into
  // explicit ranks so the neighbor math below has numbers to work with.
  if (todos.some((todo) => todo.order === undefined)) await materialize(todos);

  const moved = todos.find((todo) => todo.id === id);
  if (!moved) return { todos: await list() };
  const prev = prevId === undefined ? undefined : todos.find((todo) => todo.id === prevId);
  const next = nextId === undefined ? undefined : todos.find((todo) => todo.id === nextId);

  let rank: number | undefined;
  if (prev?.order !== undefined && next?.order !== undefined) {
    const gap = next.order - prev.order;
    if (gap >= 2) rank = prev.order + Math.floor(gap / 2);
  } else if (prev?.order !== undefined) {
    rank = prev.order + 100;
  } else if (next?.order !== undefined) {
    if (next.order >= 2) rank = Math.floor(next.order / 2);
  } else {
    rank = 100;
  }

  if (rank === undefined) {
    // No integer fits between the neighbors: renormalize with the moved
    // record already in its target position.
    const rest = todos.filter((todo) => todo.id !== id);
    let at = rest.length;
    if (prev) at = rest.findIndex((todo) => todo.id === prev.id) + 1;
    else if (next) at = rest.findIndex((todo) => todo.id === next.id);
    if (at < 0) at = rest.length;
    todos = [...rest.slice(0, at), moved, ...rest.slice(at)];
    await materialize(todos);
  } else {
    await setFm(moved.path, "order", String(rank));
  }
  await logAction("update", moved.path, `reorder: ${moved.title}`);
  return { todos: await list() };
}

export async function toggleAll(completed: boolean): Promise<ApiResponse> {
  const todos = await list();
  const status = completed ? "completed" : "active";
  let changed = 0;
  for (const todo of todos) {
    if (todo.completed !== completed) {
      await setFm(todo.path, "status", status);
      changed++;
    }
  }
  if (changed > 0) {
    await logAction("update", "records/todos", `toggle all → ${status} (${changed})`);
  }
  return { todos: await list() };
}

const rmRoute = (path: string, force: boolean) =>
  `/v1/rm?${new URLSearchParams(force ? { path, force: "1" } : { path })}`;

// Link-aware delete: without force, `dbmd rm` refuses (409 RM_LINKED, the
// backlinks in `details`) while other todos still link to this one — the
// UI turns that refusal into a confirm.
export async function removeTodo(id: string, options?: { force?: boolean }): Promise<ApiResponse> {
  const todo = await resolveById(id);
  await call("DELETE", rmRoute(todo.path, options?.force ?? false));
  await logAction("delete", todo.path, `remove: ${todo.title}`);
  return { todos: await list() };
}

export async function clearCompleted(): Promise<ApiResponse> {
  const todos = await list();
  const completed = todos.filter((todo) => todo.completed);
  const kept: string[] = [];
  let removed = 0;
  for (const todo of completed) {
    try {
      await call("DELETE", rmRoute(todo.path, false));
      removed++;
    } catch (error) {
      // A still-linked record refuses (RM_LINKED); clear the rest and
      // report the kept ones once, so one protected todo doesn't block
      // the sweep.
      if (error instanceof ApiError && error.code === "RM_LINKED") kept.push(todo.path);
      else throw error;
    }
  }
  if (removed > 0) {
    await logAction("delete", "records/todos", `clear completed (${removed})`);
  }
  if (kept.length > 0) {
    throw new ApiError(409, "RM_LINKED", "some completed todos are still linked", { kept });
  }
  return { todos: await list() };
}

// ── The other views' reads: each one is the same-named dbmd verb.

export async function fetchLog(n = 200): Promise<LogEntry[]> {
  const entries = await call<LogEntry[]>("GET", `/v1/log/tail?${new URLSearchParams({ n: String(n) })}`);
  return entries;
}

// `dbmd search` is embedded ripgrep: the query is a regex, alternation is
// the documented expansion path. Smart case: all-lowercase queries match
// case-insensitively.
export function searchPattern(query: string): string {
  return /[A-Z]/.test(query) ? query : `(?i)${query}`;
}

export async function searchStore(query: string): Promise<SearchHit[]> {
  const params = new URLSearchParams({ q: searchPattern(query), type: "todo" });
  return call<SearchHit[]>("GET", `/v1/search?${params}`);
}

export async function fetchValidate(): Promise<ValidateReport> {
  // An invalid store is not a transport failure: `dbmd validate` exits 6
  // (→ 422) and puts the full report on stdout — the 422 body IS the
  // answer. Only statuses without a report in the body are real errors.
  const response = await fetch(`${API_BASE}/v1/validate?all=1`);
  const text = await response.text();
  const parsed = (() => {
    try {
      return JSON.parse(text) as Record<string, unknown>;
    } catch {
      return undefined;
    }
  })();
  if (parsed && Array.isArray(parsed.issues)) return parsed as unknown as ValidateReport;
  throw new ApiError(response.status, "VALIDATE_UNAVAILABLE", `validate → ${response.status}`);
}

export async function fetchStats(): Promise<StoreStats> {
  return call<StoreStats>("GET", "/v1/stats");
}

export async function fetchSchema(): Promise<SchemaField[]> {
  const schema = await call<{ types: Record<string, { fields: SchemaField[] }> }>(
    "GET",
    "/v1/schema",
  );
  return schema.types.todo?.fields ?? [];
}

export async function fetchVersion(): Promise<string> {
  const { version } = await call<{ version: string }>("GET", "/v1/version");
  return version;
}

// ── ask / do: the store's embedded harness (`dbmd ask` / `dbmd do` over
// SSE). The server streams the toolkit's flat event feed — each tool call
// carries the exact dbmd one-liner it executes — and this client hands the
// frames to the view as they arrive. Stateless server: each call is one
// run; conversation display is purely client-side.

export type AskEvent =
  | { event: "thinking_delta"; text: string }
  | { event: "text_delta"; text: string }
  | { event: "tool_call"; id: string; name: string; args: unknown; display: string }
  | { event: "tool_result"; id: string; name: string; content: string; is_error: boolean }
  | { event: "usage"; input: number; output: number }
  | { event: "turn_end"; stop: string }
  | { event: "done"; text: string; turns: number }
  // An adjustment the harness made on our behalf (a reasoning parameter the
  // provider refused, say). It exists so a silently-downgraded run never
  // looks like a clean one — so it must reach the transcript.
  | { event: "notice"; message: string }
  | { event: "error"; message: string };

export async function askStream(
  prompt: string,
  write: boolean,
  onEvent: (event: AskEvent) => void,
): Promise<void> {
  // EventSource cannot POST; read the SSE stream off fetch's body instead.
  const response = await fetch(`${API_BASE}/v1/${write ? "do" : "ask"}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ prompt }),
  });
  if (!response.ok || response.body === null) {
    const text = await response.text();
    let code = `HTTP_${response.status}`;
    let message = `ask → ${response.status}`;
    try {
      const parsed = JSON.parse(text) as {
        error?: { code?: string; message?: string };
      };
      code = parsed.error?.code ?? code;
      message = parsed.error?.message ?? message;
    } catch {
      /* non-JSON error body */
    }
    throw new ApiError(response.status, code, message);
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  const dispatch = (frame: string) => {
    for (const line of frame.split("\n")) {
      if (!line.startsWith("data: ")) continue;
      try {
        onEvent(JSON.parse(line.slice(6)) as AskEvent);
      } catch {
        /* tolerate torn frames */
      }
    }
  };
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let split = buffer.indexOf("\n\n");
    while (split !== -1) {
      dispatch(buffer.slice(0, split));
      buffer = buffer.slice(split + 2);
      split = buffer.indexOf("\n\n");
    }
  }
  if (buffer.trim() !== "") dispatch(buffer);
}

// ── The change feed. One EventSource for the whole page: `dbmd api`
// streams the store's own watcher (`dbmd watch`) as SSE, so ANY writer —
// this page, an agent running dbmd, an editor save — shows up here.
// Events are structured ({event: created|modified|removed, path}), so
// subscribers get the changed paths and refetch selectively; `null` means
// "assume everything changed" (fresh connect, or a reconnect that may
// have missed events).

export type ConnectionState = "connecting" | "online" | "offline";

type ChangeListener = (paths: string[] | null) => void;
type ConnectionListener = (state: ConnectionState) => void;

let source: EventSource | undefined;
let connection: ConnectionState = "connecting";
const changeListeners = new Set<ChangeListener>();
const connectionListeners = new Set<ConnectionListener>();
let pendingPaths: Set<string> | null | undefined;
let flushTimer: number | undefined;

function setConnection(state: ConnectionState) {
  if (connection === state) return;
  connection = state;
  for (const listener of connectionListeners) listener(state);
}

function queueChange(path: string | null) {
  // null poisons the batch to "everything".
  if (path === null) pendingPaths = null;
  else if (pendingPaths !== null) (pendingPaths ??= new Set()).add(path);
  flushTimer ??= window.setTimeout(() => {
    const batch = pendingPaths === null ? null : [...(pendingPaths ?? [])];
    pendingPaths = undefined;
    flushTimer = undefined;
    for (const listener of changeListeners) listener(batch);
  }, 150);
}

function ensureSource() {
  if (source) return;
  // Always open on subscribe, even if the document currently reports
  // hidden: `visibilitychange` only fires on a TRANSITION, so a page that
  // renders while hidden (a background tab, an occluded window) would
  // otherwise sit there with no stream, no resync, and — since connection
  // state never leaves "connecting" — no offline banner either. Stale
  // forever, silently. The hidden-close below still frees the socket the
  // moment a visible tab is backgrounded, which is what the connection
  // budget actually cares about.
  source = new EventSource(`${API_BASE}/v1/events?interval=1`);
  source.onopen = () => setConnection("online");
  source.onerror = () => setConnection("offline");
  source.onmessage = (event) => {
    let parsed: { event?: string; path?: string };
    try {
      parsed = JSON.parse(event.data) as { event?: string; path?: string };
    } catch {
      queueChange(null);
      return;
    }
    // The baseline frame opens every (re)connection — an event dropped
    // during a reconnect gap is gone for good, so treat it as "anything
    // may have changed".
    if (parsed.event === "baseline" || typeof parsed.path !== "string") queueChange(null);
    else queueChange(parsed.path);
  };
}

// SSE lifecycle hygiene: a stream left open by a dead or hidden page
// generation pins one of Chrome's six per-host connections AND one of the
// server's slots — and a QUIET stream is never noticed dead by either
// side (the server only writes on changes). Close on pagehide/hidden,
// reopen with a full resync on return.
window.addEventListener("pagehide", () => {
  source?.close();
  source = undefined;
});
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "hidden") {
    source?.close();
    source = undefined;
  } else if (changeListeners.size > 0 || connectionListeners.size > 0) {
    ensureSource();
    // Anything may have changed while the stream was down.
    queueChange(null);
  }
});

export function onStoreChange(listener: ChangeListener): () => void {
  ensureSource();
  changeListeners.add(listener);
  return () => changeListeners.delete(listener);
}

export function onConnectionChange(listener: ConnectionListener): () => void {
  ensureSource();
  connectionListeners.add(listener);
  listener(connection);
  return () => connectionListeners.delete(listener);
}

/** True when a change batch touches the given store path (null = everything). */
export function batchTouches(paths: string[] | null, predicate: (path: string) => boolean): boolean {
  if (paths === null) return true;
  return paths.some(predicate);
}

// Vite dev only: when HMR replaces this module, the old instance's stream
// must die with it — otherwise every source edit leaks one live,
// auto-reconnecting EventSource, and six of those exhaust Chrome's
// per-host connection budget and starve every fetch to the API.
if (import.meta.hot) {
  import.meta.hot.dispose(() => {
    source?.close();
    source = undefined;
  });
}
