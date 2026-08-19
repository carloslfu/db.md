# Releasing db.md

How to cut a new release of the `dbmd` toolkit (`dbmd-core` + `dbmd-cli`).
Written so an agent or a human can run it cold.

## TL;DR

Bump the version and push `main`, then run the release controller. It creates
or resumes the exact tag/run, independently rebuilds all four release targets
and byte-compares their binaries before approving publication, then converges
crates.io, the immutable GitHub release, Homebrew, and finally `latest`.

```sh
# 1. bump version (see "Files to bump" below), then:
git add -A && git commit -m "release: X.Y.Z — <one line>"
git push origin main          # runs CI checks only — does NOT publish

# 2. authorize and cut the release
scripts/release.sh X.Y.Z
```

Do not push the tag by hand. Direct tag pushes can build, but the protected
`release-publishing` environment keeps publication waiting until the local
controller has reviewed the independent rebuild.

## What is automatic vs. manual

| Step | Who |
|---|---|
| Version bump + changelog | **you / agent** (before tagging) |
| Build 4 platforms, draft assets, SHA256SUMS, provenance attestation | CI (`release.yml`, on tag) |
| Independently rebuild and byte-compare all 4 binaries | local controller, before environment approval |
| Publish `dbmd-core` then `dbmd-cli` to crates.io via OIDC | CI (`publish-crates` job, on tag) |
| Publish immutable GitHub release (not latest yet) | CI, only after both crates converge |
| Bump the Homebrew tap formula (`carloslfu/homebrew-tap`) | local controller via optimistic GitHub Contents API |
| Promote GitHub release to latest | local controller, final convergence step |
| Release authorization | protected-environment approval by the authenticated local controller |

Pushing to `main` never publishes. Only a `vX.Y.Z` tag does.

**Homebrew tap:** CI has no tap credential and no Homebrew publishing job. The
controller renders `HomebrewFormula/dbmd.rb.template` only after the immutable
release and exact crates.io checksums verify, then updates `Formula/dbmd.rb`
using the maintainer's existing `gh` session. The current blob SHA is the
optimistic concurrency token. The returned commit must descend directly from
the reviewed tap head, tap `main` must equal that commit, and its bytes are read
back exactly. A killed controller leaves no deploy key or environment secret.

The controller also asserts GitHub release immutability before it creates the
tag. Before approval it downloads the four CI artifacts, rebuilds both Darwin
targets with normalized Mach-O build metadata and both musl targets in
digest-pinned Linux builder images. Linux inputs use the same canonical
`/project`, `/cargo`, and `/rust` paths as CI before each binary and its legal
files are compared byte-for-byte.
After CI completes it verifies the tag-to-SHA binding, exact five-asset set,
SHA256 manifest, every provenance attestation, exact local-vs-crates.io package
checksums, and the resulting tap formula.

## Files to bump (must all agree on the version)

1. **`Cargo.toml`** → `[workspace.package]` `version = "X.Y.Z"`
2. **`crates/dbmd-cli/Cargo.toml`** → the dep pin `dbmd-core = { path = "../dbmd-core", version = "X.Y.Z" }`
3. **`CHANGELOG.md`** → add a `## [X.Y.Z] — <date>` section; update the
   "currently **vX.Y.Z**" line near the top.
4. Run `cargo build --workspace` so **`Cargo.lock`** updates to the new version, and commit it.

**The tag must match the `Cargo.toml` version.** `release.yml`'s `version`
job hard-fails if `vX.Y.Z` ≠ the workspace version, so a stale tag can't ship.

## Pre-tag checks (catch problems before the irreversible publish)

```sh
make fmt-check        # cargo fmt --all --check
make lint             # cargo clippy --workspace --all-targets -- -D warnings
make test             # cargo test --workspace
make publish-check    # cargo package --workspace --locked  (builds each crate from its tarball)
```

`make publish-check` is the important one: it packages each crate exactly as
`cargo publish` would and catches packaging bugs (an `include_str!` that
escapes the crate, a path dep missing a `version`, a missing README) **before**
you tag. CI runs the same check (`publish-check.yml`) on every push.

## Verify after the tag

```sh
gh release view vX.Y.Z                            # 4 tarballs + SHA256SUMS attached
```

`scripts/release.sh` does not return success until the workflow and the
post-release verification are green.

Then confirm on the web (crates.io rate-limits scripted curl — use a browser):

- `https://crates.io/crates/dbmd-cli/versions` — new version shows **"VIA GITHUB"**
  (that label = it was published by Trusted Publishing / OIDC, not a token).
- `https://docs.rs/crate/dbmd-core/X.Y.Z` — builds within a few minutes of publish.

## crates.io is permanent

A published version cannot be deleted, only **yanked** (hidden from
resolution: `cargo yank --version X.Y.Z <crate>`, undo with `--undo`). So get
the version and contents right before tagging. There is no un-publish.

## The publishing setup (how it works, for reference)

- **Trusted Publishing** is configured on both crates (crates.io → each crate →
  Settings → Trusted Publishing): publisher GitHub, repo `carloslfu/db.md`,
  workflow `release.yml`, environment `crates-io`. CI mints a short-lived
  crates.io token via GitHub OIDC at run time — **no token is stored anywhere.**
- The **`crates-io` GitHub environment** must exist (repo → Settings →
  Environments). It binds the OIDC trust — crates.io only accepts a publish
  from a job running in that environment. The intended solo-maintainer setup has
  no required reviewers, so publishing is hands-off.
- The **`release-publishing` environment** has the authenticated maintainer as
  required reviewer. The controller idempotently reconciles that policy and
  approves the exact pending deployment only after independent rebuild
  verification. There is no `DBMD_RELEASE_AUTH`, Homebrew secret, or deploy key
  to leak.

## If a release half-fails

Run `scripts/release.sh X.Y.Z` again. The controller accepts an existing tag
only when it names the exact reviewed `main` commit. It finds the bound
workflow, reruns failed jobs when necessary, re-verifies the four rebuilds
before a new approval, treats already-published crates as idempotent only after
exact checksum comparison, treats an already matching tap formula as a no-op,
and promotes `latest` only after every channel has converged. Never cut a patch
version merely to recover a transient channel failure.
