# Releasing db.md

How to cut a new release of the `dbmd` toolkit (`dbmd-core` + `dbmd-cli`).
Written so an agent or a human can run it cold.

## TL;DR

Bump the version, push `main`, push a `vX.Y.Z` tag. The tag triggers CI,
which builds all platforms and publishes to crates.io via Trusted Publishing
(OIDC, no token), then creates the GitHub release. In the intended
solo-maintainer setup, there is no manual approval step.

```sh
# 1. bump version (see "Files to bump" below), then:
git add -A && git commit -m "release: X.Y.Z — <one line>"
git push origin main          # runs CI checks only — does NOT publish

# 2. tag it — THIS is the publish trigger
git tag vX.Y.Z
git push origin vX.Y.Z
```

Then watch the run and verify (see "Verify" below).

## What is automatic vs. manual

| Step | Who |
|---|---|
| Version bump + changelog | **you / agent** (before tagging) |
| Build 4 platforms, GitHub release, SHA256SUMS, provenance attestation | CI (`release.yml`, on tag) |
| Publish `dbmd-core` then `dbmd-cli` to crates.io via OIDC | CI (`publish-crates` job, on tag) |
| Bump the Homebrew tap formula (`carloslfu/homebrew-tap`) | CI (`homebrew` job, on tag) when `HOMEBREW_TAP_DEPLOY_KEY` is configured; otherwise it skips cleanly |
| Approval click | None in the intended solo-maintainer setup; adding required reviewers to the `crates-io` environment turns this into a GitHub approval gate |

Pushing to `main` never publishes. Only a `vX.Y.Z` tag does.

**Homebrew tap:** the `homebrew` job renders `HomebrewFormula/dbmd.rb.template`
(via `HomebrewFormula/render.sh <version> SHA256SUMS`) and pushes
`Formula/dbmd.rb` to `carloslfu/homebrew-tap`. It authenticates with an SSH
**deploy key** scoped to the tap repo only (write), stored as the
`HOMEBREW_TAP_DEPLOY_KEY` secret on this repo — least-privilege, no broad
account token in CI. If that secret is absent the job **skips cleanly** and the
formula can be bumped by hand: `HomebrewFormula/render.sh X.Y.Z SHA256SUMS >
Formula/dbmd.rb` (download `SHA256SUMS` from the release first), then commit to
the tap. To rotate: generate a new ed25519 pair, replace the tap's deploy key
(tap repo → Settings → Deploy keys) and the `HOMEBREW_TAP_DEPLOY_KEY` secret on
this repo.

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
gh run list --workflow=release.yml --limit 1      # find the run
gh run watch <run-id>                             # all jobs should go green
gh release view vX.Y.Z                            # 4 tarballs + SHA256SUMS attached
```

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
- **To re-add a manual approval gate** (e.g. if more maintainers join): repo →
  Settings → Environments → `crates-io` → add yourself/others as Required
  reviewers. The publish job will then wait for a one-click approval per release.

## If a release half-fails

The `publish-crates` job is **idempotent** — it skips any version already on
crates.io. So if `dbmd-core` published but `dbmd-cli` failed (e.g. a transient
index lag), just re-run the failed job (`gh run rerun <run-id> --failed`) or
re-push the tag; it won't double-publish core.
