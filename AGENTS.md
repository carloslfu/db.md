# AGENTS.md — db.md repository

Guidance for AI agents working in this repo. (Human contributors: see
[CONTRIBUTING.md](CONTRIBUTING.md).)

## What this is

`db.md` is **the open standard for databases in plain files**: the spec
plus its reference toolkit. The toolkit is one Rust binary, `dbmd`, in a Cargo
workspace:

- `crates/dbmd-core` — the library: all logic (parser, store, graph, validate,
  query, index, log, summary, stats, render, extract; plus `linkmd`, the
  link.md client — feature `link`, default-on).
- `crates/dbmd-cli` — the thin `dbmd` binary that wraps `dbmd-core`.
- `SPEC.md` — the canonical format + curator contract (single source of truth).
- `tests/corpora/` — frozen test stores. `examples/` — small worked stores.

## Hard rules (do not violate)

- **Zero AI/LLM dependencies.** No provider SDKs, API keys, model calls,
  embeddings, vectors, or ANN — anywhere, ever. The agent driving `dbmd` is the
  intelligence; `dbmd` is a deterministic tool.
- **All logic in `dbmd-core`; `dbmd-cli` is thin wrappers.**
- **Embedded ripgrep** via the `grep` + `ignore` crates — never bundle or shell
  out to `rg`.
- **Permissive dependency licenses only** (MIT / Apache-2.0 / BSD / 0BSD /
  Unlicense / MPL-2.0 / Zlib / Unicode-3.0 / ISC / CDLA-Permissive-2.0);
  record every new dep in
  `THIRD_PARTY_NOTICES`. Enforced by `deny.toml` plus
  `crates/dbmd-cli/tests/license_policy.rs`.
- **Interactive-loop ops avoid whole-store walks.** Use sidecars, changed sets,
  and bounded type-folder work on the loop path; full `Store::walk` belongs in
  sweep paths (`validate --all`, `index rebuild`, `stats`) or explicit repair.
- **Standalone by design.** db.md must not require any platform, account, or
  runtime outside a folder of files plus the toolkit. Comparative references to
  adjacent standards are fine; dependencies on them are not. The link.md
  client verbs (`resolve` / `sync` / `grant` / `propose` / `subscribe`) are
  optional capabilities against a hub the USER configures — no default
  endpoint is baked in, no store operation requires them, and the toolkit
  never phones home on its own (no telemetry, no auto-update). Copyright is
  Carlos Galarza.

## Build / test / checks

```sh
make build         # cargo build --workspace
make test          # cargo test --workspace
make lint          # cargo clippy --workspace --all-targets -- -D warnings
make fmt-check     # cargo fmt --all --check
make publish-check # cargo package --workspace --locked  (run before any release)
```

CI (`.github/workflows/`): `test.yml` (fmt/build/clippy/test), `publish-check.yml`
(packaging guard), `release.yml` (build + publish on a tag), `smoke.yml`,
`licenses.yml`, `cla.yml`.

## Releasing

Bump the version, push `main`, push a `vX.Y.Z` tag. The tag publishes via CI
(Trusted Publishing / OIDC, no token; approval only if required reviewers are
configured on the GitHub environment). **Full procedure and
the files to bump: [RELEASING.md](RELEASING.md).** Do not try to `cargo publish`
by hand — the release flow is the tag.

## Conventions

- Every new source file gets `SPDX-License-Identifier: Apache-2.0`.
- User-facing changes get a `## [Unreleased]` entry in `CHANGELOG.md`.
- Any change to the validation-code vocabulary (adding OR removing a code) must
  update `SPEC.md § Validation`, the `codes` module, the
  `tests/corpora/corpus-b-edges` seeding fixtures, and `EXPECTED/coverage.json`
  in the same change — the corpus-b coverage test enforces SPEC↔fixture parity
  both directions, so a half-done change turns CI red.
- Wiki-links in stores are full store-relative paths; short form is a validation
  error.
