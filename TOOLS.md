# db.md tools

db.md is plain files. Any tool that reads files works. The reference
toolkit is **one binary** — `dbmd` — that performs every
db.md-specific file/data operation. **Zero LLM dependencies**; the
agent runtime is BYO.

## One binary, many subcommands

`dbmd` follows the git / cargo / kubectl shape: a single binary with
subcommands. It embeds ripgrep (via the `grep` + `ignore` crates) for
fast search and builds its own document extraction (`dbmd extract`),
so there are no external tools to install or license.

- **All Rust.** Built for velocity the way ripgrep is.
- **Zero AI dependencies.** No provider SDKs, no API keys, no model
  calls. `dbmd` is deterministic file/data plumbing; the agent
  reasons, `dbmd` executes. It never scaffolds, templates, or
  generates what a capable agent authors itself — there is no `dbmd
  init`, no wizards: you write `DB.md` and summaries; `dbmd` validates,
  indexes, queries, and links.
- **Permissive dependency policy.** No GPL, no AGPL, no AI SDKs, no
  vector database crates anywhere in the binary.
- **One install.** ~6MB, cross-platform (darwin / linux ×
  x86_64 / arm64).

## Why one binary, not a kit

An earlier design bundled six upstream tools (ripgrep, rga, pdfgrep,
fd, jq, git) behind a smart installer. We collapsed it to one binary:

1. **License hygiene.** rga (AGPL-3) and pdfgrep (GPL-2 + poppler)
   force a permanent compliance program — source-mirror obligations,
   enterprise license-scanner flags. Embedding ripgrep-compatible
   search and building extraction on permissively licensed Rust crates
   keeps the artifact clean to ship and audit.
2. **One thing to install.** `curl | sh` drops a single binary — no
   version resolution, no `command -v` probing, no PATH juggling
   across six tools.
3. **The model does the composition.** A capable agent composes
   `dbmd` subcommands through pipes far better than it juggles six
   differently-flavored CLIs.

## Subcommand surface

Grouped by the agent session phase (SPEC.md § The agent session).
Every subcommand supports `--json` and `--help`; none prompt
interactively. **Loop ops** (search, fm, backlinks, write, log tail,
working-set validate) are designed around the changed working set and
sidecar reads; **SWEEP ops** (`validate --all`, `index rebuild`,
`stats`, whole-graph queries) are O(store) and run off the interactive
loop. See SPEC.md § Scale.

### Open
- `dbmd spec` — print the bundled canonical SPEC. This is the
  mechanism: install `dbmd`, run `dbmd spec`, read the standard once
  per session. A capable agent needs nothing more.
  (Persistence across sessions is optional and is **not** a `dbmd`
  command: place the open-format skill `skills/db-md/SKILL.md` where your
  harness reads skills — copy it, use the harness's own skill installer
  (Codex's `skill-installer`, a Claude Code plugin), or tell your agent
  to. See § Agent bootstrap.)
- `dbmd fm get DB.md <key>` — read store identity

### Warm up
- `dbmd log tail [N]` — last N log entries (default 20; reverse-read from EOF)
- `dbmd log since <RFC3339>` — entries since a timestamp

### Read
- `dbmd search <query> [--type --in --where --linked-from --linked-to --updated-after --updated-before --created-after --created-before]` — embedded ripgrep over content + the frontmatter block; filters never parse the whole store
- `dbmd fm get <file> <key>` / `dbmd fm query <key>=<value>` — `fm query` is sidecar-backed frontmatter filtering (the pre-write dedup primitive)
- `dbmd graph backlinks|forwardlinks|neighborhood|orphans` — relationship retrieval; `orphans` is the SWEEP curation worklist
- `dbmd tree [--layer --type]`
- `dbmd outline <file>`
- `dbmd stats` — store metrics (SWEEP)
- `dbmd extract <file>` — PDF / docx / xlsx / epub / html → plain text
- `dbmd index show [<path>]`

### Write
Each write maintains the `index.md` catalog write-through (no rebuild step in the loop).
- `dbmd write <path> --type <t> [--summary --fm --body-file]` — sharded source and event types resolve to date paths (`sources/<type>/<YYYY>/<MM>/`, `records/<type>/<YYYY>/<MM>/`); flat entity types stay flat; prints the resolved path
- `dbmd fm set <file> <key>=<value>`
- `dbmd fm init <file>` — generate canonical frontmatter + default
  `summary`; the reconcile primitive for externally-dropped sources
- `dbmd link <from> <to>`
- `dbmd rename <old> <new>` — move + rewrite incoming wiki-links

### Validate
- `dbmd validate [--json]` — working-set by default (changed files
  since the last `validate` log entry, O(changed)); the single
  validation entrypoint (SPEC.md § Validation lists the codes)
- `dbmd validate --all [--json]` — full-store SWEEP (every link, every
  index, entity-dedup) — CI / recovery, not the loop

### Maintain / repair
- the catalog is maintained write-through by the write commands; no
  rebuild step in the normal loop
- `dbmd index rebuild [--layer --folder --dry-run]` — from-scratch
  repair (after a bulk external drop into `sources/`, or to recover a
  damaged index)

### Close
- `dbmd log <kind> <object> [-m <note>]` — append to the active `log.md`; auto-rotates older months into `log/<YYYY-MM>.md`

## The library: `dbmd-core`

All logic lives in `dbmd-core`, a Rust library crate; the `dbmd`
binary is thin CLI wrappers (parse args, call the library, format
output). Any Rust tool — an Obsidian plugin, a Notion exporter, an
LSP server, a custom agent harness — can `cargo add dbmd-core` and
get the full library: parser, store walk, wiki-link graph,
validation, stats, query, index/log ops. Precedent: ripgrep's
`grep` + `ignore` libs do the work; `rg` is the thin binary.

## Install

**Recommended — prebuilt binary, no toolchain** (macOS + Linux):

```bash
curl -fsSL https://raw.githubusercontent.com/carloslfu/db.md/main/scripts/install.sh | sh
```

**Alternatives**:

```bash
brew install carloslfu/tap/dbmd     # prebuilt release through the Homebrew tap
cargo install dbmd-cli              # build from crates.io with your Rust toolchain
# or download a prebuilt tarball from the GitHub releases page:
#   https://github.com/carloslfu/db.md/releases
```

Prebuilt tarballs are SHA256-checksummed and carry build-provenance
attestations (`gh attestation verify <tarball> --repo carloslfu/db.md`).

## Agent bootstrap

**The installer is text.** db.md is installed and integrated by reading
markdown and acting on it — a capable agent is the installer. There is no
per-harness machinery to depend on: the mechanism is generic text + a smart
model. The repo-root `llms.txt` is the agent-readable entry point (what db.md
is, plus how to install, integrate, and operate); the canonical path is **read
`dbmd spec` (or `llms.txt`) and act.**

```bash
# 1 — get the binary (prebuilt; brew / cargo are alternatives, same
#      release artifacts)
curl -fsSL https://raw.githubusercontent.com/carloslfu/db.md/main/scripts/install.sh | sh

# 2 — load the contract: read it once per session and act on it.
dbmd spec                                        # the single source of truth

# OPTIONAL — persist the contract so it loads every future session.
#   Still text: place the skill file, or carry the spec in a prompt.
dbmd spec > /tmp/dbmd-spec.md                    # capture the contract
# paste or load /tmp/dbmd-spec.md into your harness's system prompt
```

There is one source of truth — `dbmd spec`, which prints the SPEC. Read it (or
the repo-root `llms.txt`) and act; that is the whole mechanism. Persisting it
is optional: place a skill where your harness reads skills (the open `SKILL.md`
format — the canonical file is `skills/db-md/SKILL.md`, dropped into
`~/.claude/skills/db-md`, `~/.codex/skills/db-md`, or any other harness's skills
dir), or configure your harness to include the captured `dbmd spec` output in
the prompt. Placing the file is generic work — copy it, use your harness's own
skill installer, or tell your agent to; db.md ships no per-harness install
command. The skill body just points at `dbmd spec` (never an inlined copy, so it
cannot drift). Either way the agent has the canonical SPEC for the session —
the format, example types, curator contract, session lifecycle, the full
subcommand surface, and the validation issue-code vocabulary. Per-store
overrides come from `DB.md` on every operation.

## Status

The format (SPEC.md) is at v0.3. The single-binary all-Rust
`dbmd` described here is the active build target — treat this
document as the toolkit contract the binary implements. The
workspace is `crates/dbmd-core` (library) + `crates/dbmd-cli`
(binary); releases ship as per-platform tarballs plus a Homebrew tap
and a crates.io crate.
