//! no_stub_bodies.rs — "the core is implemented" guard.
//!
//! WHY THIS EXISTS
//! ---------------
//! `dbmd-core` was once a typed skeleton: every operation had its real
//! signature but a `todo!()` body, guarded by a crate-wide
//! `#![allow(unused_variables, unused_imports, unused_mut, dead_code)]` "until
//! the bodies land". The bodies have since landed and the allow was removed.
//!
//! Two regressions this test exists to catch:
//!
//! 1. **A real stub body sneaks back in.** Any `todo!()` / `unimplemented!()`
//!    macro *invocation* in shipped (non-`#[cfg(test)]`) core code means an
//!    operation panics at runtime instead of doing its job. Validating claim 2
//!    ("ALL toolkit logic lives in dbmd-core") requires the logic to actually be
//!    there, not deferred to a panic.
//!
//! 2. **The crate-wide unused/dead-code allow comes back.** That blanket
//!    `#![allow(...)]` was load-bearing only for the skeleton; with the bodies
//!    landed it silently masks genuinely-unused imports, bindings, and dead
//!    code — exactly the rot a future audit would want the compiler to surface.
//!    Re-introducing it must fail the build's tests, not pass quietly.
//!
//! These are deliberately checked against the *source text* (the contract is
//! about the source a reader audits), mirroring `notices.rs`'s source-auditing
//! shape. Occurrences inside `//`/`///`/`//!` line comments are ignored so the
//! prose may still *mention* `todo!()` when explaining history.

use std::path::PathBuf;

/// `<root>/crates/dbmd-core/src`. `CARGO_MANIFEST_DIR` is `<root>/crates/dbmd-cli`.
fn core_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("dbmd-core")
        .join("src")
}

/// Every `*.rs` file under `dbmd-core/src`, recursively.
fn core_rs_files() -> Vec<PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir core src") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&core_src_dir(), &mut out);
    assert!(
        !out.is_empty(),
        "found no .rs files under {} — path resolution is wrong",
        core_src_dir().display()
    );
    out
}

/// The portion of a source line that is *code*, i.e. with any trailing `//`
/// line comment stripped. Intentionally simple: it is conservative about string
/// literals containing `//` (treats `//` inside a string as a comment start),
/// which only makes the stub scan *stricter*, never laxer — a false "this is a
/// comment" can only hide a macro, but no shipped string literal in core embeds
/// a `todo!()`/`unimplemented!()` token, so this cannot mask a real regression.
fn code_part(line: &str) -> &str {
    match line.find("//") {
        Some(idx) => &line[..idx],
        None => line,
    }
}

#[test]
fn no_stub_macro_bodies_in_core() {
    let mut offenders: Vec<String> = Vec::new();

    for file in core_rs_files() {
        let text = std::fs::read_to_string(&file).expect("read core source");
        let rel = file
            .strip_prefix(core_src_dir())
            .unwrap_or(&file)
            .display()
            .to_string();

        for (i, line) in text.lines().enumerate() {
            let code = code_part(line);
            for needle in ["todo!()", "todo!(", "unimplemented!()", "unimplemented!("] {
                if code.contains(needle) {
                    offenders.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                    break;
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "dbmd-core must contain ZERO `todo!()`/`unimplemented!()` macro bodies \
         (the crate is fully implemented, not a skeleton). Offending lines:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_crate_wide_unused_or_deadcode_allow_in_core() {
    // The skeleton-era blanket allow. Once the bodies landed, this masks real
    // unused/dead-code lints from any future audit. Match it tolerant of
    // whitespace and argument order so a reformat or re-order can't slip past.
    let categories = [
        "unused_variables",
        "unused_imports",
        "unused_mut",
        "dead_code",
    ];

    let mut offenders: Vec<String> = Vec::new();

    for file in core_rs_files() {
        let text = std::fs::read_to_string(&file).expect("read core source");
        let rel = file
            .strip_prefix(core_src_dir())
            .unwrap_or(&file)
            .display()
            .to_string();

        for (i, line) in text.lines().enumerate() {
            let code = code_part(line);
            // A crate-wide (`#!`) allow that covers any of the blanket
            // skeleton categories is the regression we forbid.
            let is_inner_allow = code.contains("#![allow(");
            if is_inner_allow && categories.iter().any(|c| code.contains(c)) {
                offenders.push(format!("{rel}:{}: {}", i + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "dbmd-core must NOT re-introduce a crate-wide \
         `#![allow(unused_variables|unused_imports|unused_mut|dead_code)]` — the \
         bodies have landed, so this only masks real lints. Offending lines:\n{}",
        offenders.join("\n")
    );
}
