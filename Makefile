# SPDX-License-Identifier: Apache-2.0
#
# Build + test the db.md toolkit. One binary: `dbmd`.
#
# All logic lives in `crates/dbmd-core`; `crates/dbmd-cli` is a thin
# arg-parse/format wrapper that produces the single `dbmd` binary.
# Ripgrep is embedded via the `grep` + `ignore` crates — never a bundled
# `rg` binary, never a separate ingester/curator binary.

.PHONY: build release test fmt fmt-check lint publish-check sync clean

# Single source of truth: the repo-root SPEC.md and skills/ are authoritative.
# The `dbmd` binary embeds them via include_str!, but cargo package requires the
# embedded path to stay INSIDE the crate — so we mirror them into crates/dbmd-cli
# and embed the mirror. `make sync` regenerates the mirror; the
# `bundled_assets_match_repo_root` test fails if anyone edits a root source and
# forgets to re-sync. Edit the root files, never the mirrors.
sync:
	cp SPEC.md crates/dbmd-cli/SPEC.md
	mkdir -p crates/dbmd-cli/skills/db-md
	cp skills/db-md/SKILL.md crates/dbmd-cli/skills/db-md/SKILL.md

# Debug build of the whole workspace -> target/debug/dbmd
build: sync
	cargo build --workspace --locked

# Optimized build (LTO, stripped) -> target/release/dbmd
release: sync
	cargo build --workspace --release --locked

test:
	cargo test --workspace --locked

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

lint:
	cargo clippy --workspace --all-targets --locked -- -D warnings

# Publishability guard. Packages every crate from its TARBALL (the form
# `cargo publish` ships) so an include_str!/include_bytes! path that escapes
# the crate, a path-only dep missing a version, or missing publish metadata
# fails HERE, not at publish. Mirrors CI (.github/workflows/publish-check.yml).
# Run this before any `cargo publish`.
publish-check: sync
	cargo package --workspace --locked

clean:
	cargo clean
