# db.md store operations

Your working directory is a db.md store (a `DB.md` file sits at its root). It
is the database of the TodoMVC demo app one directory up. Operate the store
with the `dbmd` CLI through bash; do not edit record files by hand unless a
dbmd verb cannot do the job.

Every todo is one markdown record in `records/todos/`. The todo's text lives
in the frontmatter field `summary`; its state lives in `status`
(`active` | `completed`). The record body below the frontmatter is the
todo's optional long description — plain markdown (notes, checklists). Edit
a body by rewriting the file's content below the frontmatter (keep the
frontmatter intact), then run `dbmd format <file>` and bump `updated` with
`dbmd fm set <file> updated=<RFC3339 now>`. The optional `order` field is
the manual sort rank (integer, ascending; absent sorts last by `created`) —
reorder with `dbmd fm set <file> order=<int>` between the new neighbors.

Playbook — list first, then act on exact paths:

- List todos: `dbmd query --type todo --json`
- Create: `dbmd write records/todos/<slug>.md --type todo --summary "<text>" --fm status=active --fm meta-type=operational`
- Retitle: `dbmd fm set records/todos/<file>.md 'summary=<new text>'`
- Complete / reopen: `dbmd fm set records/todos/<file>.md status=completed` (or `status=active`)
- Delete: `rm records/todos/<file>.md && dbmd index rebuild`
- Log what you did: `dbmd log update records/todos/<file>.md -m '<short note>'`
- Before finishing: `dbmd validate --all` and confirm 0 errors.

The TodoMVC app live-reloads from this store, so every change you make shows
up in the open app immediately. Keep operations minimal and exact; when the
task names a todo loosely, list first and match it to the right record.
