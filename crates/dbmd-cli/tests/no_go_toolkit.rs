// SPDX-License-Identifier: Apache-2.0
//
// Hard-rule guard: the repo ships ONE binary (`dbmd`), all Rust, with zero
// AI/LLM. This test pins the cleanup that retired the v0.1 Go toolkit so it can
// never creep back. It fails if anyone:
//   - re-adds a Go source / `go.mod` / `go.sum` anywhere in the tree,
//   - re-introduces a forbidden binary name (`dbmd-curator`, the ingesters,
//     the `dbmd-watch`/`dbmd-imap`/`dbmd-mcp` short names) in the build/ship
//     surface,
//   - bakes an LLM provider API key into the build surface (zero AI),
//   - reverts the Makefile or CI from the Rust workspace back to `go build`.
//
// Rationale: the contradiction the review caught was not in any single Rust
// code path — it was the *build surface* (Makefile, CI, Docker, tracked Go
// sources) still being the multi-binary Go toolkit while the workspace claimed
// "one binary, all Rust." A repo-hygiene test is the right shape of guard for a
// repo-hygiene invariant.

use std::fs;
use std::path::{Path, PathBuf};

/// Repo root = two levels up from this crate's manifest dir
/// (`<repo>/crates/dbmd-cli` -> `<repo>`).
fn repo_root() -> PathBuf {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent() // crates/
        .and_then(Path::parent) // repo root
        .expect("crate dir has a repo-root grandparent")
        .to_path_buf()
}

/// Walk the repo, skipping build output and VCS internals, yielding every file.
fn repo_files(root: &Path) -> Vec<PathBuf> {
    fn is_skipped_dir(name: &str) -> bool {
        // `target/` is cargo build output; `.git/` is VCS internals. Neither is
        // part of the tracked source surface this guard polices, and a stray Go
        // object cached under target/ must not fail the build.
        matches!(name, "target" | ".git")
    }

    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                let name = entry.file_name();
                if !is_skipped_dir(&name.to_string_lossy()) {
                    stack.push(path);
                }
            } else if file_type.is_file() {
                out.push(path);
            }
        }
    }
    out
}

/// True if `text` invokes the Go tool as a standalone command (`go build`,
/// `go test`, `go mod`, `go vet`, `go run`, …). Tokenizes on whitespace so the
/// `go` inside `cargo`, `golang`, etc. never matches.
fn invokes_go_tool(text: &str) -> bool {
    const GO_SUBCOMMANDS: &[&str] = &["build", "test", "mod", "vet", "run", "install", "fmt"];
    text.lines().any(|line| {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        tokens.windows(2).any(|w| {
            // `w[0]` must be exactly `go` (a whole token), and the next token a
            // Go subcommand. A leading `@`/`-` make-prefix on `go` is stripped.
            let cmd = w[0].trim_start_matches(['@', '-']);
            cmd == "go" && GO_SUBCOMMANDS.contains(&w[1])
        })
    })
}

#[test]
fn invokes_go_tool_distinguishes_go_from_cargo() {
    // Guards the guard: `cargo build`/`cargo test` must NOT read as the Go tool,
    // and a real `go build` must.
    assert!(!invokes_go_tool("cargo build --workspace"));
    assert!(!invokes_go_tool("\tcargo test --workspace"));
    assert!(!invokes_go_tool("# golang is not used here"));
    assert!(invokes_go_tool("\tgo build ./..."));
    assert!(invokes_go_tool("        run: go test ./..."));
}

#[test]
fn no_go_sources_or_modules_remain() {
    let root = repo_root();
    let offenders: Vec<String> = repo_files(&root)
        .into_iter()
        .filter(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            let is_go_src = p.extension().map(|e| e == "go").unwrap_or(false);
            let is_go_mod = matches!(name.as_deref(), Some("go.mod") | Some("go.sum"));
            is_go_src || is_go_mod
        })
        .map(|p| {
            p.strip_prefix(&root)
                .unwrap_or(&p)
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "Go toolkit must be fully removed (all-Rust hard rule). Found Go files: {offenders:?}"
    );
}

#[test]
fn makefile_drives_the_rust_workspace_not_go() {
    let root = repo_root();
    let makefile = fs::read_to_string(root.join("Makefile")).expect("Makefile must exist");

    // The build path is the Rust workspace.
    assert!(
        makefile.contains("cargo build --workspace"),
        "Makefile `build` must run `cargo build --workspace`, not the Go toolkit"
    );
    assert!(
        makefile.contains("cargo test --workspace"),
        "Makefile `test` must run `cargo test --workspace`"
    );

    // And it is NOT the old Go multi-binary build. Match `go` as a standalone
    // command token so we don't false-positive on the `go` inside `cargo`.
    assert!(
        !invokes_go_tool(&makefile),
        "Makefile must not invoke the `go` tool (`go build`/`go test`/…) — all-Rust hard rule"
    );
    for forbidden in FORBIDDEN_BINARY_NAMES {
        assert!(
            !makefile.contains(forbidden),
            "Makefile must not reference the forbidden binary `{forbidden}` \
             (one-binary `dbmd` hard rule)"
        );
    }
}

#[test]
fn ci_actually_builds_and_tests_the_rust_workspace() {
    // The review finding this guard pins was, at its root, that CI built and
    // tested ONLY the Go tree — the entire Rust workspace (and every corpus
    // test) never ran in CI. Asserting the *absence* of `go build` is not
    // enough: a CI file that runs neither `go` nor `cargo test --workspace`
    // (an empty job, or one that only runs `cargo fmt`) would silently leave
    // the toolkit untested again and still pass the negative checks below.
    // This test is the positive counterpart — CI MUST exercise the workspace.
    let root = repo_root();
    let ci = fs::read_to_string(root.join(".github/workflows/test.yml"))
        .expect(".github/workflows/test.yml must exist");

    assert!(
        ci.contains("cargo build --workspace"),
        ".github/workflows/test.yml must build the whole Rust workspace \
         (`cargo build --workspace`); CI testing only part of (or none of) the \
         workspace is the exact regression this guard exists to catch"
    );
    assert!(
        ci.contains("cargo test --workspace"),
        ".github/workflows/test.yml must run `cargo test --workspace` so the \
         dbmd-core + dbmd-cli tests (including the corpus e2e suites) actually \
         run in CI — not just locally"
    );
    // And it must not have reverted to the Go toolkit it replaced.
    assert!(
        !invokes_go_tool(&ci),
        ".github/workflows/test.yml must not invoke the `go` tool \
         (`go build`/`go test`/…) — all-Rust hard rule"
    );
}

/// Binary names the plan explicitly forbids: no curator binary, no ingester
/// binaries, none of their short aliases. Only `dbmd` ships.
const FORBIDDEN_BINARY_NAMES: &[&str] = &[
    "dbmd-curator",
    "dbmd-file-watcher",
    "dbmd-email-imap",
    "dbmd-mcp-fetcher",
    "dbmd-watch",
    "dbmd-imap",
    "dbmd-mcp",
];

#[test]
fn build_surface_has_no_forbidden_binaries_or_ai_keys() {
    let root = repo_root();

    // The files that define how the toolkit is built, tested, shipped, and
    // documented to contributors — the exact surface the review flagged.
    let build_surface = [
        "Makefile",
        ".github/workflows/test.yml",
        "CONTRIBUTING.md",
        "SECURITY.md",
        "Cargo.toml",
        "crates/dbmd-cli/Cargo.toml",
        "crates/dbmd-core/Cargo.toml",
    ];

    for rel in build_surface {
        let path = root.join(rel);
        // A docker-compose.yml (the v0.1 curator+ingester stack) must stay
        // deleted; only assert on files that are expected to exist.
        let contents = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for forbidden in FORBIDDEN_BINARY_NAMES {
            assert!(
                !contents.contains(forbidden),
                "{rel} references forbidden binary `{forbidden}` \
                 (one-binary `dbmd`, no curator/ingester binaries)"
            );
        }

        // No file in the build/ship surface invokes the Go tool.
        assert!(
            !invokes_go_tool(&contents),
            "{rel} invokes the `go` tool (`go build`/`go test`/…) — all-Rust hard rule"
        );

        // Zero AI: no provider API keys wired into the build/ship surface.
        for key in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY"] {
            assert!(
                !contents.contains(key),
                "{rel} wires up `{key}` — `dbmd` ships zero AI and handles no API keys"
            );
        }
    }

    // The v0.1 Docker stack ran `dbmd-curator` + ingesters with provider keys;
    // it must not come back.
    assert!(
        !root.join("docker-compose.yml").exists(),
        "docker-compose.yml ran the forbidden curator+ingester binaries with \
         provider API keys; it must stay removed"
    );
}
