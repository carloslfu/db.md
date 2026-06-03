# AGENTS.md — db.md repository

Guidance for AI agents working in this repo. (Human contributors: see
[CONTRIBUTING.md](CONTRIBUTING.md).)

## What this is

`db.md` is **the open database in plain files** — a standard plus its
reference toolkit. The toolkit is one Rust binary, `dbmd`, in a Cargo
workspace:

- `crates/dbmd-core` — the library: all logic (parser, store, graph, validate,
  query, index, log, summary, stats, render, extract).
- `crates/dbmd-cli` — the thin `dbmd` binary that wraps `dbmd-core`.
- `SPEC.md` — the canonical format + curator contract (single source of truth).
- `tests/corpora/` — frozen test stores. `db/` — the dogfood store.

## Hard rules (do not violate)

- **Zero AI/LLM dependencies.** No provider SDKs, API keys, model calls,
  embeddings, vectors, or ANN — anywhere, ever. The agent driving `dbmd` is the
  intelligence; `dbmd` is a deterministic tool.
- **All logic in `dbmd-core`; `dbmd-cli` is thin wrappers.**
- **Embedded ripgrep** via the `grep` + `ignore` crates — never bundle or shell
  out to `rg`.
- **Permissive dependency licenses only** (MIT / Apache-2.0 / BSD / Unlicense /
  MPL); record every new dep in `THIRD_PARTY_NOTICES`. Enforced by
  `crates/dbmd-cli/tests/license_policy.rs`.
- **Interactive-loop ops are O(changed), never O(store)** — no full `Store::walk`
  on the loop path (walks only in `validate --all`, `index rebuild`, `stats`).
- **Self-contained.** db.md references no other project/platform. Copyright is
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

Bump the version, push `main`, push a `vX.Y.Z` tag. The tag auto-publishes via
CI (Trusted Publishing / OIDC, no token, no approval gate). **Full procedure and
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
