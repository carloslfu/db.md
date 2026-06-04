//! Drift guard for the single-source SPEC.
//!
//! The repo-root `SPEC.md` is the source of truth. `dbmd spec` embeds it with
//! `include_str!`, but `cargo package` requires the embedded path to stay inside
//! the crate, so it is mirrored into `crates/dbmd-cli/SPEC.md` and the binary
//! embeds the mirror. This test fails if `SPEC.md` is edited without re-running
//! `make sync` — so `dbmd spec` can never ship a contract that has drifted from
//! the authoritative repo-root file.

use std::path::PathBuf;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn bundled_spec_matches_repo_root() {
    let mirror = std::fs::read_to_string(crate_dir().join("SPEC.md")).expect("read mirror SPEC.md");
    let source = std::fs::read_to_string(crate_dir().join("..").join("..").join("SPEC.md"))
        .expect("read repo-root SPEC.md");
    assert_eq!(
        mirror, source,
        "crates/dbmd-cli/SPEC.md has drifted from the repo-root SPEC.md \
         (the source of truth). Run `make sync` and commit the result."
    );
}
