//! The format-v0.4 non-breaking proof (SPEC § Versioning: "A v0.3 store
//! validates unchanged under a v0.4 toolkit"), pinned against the shipped
//! example stores.
//!
//! The examples are committed UNCHANGED from v0.3: none carries a minted ULID
//! `id`, and `examples/research-wiki` carries dozens of hand-authored slug ids
//! (`id: q-learning`, `id: schulman-trpo-2015`, …). Under v0.4 every one of
//! them must keep validating with **zero errors and zero id-related issues** —
//! the recommended lowercase-ULID form is a recommendation, never a gate. A
//! regression here means v0.4 stopped being additive.

use std::path::PathBuf;
use std::process::Command;

/// Absolute path to the `dbmd` binary Cargo built for this test target.
const DBMD: &str = env!("CARGO_BIN_EXE_dbmd");

#[test]
fn every_example_store_validates_with_no_errors_and_no_id_issues() {
    let examples: PathBuf = [env!("CARGO_MANIFEST_DIR"), "..", "..", "examples"]
        .iter()
        .collect();
    let mut stores = 0usize;

    for entry in std::fs::read_dir(&examples).expect("examples/ directory") {
        let dir = entry.expect("dir entry").path();
        if !dir.join("DB.md").is_file() {
            continue; // not a store (stray file)
        }
        stores += 1;

        let out = Command::new(DBMD)
            .args(["--json", "validate", "--all"])
            .current_dir(&dir)
            .output()
            .expect("spawn dbmd");
        assert_eq!(
            out.status.code(),
            Some(0),
            "{} must validate --all clean of errors; stderr: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );

        let report: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{}: validate --json output ({e})", dir.display()));
        assert_eq!(
            report["summary"]["errors"],
            0,
            "{}: zero errors required; report:\n{report:#}",
            dir.display()
        );
        for issue in report["issues"].as_array().expect("issues array") {
            let code = issue["code"].as_str().unwrap_or_default();
            assert!(
                code != "FM_BAD_ID" && code != "DUP_ID",
                "{}: an UNCHANGED v0.3 example fired the v0.4 id check {code} — \
                 the format stopped being additive:\n{issue:#}",
                dir.display()
            );
        }
    }

    assert!(
        stores >= 5,
        "expected the five shipped example stores under {}, found {stores}",
        examples.display()
    );
}
