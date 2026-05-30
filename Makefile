# SPDX-License-Identifier: Apache-2.0
#
# Build + test the db.md toolkit. One binary: `dbmd`.
#
# All logic lives in `crates/dbmd-core`; `crates/dbmd-cli` is a thin
# arg-parse/format wrapper that produces the single `dbmd` binary.
# Ripgrep is embedded via the `grep` + `ignore` crates — never a bundled
# `rg` binary, never a separate ingester/curator binary.

.PHONY: build release test fmt fmt-check lint publish-check clean

# Debug build of the whole workspace -> target/debug/dbmd
build:
	cargo build --workspace

# Optimized build (LTO, stripped) -> target/release/dbmd
release:
	cargo build --workspace --release

test:
	cargo test --workspace

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

lint:
	cargo clippy --workspace --all-targets -- -D warnings

# Publishability guard. Packages every crate from its TARBALL (the form
# `cargo publish` ships) so an include_str!/include_bytes! path that escapes
# the crate, a path-only dep missing a version, or missing publish metadata
# fails HERE, not at publish. Mirrors CI (.github/workflows/publish-check.yml).
# Run this before any `cargo publish`.
publish-check:
	cargo package --workspace --locked

clean:
	cargo clean
