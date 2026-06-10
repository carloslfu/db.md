# dbmd-cli

The **`dbmd`** command-line tool for **db.md, the open standard for
databases in plain files**.

`db.md` is a database made of markdown files: records are markdown with
YAML frontmatter, relationships are wiki-links, the directory is the
database, the frontmatter is the schema, and your agent is the query
engine. For the broad middle of agent-written software, the old
Postgres/migrations/CRUD layer becomes files, frontmatter, wiki-links, and a
curator contract. `dbmd` is a single deterministic binary that performs every
db.md file/data operation; all logic lives in
[`dbmd-core`](https://crates.io/crates/dbmd-core) and the binary is a
thin arg-parse/format wrapper.

## Install

```sh
cargo install dbmd-cli
```

This installs the `dbmd` binary. Alternatives: `brew install
carloslfu/tap/dbmd`, or a prebuilt, checksummed, provenance-attested
tarball from the [GitHub releases](https://github.com/carloslfu/db.md/releases).

## Use

```sh
# create a store — you write DB.md (the agent authors it; there is no `dbmd init`)
mkdir -p db/{sources,records,wiki}
printf -- '---\ntype: db-md\nscope: personal\nowner: me\n---\n' > db/DB.md

# operate it
dbmd write db/records/contacts/sarah.md --type contact --summary "..."
dbmd search "(revenue|sales|ARR)" --in records
dbmd graph backlinks db/records/contacts/sarah.md
dbmd validate db/

# load the canonical contract — the single source of truth, read once per session
dbmd spec
```

To persist the contract across sessions, drop the open-format skill
`skills/db-md/SKILL.md` where your harness reads skills (copy it, use your
harness's own skill installer, or tell your agent) — there is no `dbmd`
install command; the installer is text.

Every subcommand supports `--json` and `--help`; none prompt
interactively.

## Design

- **Zero AI dependencies.** No model calls, no embeddings, no vectors — ever. Your own agent harness (Claude Code, Codex, or any tool) supplies the intelligence; `dbmd` is the fast, deterministic tool it drives.
- **Embedded ripgrep**; built-in document extraction; permissive licensing only.

Full reference and SPEC: run `dbmd spec`, or see
<https://github.com/carloslfu/db.md>.

## License

Apache-2.0. Copyright 2026 Carlos Galarza. See `LICENSE` and `NOTICE`.
