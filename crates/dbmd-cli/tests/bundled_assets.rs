//! Drift guard for the single-source assets.
//!
//! The repo-root `SPEC.md` and `skills/db-md/SKILL.md` are the source of truth.
//! The `dbmd` binary embeds them with `include_str!`, but `cargo package`
//! requires the embedded path to stay inside the crate, so each is mirrored into
//! `crates/dbmd-cli/` and the binary embeds the mirror. These tests fail if a
//! root source is edited without re-running `make sync` — so `dbmd spec` and
//! `dbmd install-skill` can never ship content that has drifted from the
//! authoritative repo-root files.

use std::path::PathBuf;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    // crates/dbmd-cli -> repo root
    crate_dir().join("..").join("..")
}

fn read(path: PathBuf) -> String {
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn bundled_spec_matches_repo_root() {
    let mirror = read(crate_dir().join("SPEC.md"));
    let source = read(repo_root().join("SPEC.md"));
    assert_eq!(
        mirror, source,
        "crates/dbmd-cli/SPEC.md has drifted from the repo-root SPEC.md \
         (the source of truth). Run `make sync` and commit the result."
    );
}

#[test]
fn bundled_skill_matches_repo_root() {
    let mirror = read(crate_dir().join("skills/db-md/SKILL.md"));
    let source = read(repo_root().join("skills/db-md/SKILL.md"));
    assert_eq!(
        mirror, source,
        "crates/dbmd-cli/skills/db-md/SKILL.md has drifted from the repo-root \
         skills/db-md/SKILL.md (the source of truth). Run `make sync` and commit."
    );
}
