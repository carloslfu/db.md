---
type: db-md
scope: personal
owner: Carlos Galarza
---

# TodoMVC store

The database behind the TodoMVC demo app one directory up. Every todo is
one markdown record under `records/todos/`; the React app renders this
store and writes back through `dbmd`. Any other writer — you, an agent
driving `dbmd`, a plain text editor — operates on equal footing: the app
watches the folder and re-renders on change.

This store is records-only. Todos are operating state the app maintains
(`meta-type: operational`), so there is no `sources/` layer.

## Agent instructions

- A todo's text lives in `summary`; its done state lives in `status`
  (`active` | `completed`). The body is the todo's optional long
  description — plain markdown (notes, checklists); empty when the todo
  needs none.
- `priority` is optional (`low` | `medium` | `high`). The app renders a
  control for every enum field in the schema below, so adding an enum
  field here grows the UI without an app change.
- Checklists are plain `- [ ]` / `- [x]` lines in the body; the app rolls
  them up across every todo in its checklist view.
- Link between todos with store-root wiki-links in the body, e.g.
  `[[records/todos/other-todo]]` (no `.md`). `dbmd rm` refuses to delete
  a record that others still link to unless `--force`d.
- `order` is the optional manual sort rank: an integer, ascending;
  records without one sort after ranked ones, by `created`. Move a todo
  with `dbmd fm set <file> order=<int>` (pick a value between its new
  neighbors; the app keeps gaps of 100).
- Create a todo with
  `dbmd write records/todos/<slug>.md --type todo --summary "<text>" --fm status=active --fm meta-type=operational`.
- Flip state with `dbmd fm set <file> status=completed` (or `active`);
  retitle with `dbmd fm set <file> 'summary=<new text>'`.
- Set or extend the description with `dbmd body set <file> --text "<md>"`
  (or `body append`); read one back with `dbmd show <file>`.
- Delete a todo with `dbmd rm <file>` — link-aware, and the catalog
  updates write-through (no `index rebuild` needed).
- Every write you make lands in the open app live (it follows the store's
  change feed); append your own entries to the timeline with
  `dbmd log <create|update|delete> <object> -m "<note>"`.
- Run `dbmd validate --all` before you finish.

## Schemas

### todo
- status (required, enum: active, completed)
- priority (enum: low, medium, high)
- order (int)
