# Contributing to db.md

Thanks for thinking about contributing. Three things to know up
front:

1. **License is Apache-2.0.** Everything you contribute lands
   under Apache-2.0 (with patent grant).
2. **CLA is required.** First-time PRs trigger the CLA Assistant
   bot. Sign the Apache ICLA via the comment workflow it prompts.
3. **db.md is independently usable.** It must work on any plain
   markdown folder. Any change that adds a dependency on a specific
   platform, hosted service, or account will be rejected.

## Development setup

The toolkit is a single Rust workspace. One binary (`dbmd`); all
logic lives in `crates/dbmd-core`, and `crates/dbmd-cli` is a thin
arg-parse/format wrapper.

```bash
git clone https://github.com/carloslfu/db.md.git
cd db.md

make build   # cargo build --workspace
make test    # cargo test --workspace
make lint    # cargo clippy --workspace --all-targets -- -D warnings
make fmt     # cargo fmt --all
```

Requires a recent stable Rust toolchain (see `rust-version` in
`Cargo.toml`). No other system dependencies — ripgrep and document
extraction are embedded via Rust crates, so there is nothing to
`apt install` or `brew install` to build or test.

## Project layout

```
db.md/
├── Cargo.toml          # workspace manifest (shared dep versions)
├── crates/
│   ├── dbmd-core/      # the library — parser, store, graph, validate,
│   │                   #   stats, query, index, log. All logic lives here.
│   └── dbmd-cli/       # the `dbmd` binary — thin arg-parse/format wrappers
├── examples/           # role-flavored example stores (sources/ records/ wiki/)
├── tests/corpora/      # test stores (canonical, edges, formats, scale, agent)
├── SPEC.md             # the format spec + curator contract + validation codes
└── TOOLS.md            # the toolkit reference (subcommand surface, install)
```

The split is load-bearing: **all logic lives in `dbmd-core`**, and
`dbmd-cli` only parses arguments, calls the library, and formats
output. New behavior goes in the library with a test; the CLI grows a
thin wrapper around it. (Precedent: ripgrep's `grep` + `ignore` libs
do the work; `rg` is the thin binary.)

## What to work on

- **Bugs**: file an issue first, then PR with a test.
- **Spec extensions**: additive only — propose a new optional
  `type:` or frontmatter field, not a change to existing semantics.
- **`dbmd` subcommands**: deterministic file/data operations on a
  store. New work lands in `crates/dbmd-core` (the library); the CLI
  stays a thin wrapper.
- **New example stores**: the [`examples/`](examples/) directory
  should grow over time.
- **Documentation**: README, SPEC.md, code comments.

## What we won't merge

- **Schema lock-in.** db.md is intentionally a primitive, not a
  product. Custom `type:` values must pass through; nothing in
  the spec or parser should reject them.
- **Platform-specific assumptions.** db.md must be usable on a
  plain Obsidian vault, a research project, or anyone's markdown
  folder — without any account or hosted service.
- **Heavy dependencies.** Keep the dependency tree small, and
  record every new crate plus its license in
  [`THIRD_PARTY_NOTICES`](THIRD_PARTY_NOTICES). Licenses must be
  MIT / Apache-2.0 / BSD / Unlicense / MPL — no GPL/AGPL/LGPL-static.
- **AI/LLM dependencies.** `dbmd` is deterministic and ships zero
  AI: no provider SDKs, no API keys, no model calls, and no
  embeddings / vectors / ANN — ever. The user's own agent harness
  does all the intelligence; `dbmd` is the dumb, fast tool it drives.

## Style

- **Code comments**: only when the WHY is non-obvious.
- **Commit messages**: imperative voice, under 72 chars on the
  subject line.
- **PR titles**: short and concrete.
- **Tests**: every behavior change needs a test.
- **Changelog**: user-facing changes get an entry under the
  `## [Unreleased]` heading in [CHANGELOG.md](CHANGELOG.md)
  (Keep a Changelog format).

## Pre-PR checklist

- [ ] Tests pass (`make test`)
- [ ] Lint clean (`make lint`) and formatted (`cargo fmt --all`)
- [ ] No secrets in code, tests, or fixtures
- [ ] `SPDX-License-Identifier: Apache-2.0` header on any new
      source file
- [ ] CLA signed via the bot on your first PR

## Reporting security issues

**Do not file public issues for security problems.** Use GitHub's
"Report a vulnerability" button on the repository's Security tab. See
[SECURITY.md](SECURITY.md).

## Code of conduct

Be kind. We follow the
[Contributor Covenant 2.1](https://www.contributor-covenant.org/version/2/1/code_of_conduct/).
