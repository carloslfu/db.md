# db.md tools

db.md is plain files. Any tool that reads files works. The reference
toolkit is **one binary** — `dbmd` — that performs every
db.md-specific file/data operation. **Zero LLM dependencies**; the
agent runtime is BYO. A minimal embedded harness (`dbmd ask` / `do` /
`build`) covers the case where there is no agent to bring: it drives
the same verbs with **your own** model. It is a scoped fallback, not a
coding agent, and the toolkit still ships no SDK and no vendor.

## One binary, many subcommands

`dbmd` follows the git / cargo / kubectl shape: a single binary with
subcommands. It embeds ripgrep (via the `grep` + `ignore` crates) for
fast search and builds its own document extraction (`dbmd extract`),
so there are no external tools to install or license.

- **All Rust.** Built for velocity the way ripgrep is.
- **Zero AI dependencies.** No provider SDK crates, no bundled model,
  no vendor default. Every `dbmd` verb is deterministic file/data
  plumbing; the agent reasons, `dbmd` executes. It never scaffolds,
  templates, or generates what a capable agent authors itself — there
  is no `dbmd init`, no wizards: you write `DB.md` and summaries;
  `dbmd` validates, indexes, queries, and links. The embedded harness
  (`ask` / `do` / `build`) adds a *client* for user-supplied
  intelligence — a stateless loop that drives the same verbs with a
  model **you** configure (a key in your environment, or a local
  server) over raw wire protocols; nothing in the binary calls a model
  on its own.
- **Permissive dependency policy.** No GPL, no AGPL, no AI SDKs, no
  vector database crates anywhere in the binary.
- **One install.** One static binary, cross-platform (darwin / linux ×
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
   differently-flavored CLIs. The embedded harness does not change
   this: `ask` / `do` / `build` exist for callers that cannot host a
   harness — an app process calling `/v1/ask`, a machine with a model
   but no agent installed — and their loop composes exactly the verbs
   an external agent would, through the same binary.

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
- `dbmd query [--type --in --where <k>=<v> --updated/created-after/-before --limit]` — sidecar-backed frontmatter filtering (the pre-write dedup primitive; `--where id=<id>` is the id lookup)
- `dbmd show <file>` — one file as its full structured record: parsed
  frontmatter, derived fields, verbatim body, link spans, file-bytes
  SHA-256 — the random-access, single-file form of `dbmd emit` (equal
  under `--json` to that dump's entry for the file). O(one file), loop-safe
- `dbmd fm get <file> <key>` — read one frontmatter key
- `dbmd section get <file> <heading>` — print one section verbatim (heading
  line + content, deeper sub-sections included); store-free, any markdown file
- `dbmd schema [<type>]` — the store's declared `DB.md ## Schemas` contracts,
  parsed: fields with modifiers, `unique:` keys, `summary_template`, `shard`.
  The introspection twin of validate's enforcement — an app renders forms
  from this instead of re-parsing `DB.md`
- `dbmd watch [--path <prefix>] [--interval <secs>]` — the local change
  feed: poll the emit membership and print one event line per created /
  modified / removed content file (NDJSON under `--json`, a `baseline` line
  first) — the local-filesystem sibling of `subscribe`. Dependency-free
  polling, no locks (a watcher never blocks a writer); each tick re-stats
  the membership, so scope very large stores with `--path`
- `dbmd graph backlinks|forwardlinks|neighborhood|orphans` — relationship retrieval; `orphans` is the SWEEP curation worklist
- `dbmd tree [--layer --type]`
- `dbmd outline <file>`
- `dbmd stats` — store metrics (SWEEP)
- `dbmd extract <file>` — PDF / docx / xlsx / epub / html → plain text
- `dbmd index show [<path>]`
- `dbmd emit` — the whole-store structured dump (SWEEP; read-only):
  every content file plus `DB.md` as one JSON document under `--json`
  (parsed frontmatter with values verbatim, derived
  layer/type/meta-type/title/summary/timestamps, verbatim body,
  normalized wiki-link targets, file-bytes SHA-256), so a host — a
  hub, an indexer, a migration — ingests a store as a pure consumer of
  `dbmd` output instead of reimplementing the parse; text mode prints
  the would-be-emitted paths. Each file also carries `link_spans`: every
  wiki-link occurrence in the body, in order, with the byte range it
  covers — the positional view a RENDERER needs, so rewriting `[[…]]`
  into markup is a splice at an offset rather than a second
  implementation of bracket scanning and fence tracking. `--ndjson`
  streams the same contract: one compact JSON object per line — exactly
  the `--json` `files[]` element shape, same membership and order, no
  envelope — projected, printed, and dropped one file at a time, so
  neither `dbmd` nor a line-reading consumer ever holds the whole dump
  (the large-store ingestion mode).

### Write
Each write maintains the `index.md` catalog write-through (no rebuild step in the loop).
- `dbmd write <path> --type <t> [--summary --fm --body-file]` — sharded source and event types resolve to date paths (`sources/<type>/<YYYY>/<MM>/`, `records/<type>/<YYYY>/<MM>/`); flat entity types stay flat; prints the resolved path. Mints a stable lowercase-ULID `id` when none is supplied (`--fm id=…` wins; recommended, not required — SPEC § The `id` field)
- `dbmd fm set <file> <key>=<value>`
- `dbmd fm init <file>` — generate canonical frontmatter + default
  `summary`; the reconcile primitive for externally-dropped sources.
  Never mints an `id` — adding ids to existing files is the agent's
  call (SPEC § The `id` field)
- `dbmd body set|append <file> (--text | --body-file <path|->)` — replace or
  extend the body (everything after the frontmatter). `updated` re-stamps,
  frozen pages refuse, the indexes update write-through; `summary` is never
  recomputed (the catalog line stays the agent's explicit judgment)
- `dbmd section set|append <file> <heading> (--text | --body-file <path|->)
  [--create --level <2-6>]` — section-addressed body edit: exact heading
  match, span = heading → next sibling-or-shallower heading (fence-aware;
  H1 terminates), so `set` replaces the whole subtree; `--create` upserts a
  missing section; duplicate headings refuse as `SECTION_AMBIGUOUS`
- `dbmd link <from> <to>`
- `dbmd rename <old> <new>` — move + rewrite incoming wiki-links
- `dbmd rm <path> [--force]` — link-aware delete of one content file:
  refuses while other content files still wiki-link to it (`RM_LINKED`,
  backlinks listed), `--force` deletes anyway; reserved meta files and
  frozen pages never deletable; the catalog row drops write-through (no
  `index rebuild` needed)
- `dbmd format <file>` — re-emit frontmatter + body canonically (key
  order, YAML style, whitespace); writes back in place

### Validate
- `dbmd validate [--json]` — working-set by default (changed files
  since the last `validate` log entry, O(changed)); the single
  validation entrypoint (SPEC.md § Validation lists the codes)
- `dbmd validate --all [--json]` — full-store SWEEP (every link, every
  index, entity-dedup) — CI / recovery, not the loop
- `dbmd validate --all --projection-excludes <file> [--json]` — full SWEEP of
  an intentionally partial projection; only missing wiki-link targets matched
  by the bounded `.sevralocal`-compatible policy become explicit unresolved info, and
  every unlisted or unrelated error still fails
- `dbmd validate --all --projection-manifest <file|-> [--json]` — the same
  explicit partial-view proof from sorted domain-separated SHA-256 path
  commitments; `-` reads bounded stdin, so a signed package can retain its
  private source policy

### Maintain / repair
- the catalog is maintained write-through by the write commands; no
  rebuild step in the normal loop
- `dbmd index rebuild [--layer --folder --dry-run]` — from-scratch
  repair (after a bulk external drop into `sources/`, or to recover a
  damaged index)

### Assets
- `dbmd assets scan|refresh|refresh-wrapper|verify|status|paths` — catalog,
  incrementally reconcile one asset or one wrapper's complete current set,
  verify, and report
  raw binary assets a wrapper declares (`asset:`/`assets:` frontmatter)
  but Git should not carry; maintains the root `assets.jsonl` manifest,
  never transports bytes (SPEC § Assets)
- `dbmd assets verify --projection-excludes <file>` — verify a declared partial
  projection: matched absence stays visible as `projected_missing`, while
  `complete` stays false and `projection_complete` proves only the materialized
  view; present corruption and every undeclared missing asset still fail
- `dbmd assets verify --projection-manifest <file|->` — the same byte gate from
  a canonical path-commitment manifest, with bounded stdin supported by `-`

### Close
- `dbmd log <kind> <object> [-m <note>]` — append to the active `log.md`; auto-rotates older months into `log/<YYYY-MM>.md`

### The local app API
- `dbmd api [--addr 127.0.0.1:3263] [--dir]` — serve the store's full local
  verb surface over loopback HTTP, so an application (a browser page
  included — CORS is open) uses the store as its backend without shelling
  out. ONE SEMANTICS by construction: every route executes the same-named
  `dbmd` verb (same binary, `--json` output passed through verbatim, the
  same frozen-page policy, index write-through, and store transaction lock
  — concurrent HTTP writes serialize correctly), so the API can never
  drift from the CLI. Exit codes map onto HTTP statuses (0→200, 1/2→400,
  3→404, 4→403, 5→409, 6→422) with the structured `{"error":{…}}` line as
  the error body. `GET /v1` lists every route; `GET /v1/events` streams
  `dbmd watch` as Server-Sent Events; `GET /v1/emit?ndjson=1` line-streams
  the whole-store dump. Loopback only, deliberately without a public-bind
  escape hatch (an unauthenticated read-write surface for the machine's
  own apps); cross-party verbs (sync, grants, proposals, keys, mirror) are
  not exposed — link.md remains the cross-party surface.

### The embedded harness (ask / do / build)

One engine, three tool masks — a stateless tool-calling loop that runs
**your own** model against the store's verb surface. Every tool call executes as a `dbmd` verb (the api's ONE
SEMANTICS rule: same binary, same schema enforcement, frozen pages,
per-call transaction lock, write-through indexes, log.md), and there is
no shell tool at any mask.

**Scope, stated plainly.** This is the fallback operator for callers
that cannot host a real one: an app calling the store, or a machine
with a model and no agent installed. It is not a coding agent and does
not aim to be. With no shell it cannot install a dependency, run a
build, run tests, or verify its own work; a BYO agent (Claude Code,
Codex, pi) does that, and remains the primary and better path. Keep it
small: sessions, memory, subagents, and plugins belong to real
harnesses, not here.

- `dbmd ask "<question>"` — READ verbs only (query, search, show,
  schema, tree, log tail). Guaranteed no mutation: on untrusted content
  a prompt injection can at worst produce a wrong answer.
- `dbmd do "<request>"` — adds the store WRITE verbs (write, fm set,
  body set, rm — link-aware, never forced — and log; the store log is
  the audit trail of what the model did).
- `dbmd build "<request>"` — adds file tools (list/read/write/edit)
  confined beneath a DECLARED workspace root (`--workspace`,
  `DBMD_WORKSPACE`, or `workspace = <path>` in `.dbmd/config`). The
  store subtree is refused for file tools; symlink and `..` escapes are
  refused; CLI-only — never exposed over `dbmd api`. It edits source and
  stops there: a running dev server picks the change up, and anything
  that needs a command (installs, builds, tests) is the BYO agent's job.

Providers, in four layers. (1) Presets — `--provider anthropic |
openai | openrouter | groq | together | deepseek | mistral | ollama |
lmstudio | llamacpp` — each just a base URL + wire protocol + the
provider's conventional key env var. Two hand-rolled protocols cover
them all: OpenAI-compatible Chat Completions and Anthropic Messages
(which also reaches llama.cpp's native `/v1/messages`). (2) Local,
zero-config: with nothing configured the well-known local servers
(Ollama, LM Studio, llama.cpp) are autodetected and their models
listed. (3) A ChatGPT subscription, natively: `dbmd login codex` runs
OpenAI's public PKCE flow (the one endorsed for third-party OSS
clients), stores the tokens in the toolkit state dir at 0600 — never
in a store — refreshes them automatically, and `--provider codex`
then spends them against the ChatGPT backend's Responses API. No
vendor CLI needed. (4) An Anthropic session, through Anthropic's own
CLI: `dbmd login anthropic` runs `ant auth login`, and thereafter an
Anthropic endpoint with no API key in the environment asks `ant auth
print-credentials --access-token` for a fresh short-lived token and
sends it as `Authorization: Bearer` plus the required
`anthropic-beta: oauth-2025-04-20`. That is the vendor's published
handoff for third-party HTTP clients; the profile stays owned by `ant`,
nothing is copied into this toolkit, and an explicit
`ANTHROPIC_API_KEY` still wins. (5) Every other subscription by
DELEGATION — `--provider claude-code` / `codex-cli` spawns the
vendor's own logged-in CLI headless (`claude -p` / `codex exec`,
read-only sandbox for `ask`); experimental, since headless flags drift
across vendor releases.

**The identity line.** dbmd always identifies itself honestly:
`originator: dbmd` and a `dbmd/<version>` User-Agent on every
subscription request. It uses a vendor's own flow only where
that flow is published for third-party clients — OpenAI's public PKCE
client id for ChatGPT, Anthropic's documented `ant` token handoff for
Anthropic — and it will not implement one that requires posing as a
vendor's first-party client. The rejected shape is specific: the
*other* Anthropic OAuth path, and Copilot's, work only by borrowing
that vendor's client id and asserting its identity (a "You are Claude
Code" system block, a `vscode-chat` integration id). Those are refused;
Copilot is reached by delegation instead. Using your subscription is
your right; pretending to be someone else's software is not something
this toolkit does.

Config: flag > `DBMD_LLM_*` env > non-secret `llm_*` keys in
`.dbmd/config` (`llm_provider`, `llm_base_url`, `llm_protocol`,
`llm_model`, `llm_effort`; a `<key>_<provider>` suffix pins a value to
one provider and survives an explicit `--provider`). **The key is environment-only** (`DBMD_LLM_KEY`, or the
preset's conventional variable) — never a file in the store — and an
endpoint selected by store-local config refuses an ambient key unless
`DBMD_LLM_KEY_ORIGIN` binds it to that exact origin, so a cloned store
cannot exfiltrate a credential (the hub client's rule, applied to
models). Plain http is loopback-only unless
`DBMD_LLM_ALLOW_INSECURE_HTTP=1`. No default vendor or endpoint,
anywhere.

Reasoning effort: `--effort off | minimal | low | medium | high | xhigh
| max` (also `DBMD_LLM_EFFORT`, `llm_effort`). One ladder, translated
per protocol — `reasoning.effort` + `summary: "auto"` on the ChatGPT
Responses backend, `output_config.effort` alongside adaptive thinking
on Anthropic, `reasoning_effort` on OpenAI-compatible servers — and
each vendor's own vocabulary underneath, probed against the live
endpoints rather than assumed: the ChatGPT backend takes
`none|low|medium|high|xhigh|max` and has no `minimal`; Ollama 0.32.15
takes a superset of the whole ladder, so every rung passes through by
name; OpenAI proper stops at `high`, so the top rungs collapse onto it
instead of erroring. A server's own
validator is not the last word: the model's chat template runs after
it and can reject a value the server accepted, which arrives as a 500
rather than a 400 (Ollama takes `xhigh`, then Qwen3.8's template
raises). Both shapes of refusal degrade the same way — a 500 counts
only when the body names the parameter, so a real outage still fails
loudly. If the endpoint refuses the field anyway, the request is
retried without it (Anthropic tries a
legacy `thinking.budget_tokens` shape first, for models older than
4.6), and a `notice` event says so — a downgraded run never looks like
a clean one. That negotiation is remembered for the rest of the run, so
a server with no support for the field (LM Studio today) costs one
rejected round-trip, not one per turn. `off` sends `none`, the value
llama.cpp and vLLM document as disabling reasoning — not `minimal`,
which is a short think. **Unset is not a level**: with no effort configured no
reasoning field is sent at all, which matters because the defaults
differ sharply (Ollama enables thinking for capable models on its own,
and Qwen3.8 defaults to its top rung).

Caps: `--max-turns` (default 15; the capped final call carries no tools
and answers with what it has), `--max-tokens` per call, per-tool result
truncation with explicit re-query markers. Stateless one-shot: nothing
persists; `--json` streams the flat event feed (`text_delta`,
`tool_call` with the exact CLI one-liner it executes, `tool_result`,
`usage`, `notice`, `done`) as NDJSON. Over HTTP the same feed is `POST /v1/ask`
and `POST /v1/do` on `dbmd api` — SSE, each opt-in via `dbmd api
--ask` / `--do` and off by default.

### Interconnect (the link.md client)

One binary, two specs: `dbmd` also speaks the link.md client verbs
against a hub — a server that hosts, indexes, and serves db.md stores.
The db.md FORMAT is untouched (SPEC.md reserves only the `@brain/id`
address *shape*); these are client capabilities, never store
requirements. No hub is baked in: the hub URL comes from `--hub`, the
`DBMD_HUB_URL` env var, or a `hub = <URL>` line in the store-local
`.dbmd/config` (precedence in that order); the credential is the
`DBMD_HUB_KEY` env var, never a file in the store. When a store-local config
selects the hub, an ambient bearer, agent key, or brain key is used only if
`DBMD_HUB_CREDENTIAL_ORIGIN` exactly binds it to that origin. Identity pins and
monotonic feed checkpoints live in the user's global dbmd state directory,
never under the store. Non-HTTPS hubs are refused (loopback exempt). Zero AI,
zero telemetry: network I/O happens only when a verb is explicitly invoked.

- `dbmd resolve @brain[/<id>]` — a bare `@brain` returns the brain card
  (metadata + index stats); `@brain/<record-id>` (the reserved address
  shape; a `@brain/<store-path>.md` form also works) returns the full
  record, frontmatter + body
- `dbmd sync @brain [--out DIR]` — reconcile the granted slice as plain
  files. Permissioned v2 pulls verify incremental manifests and install them
  atomically; v2 pushes send only changed operations/blobs with exact
  preconditions and baseline-safe deletes. A scoped/self-custodied writer is
  queued as a proposal and reports `proposal_pending` without advancing the
  local baseline. Canonical `log.md` history rides sync; derived `index.*`
  catalogs rebuild locally. Typed post-commit projection lag waits in place;
  an exact local/head match can recover a lost-response baseline without force.
  Stable Unix checkouts reuse a private racy-clean-safe stat/hash/link cache;
  policy changes or ambiguous metadata fall back to full reads.
  Cold pulls stage proof-bound blobs in a private resumable content cache and
  fetch independent ≤8 MiB windows with at most four workers; a retry reuses
  exact hashes and still publishes only after complete verification.
  `dbmd sync <canonical-brain-id> relocate --from <old-store> --to <new-store>`
  is the explicit local-only handoff for an atomically moved checkout: it
  verifies the new store against the old path-bound baseline and moves that
  private baseline without overwriting another checkout's state.
  A mixed local `DB.md` + content/asset delta is automatically committed in
  two exact-head phases: the sole contract first, then a freshly planned
  remainder. Lost-response retries also wait through typed validation recovery
  until the same content-derived mutation receipt is available.
  Conflict bundles bound optional historical-body retrieval and keep asset
  resolution scoped to explicitly reviewed owning wrappers. Legacy v1 hubs
  retain whole-snapshot push behavior.
- `dbmd grant issue|list|revoke` — the capability model, owner-side:
  grant read or write to a principal (by email in v0), scoped to an
  optional store-path prefix, with an optional `--until` expiry
- `dbmd propose <site> --app <slug> --body/--body-file` — write without
  trust: submit evidence to a published site's inbox; it lands in the
  owner's `sources/inbox/` for their curator to accept or reject
  (unauthenticated by design)
- `dbmd proposal list|show|accept|reject @brain …` — operate the permissioned
  encrypted change queue. Show/accept independently verify the signed
  submission claim, canonical descriptor, pinned hub signer, blob declarations,
  and origin-bound endpoints. Exact acceptance rechecks and downloads only the
  declared changed blobs; self-custodied acceptance signs the complete verified
  candidate with `DBMD_BRAIN_KEY_FILE`.
- `dbmd subscribe @brain [--once] [--since N] [--interval S]` — follow
  the brain's feed head; emits one event line per advance (NDJSON under
  `--json`), `--once` for a single head read

## The library: `dbmd-core`

All logic lives in `dbmd-core`, a Rust library crate; the `dbmd`
binary is thin CLI wrappers (parse args, call the library, format
output). Any Rust tool — an Obsidian plugin, a Notion exporter, an
LSP server, a custom agent harness — can `cargo add dbmd-core` and
get the full library: parser, store walk, wiki-link graph,
validation, stats, query, index/log ops, and the link.md client
(`linkmd`, cargo feature `link`, default-on — a format-only consumer
drops it and its HTTP/TLS closure with `default-features = false`).
Precedent: ripgrep's `grep` + `ignore` libs do the work; `rg` is the
thin binary.

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
`dbmd spec` (or `llms.txt`) and act.** (For a machine with a model but no
agent, the embedded harness is the fallback operator: `dbmd ask` / `do` run
the same verb surface with a model you point them at — see "The embedded
harness" below. External agents remain the primary residents; the harness is
the doorbell, not the tenant.)

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

The format (SPEC.md) is at v0.4; the toolkit versions independently
(see the [CHANGELOG](CHANGELOG.md) for both axes and the current number).
The single-binary all-Rust
`dbmd` described here is the active build target — treat this
document as the toolkit contract the binary implements. The
workspace is `crates/dbmd-core` (library) + `crates/dbmd-cli`
(binary); releases ship as per-platform tarballs plus a Homebrew tap
and a crates.io crate.
