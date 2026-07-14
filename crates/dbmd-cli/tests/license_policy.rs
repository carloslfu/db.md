//! License-policy guard for the **shipped** `dbmd` binary.
//!
//! db.md's hard rule is permissive-only dependencies. THIRD_PARTY_NOTICES (and
//! the header comments on the workspace + crate `Cargo.toml`s) state the
//! allowlist of accepted SPDX license identifiers. This test makes that prose
//! mechanically true: it walks the *shipped* dependency closure of the `dbmd`
//! binary and fails the build if any crate's SPDX expression cannot be
//! satisfied entirely from the allowlist.
//!
//! ## Why this test exists
//!
//! A hand-kept license list silently drifts from what actually ships. A single
//! `cargo update` (or a new dependency, or a transitive bump) can pull in a
//! crate whose license is GPL/AGPL/LGPL-static — or a *new* permissive one the
//! NOTICES file never recorded, e.g. `unicode-ident`'s conjunctive
//! `(MIT OR Apache-2.0) AND Unicode-3.0`, which a casual "it's MIT-or-Apache"
//! read misses entirely. This test is the immune system: it reads the real
//! graph from `cargo metadata` and refuses anything off the allowlist.
//!
//! ## What "shipped" means here
//!
//! - **Normal edges only.** We follow `kind: null` (normal) dependency edges
//!   from the two workspace crates. `build`/`dev` edges (build scripts, the test
//!   harness like `assert_cmd`) are excluded — they are not in the released
//!   binary.
//! - **Real targets only.** We resolve with `--filter-platform` for the two
//!   targets db.md ships (`x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`)
//!   and take the union, so Windows/wasm/haiku-only crates (which never compile
//!   into the released binary) do not count, and the result is host-independent.
//!
//! ## SPDX semantics
//!
//! The allowlist is checked with correct `AND`/`OR` semantics, not a substring
//! match: an expression is accepted iff it is *satisfiable* from the allowlist.
//! `A OR B` is accepted if either side is; `A AND B` is accepted only if BOTH
//! sides are (so a copyleft term joined with `AND` — the case that cannot be
//! opted out of — fails). The legacy slash form (`MIT/Apache-2.0`) is treated
//! as `OR`; a trailing `WITH <exception>` is stripped to its base license.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::process::Command;

use serde_json::Value;

/// SPDX identifiers db.md accepts. Permissive only — must match the allowlist
/// documented in THIRD_PARTY_NOTICES and the `Cargo.toml` header comments.
/// Deliberately does NOT include any copyleft (GPL/AGPL/LGPL) identifier.
const ALLOWED: &[&str] = &[
    "MIT",
    "Apache-2.0",
    "BSD-3-Clause",
    "0BSD",
    "Unlicense",
    "Zlib",
    "Unicode-3.0",
    // The link.md client's TLS stack (ureq → rustls): rustls-webpki +
    // untrusted are plain ISC; ring is `Apache-2.0 AND ISC` (conjunctive).
    "ISC",
    // webpki-roots — the bundled CCADB root-certificate data (a permissive
    // DATA license: no copyleft, no patent traps).
    "CDLA-Permissive-2.0",
];

/// The two targets the released `dbmd` binary is built for. We union the
/// shipped closure across both so the test does not depend on the host OS.
const SHIP_TARGETS: &[&str] = &["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"];

/// Shipped crates that carry an obligation beyond plain MIT/Apache and are
/// therefore called out by name in THIRD_PARTY_NOTICES. Pinned here so the test
/// is also a *positive* guard: if a dependency change drops one of these or
/// changes its license string, the maintainer is forced back to the NOTICES
/// file to keep the documentation honest, rather than the call-out silently
/// rotting. `(crate name, exact SPDX expression)`.
const DOCUMENTED_NONTRIVIAL: &[(&str, &str)] = &[
    ("unicode-ident", "(MIT OR Apache-2.0) AND Unicode-3.0"),
    ("encoding_rs", "(Apache-2.0 OR MIT) AND BSD-3-Clause"),
    ("zlib-rs", "Zlib"),
    // The link.md client's TLS stack (feature `link`, default-on).
    ("ring", "Apache-2.0 AND ISC"),
    ("rustls-webpki", "ISC"),
    ("untrusted", "ISC"),
    ("webpki-roots", "CDLA-Permissive-2.0"),
];

/// Resolve `cargo metadata` for one target and return its parsed JSON.
fn metadata_for_target(target: &str) -> Value {
    // Cargo sets $CARGO to the cargo that launched the test; fall back to PATH.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    // CARGO_MANIFEST_DIR is .../crates/dbmd-cli; the workspace root is two up.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_manifest = format!("{manifest_dir}/../../Cargo.toml");

    let out = Command::new(&cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--filter-platform",
            target,
            "--manifest-path",
            &workspace_manifest,
        ])
        // Keep cargo from inheriting test-runner env that could perturb output.
        .env_remove("RUSTFLAGS")
        .output()
        .unwrap_or_else(|e| panic!("failed to run `{cargo} metadata` for {target}: {e}"));

    assert!(
        out.status.success(),
        "`cargo metadata --filter-platform {target}` failed:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );

    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("`cargo metadata` for {target} was not valid JSON: {e}"))
}

/// Walk normal-edge dependencies from the two workspace crates and return the
/// shipped closure as a map of `package id -> (name, license)`.
fn shipped_closure(meta: &Value) -> BTreeMap<String, (String, String)> {
    let packages = meta["packages"].as_array().expect("metadata.packages");

    // id -> (name, license-or-empty)
    let mut by_id: BTreeMap<&str, (String, String)> = BTreeMap::new();
    // Workspace roots we start the walk from.
    let mut roots: Vec<String> = Vec::new();
    for p in packages {
        let id = p["id"].as_str().expect("package.id");
        let name = p["name"].as_str().expect("package.name").to_string();
        let license = p["license"].as_str().unwrap_or("").to_string();
        if name == "dbmd-cli" || name == "dbmd-core" {
            roots.push(id.to_string());
        }
        by_id.insert(id, (name, license));
    }
    assert_eq!(
        roots.len(),
        2,
        "expected to find both workspace crates (dbmd-cli, dbmd-core) in metadata",
    );

    // id -> set of normal-edge dependency ids, from the resolve graph.
    let nodes = meta["resolve"]["nodes"]
        .as_array()
        .expect("metadata.resolve.nodes");
    let mut normal_deps: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for n in nodes {
        let id = n["id"].as_str().expect("node.id");
        let mut deps: Vec<&str> = Vec::new();
        for d in n["deps"].as_array().expect("node.deps") {
            let is_normal = d["dep_kinds"]
                .as_array()
                .expect("dep.dep_kinds")
                .iter()
                // A normal dependency edge is encoded as `kind: null`.
                .any(|k| k["kind"].is_null());
            if is_normal {
                deps.push(d["pkg"].as_str().expect("dep.pkg"));
            }
        }
        normal_deps.insert(id, deps);
    }

    // BFS over normal edges.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = roots.into_iter().collect();
    let mut shipped: BTreeMap<String, (String, String)> = BTreeMap::new();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(info) = by_id.get(id.as_str()) {
            shipped.insert(id.clone(), info.clone());
        }
        if let Some(deps) = normal_deps.get(id.as_str()) {
            for dep in deps {
                if !seen.contains(*dep) {
                    queue.push_back((*dep).to_string());
                }
            }
        }
    }
    shipped
}

/// Is a single SPDX license id on the allowlist? A trailing `WITH <exception>`
/// is reduced to its base license (the exception only *loosens* obligations).
fn token_allowed(token: &str) -> bool {
    let base = token.split(" WITH ").next().unwrap_or(token).trim();
    ALLOWED.iter().any(|a| a.eq_ignore_ascii_case(base))
}

/// Evaluate whether an SPDX expression is satisfiable from the allowlist, with
/// correct `OR` (either side) / `AND` (both sides) semantics and parentheses.
///
/// Minimal recursive-descent over the grammar `cargo metadata` emits in
/// practice: identifiers, `AND`, `OR`, `( )`, the legacy `/` (treated as `OR`),
/// and `WITH` exceptions. Precedence follows SPDX: `AND` binds tighter than
/// `OR`.
fn expr_satisfiable(expr: &str) -> bool {
    let tokens = tokenize(expr);
    let mut pos = 0;
    let val = parse_or(&tokens, &mut pos);
    debug_assert_eq!(pos, tokens.len(), "unconsumed tokens in `{expr}`");
    val
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    And,
    Or,
    LParen,
    RParen,
    Ident(String),
}

fn tokenize(expr: &str) -> Vec<Tok> {
    // Normalize the legacy slash form to ` OR ` and pad parens so a simple
    // whitespace split yields clean tokens. (`MIT/Apache-2.0` is an old crates
    // convention meaning a disjunction.)
    let normalized = expr
        .replace('/', " OR ")
        .replace('(', " ( ")
        .replace(')', " ) ");
    let mut out = Vec::new();
    let mut words = normalized.split_whitespace().peekable();
    while let Some(w) = words.next() {
        match w {
            "AND" => out.push(Tok::And),
            "OR" => out.push(Tok::Or),
            "(" => out.push(Tok::LParen),
            ")" => out.push(Tok::RParen),
            "WITH" => {
                // Fold `WITH <exception>` into the preceding identifier so the
                // exception travels with its license through the grammar.
                if let Some(exc) = words.next() {
                    if let Some(Tok::Ident(prev)) = out.last_mut() {
                        prev.push_str(" WITH ");
                        prev.push_str(exc);
                    } else {
                        out.push(Tok::Ident(format!("WITH {exc}")));
                    }
                }
            }
            other => out.push(Tok::Ident(other.to_string())),
        }
    }
    out
}

// or := and ( OR and )*
fn parse_or(tokens: &[Tok], pos: &mut usize) -> bool {
    let mut acc = parse_and(tokens, pos);
    while matches!(tokens.get(*pos), Some(Tok::Or)) {
        *pos += 1;
        let rhs = parse_and(tokens, pos);
        acc = acc || rhs;
    }
    acc
}

// and := atom ( AND atom )*
fn parse_and(tokens: &[Tok], pos: &mut usize) -> bool {
    let mut acc = parse_atom(tokens, pos);
    while matches!(tokens.get(*pos), Some(Tok::And)) {
        *pos += 1;
        let rhs = parse_atom(tokens, pos);
        acc = acc && rhs;
    }
    acc
}

// atom := IDENT | ( or )
fn parse_atom(tokens: &[Tok], pos: &mut usize) -> bool {
    match tokens.get(*pos) {
        Some(Tok::LParen) => {
            *pos += 1;
            let v = parse_or(tokens, pos);
            assert!(
                matches!(tokens.get(*pos), Some(Tok::RParen)),
                "unbalanced parentheses in SPDX expression",
            );
            *pos += 1;
            v
        }
        Some(Tok::Ident(id)) => {
            *pos += 1;
            token_allowed(id)
        }
        other => panic!("unexpected token while parsing SPDX expression: {other:?}"),
    }
}

/// The guard: every shipped crate's license must be satisfiable from the
/// permissive allowlist. Any crate that is not is a policy violation and must
/// be removed or, if it is genuinely permissive, added to BOTH the allowlist
/// here and the THIRD_PARTY_NOTICES file (in lockstep).
#[test]
fn shipped_dependencies_are_permissive_only() {
    // Union the shipped closure across both ship targets so the result does not
    // depend on which OS runs the test.
    let mut shipped: BTreeMap<String, (String, String)> = BTreeMap::new();
    for target in SHIP_TARGETS {
        let meta = metadata_for_target(target);
        for (id, info) in shipped_closure(&meta) {
            shipped.entry(id).or_insert(info);
        }
    }
    assert!(
        shipped.len() > 50,
        "shipped closure looks implausibly small ({} crates) — metadata walk is likely broken",
        shipped.len(),
    );

    let mut violations: Vec<String> = Vec::new();
    let mut missing_license: Vec<String> = Vec::new();
    for (name, license) in shipped.values() {
        if license.is_empty() {
            missing_license.push(name.clone());
            continue;
        }
        if !expr_satisfiable(license) {
            violations.push(format!("  {name}: \"{license}\""));
        }
    }

    assert!(
        missing_license.is_empty(),
        "shipped crate(s) declare no SPDX license (cannot be verified permissive): {missing_license:?}",
    );

    assert!(
        violations.is_empty(),
        "shipped dependency license(s) are NOT satisfiable from the permissive allowlist \
         {ALLOWED:?}.\nEach line is a crate compiled into the released `dbmd` binary whose \
         SPDX expression contains a term off the allowlist (e.g. a copyleft `AND`, or a new \
         permissive license not yet recorded). Remove the crate, or — if it is genuinely \
         permissive — add the identifier to BOTH `ALLOWED` here and THIRD_PARTY_NOTICES.\n{}",
        violations.join("\n"),
    );
}

/// Positive guard for the call-outs in THIRD_PARTY_NOTICES: the three crates we
/// document as carrying a non-MIT/Apache obligation must actually be present in
/// the shipped tree with the exact license string the NOTICES file quotes. If a
/// dependency change drops one of them (or its license string changes), this
/// fails and forces the NOTICES call-out to be revisited — the documentation
/// cannot silently go stale.
#[test]
fn documented_nontrivial_licenses_match_reality() {
    let mut shipped: BTreeMap<String, String> = BTreeMap::new();
    for target in SHIP_TARGETS {
        let meta = metadata_for_target(target);
        for (_id, (name, license)) in shipped_closure(&meta) {
            shipped.entry(name).or_insert(license);
        }
    }

    for (crate_name, expected_license) in DOCUMENTED_NONTRIVIAL {
        match shipped.get(*crate_name) {
            None => panic!(
                "THIRD_PARTY_NOTICES calls out `{crate_name}` ({expected_license}) as a shipped \
                 crate with a non-MIT/Apache obligation, but it is no longer in the shipped \
                 closure. Update the NOTICES call-out (and this list) to match the current tree.",
            ),
            Some(actual) => assert_eq!(
                actual, expected_license,
                "license string for shipped crate `{crate_name}` changed: NOTICES documents \
                 \"{expected_license}\" but cargo metadata reports \"{actual}\". Re-check the new \
                 license against the allowlist and update THIRD_PARTY_NOTICES.",
            ),
        }
    }
}

/// Parse the `allow = [ ... ]` array out of `[licenses]` in `/deny.toml` with a
/// tiny line scanner (no `toml` dependency — dbmd-core/cli take zero new crates
/// for a test). Comments (`#`) and the surrounding whitespace are ignored.
fn deny_toml_allow_list() -> BTreeSet<String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let deny_path = format!("{manifest_dir}/../../deny.toml");
    let text = std::fs::read_to_string(&deny_path)
        .unwrap_or_else(|e| panic!("read deny.toml at {deny_path}: {e}"));

    let mut in_licenses = false;
    let mut in_allow = false;
    let mut out: BTreeSet<String> = BTreeSet::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_licenses = line == "[licenses]";
            in_allow = false;
            continue;
        }
        if !in_licenses {
            continue;
        }
        if !in_allow {
            if line.starts_with("allow") && line.contains('[') {
                in_allow = true;
                collect_quoted(line, &mut out); // handle single-line arrays too
                if line.contains(']') {
                    break;
                }
            }
            continue;
        }
        let closing = line.contains(']');
        collect_quoted(line, &mut out);
        if closing {
            break;
        }
    }
    assert!(
        !out.is_empty(),
        "could not parse a non-empty [licenses].allow array from deny.toml \
         (parser or file shape changed)",
    );
    out
}

/// Push every `"..."`-quoted token on a line into `out`, ignoring `#` comments.
fn collect_quoted(line: &str, out: &mut BTreeSet<String>) {
    let code = line.split('#').next().unwrap_or("");
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'"' {
                j += 1;
            }
            out.insert(code[start..j].to_string());
            i = j + 1;
        } else {
            i += 1;
        }
    }
}

/// Bind the two enforcement points together: the in-test `ALLOWED` (checked on
/// every `cargo test`) and `deny.toml`'s `[licenses].allow` (checked by
/// `cargo deny check licenses` in CI) MUST be the same set. Editing one allow
/// list without the other — the silent-drift failure mode the review flagged —
/// fails here.
#[test]
fn allow_list_matches_deny_toml() {
    let from_test: BTreeSet<String> = ALLOWED.iter().map(|s| s.to_string()).collect();
    let from_toml = deny_toml_allow_list();
    assert_eq!(
        from_test,
        from_toml,
        "the permissive allow-list has drifted between this test's ALLOWED and \
         deny.toml's [licenses].allow. Keep them identical.\n  only in test:     {:?}\n  \
         only in deny.toml: {:?}",
        from_test.difference(&from_toml).collect::<Vec<_>>(),
        from_toml.difference(&from_test).collect::<Vec<_>>(),
    );
}

/// Pin the SPDX evaluator semantics directly, independent of whatever crates
/// happen to be in the tree today. This is what makes the guard robust against
/// a future refactor: it asserts that a copyleft term joined with `AND` (the
/// case that CANNOT be opted out of) is rejected, while the conjunctive
/// permissive case from the original finding (`unicode-ident`) is accepted.
#[test]
fn spdx_and_or_semantics() {
    // Plain permitted identifiers.
    assert!(expr_satisfiable("MIT"));
    assert!(expr_satisfiable("Apache-2.0"));
    assert!(expr_satisfiable("Zlib"));
    assert!(expr_satisfiable("Unicode-3.0"));

    // A single copyleft identifier is rejected.
    assert!(!expr_satisfiable("GPL-3.0"));
    assert!(!expr_satisfiable("AGPL-3.0-only"));
    assert!(!expr_satisfiable("LGPL-2.1-only"));

    // OR: satisfiable if ANY branch is permitted — so a copyleft OR-alternative
    // is dischargeable via the permissive branch.
    assert!(expr_satisfiable("MIT OR Apache-2.0"));
    assert!(expr_satisfiable("Apache-2.0 OR GPL-3.0")); // discharge via Apache-2.0
    assert!(expr_satisfiable("Unlicense OR MIT"));
    assert!(!expr_satisfiable("GPL-3.0 OR AGPL-3.0")); // no permissive branch

    // AND: satisfiable only if EVERY term is permitted — a conjunctive copyleft
    // term always binds and must fail.
    assert!(!expr_satisfiable("MIT AND GPL-3.0"));
    assert!(!expr_satisfiable("(MIT OR Apache-2.0) AND GPL-3.0"));

    // The exact expressions from the audit finding / shipped tree.
    assert!(expr_satisfiable("(MIT OR Apache-2.0) AND Unicode-3.0")); // unicode-ident
    assert!(expr_satisfiable("(Apache-2.0 OR MIT) AND BSD-3-Clause")); // encoding_rs

    // Legacy slash form is treated as OR.
    assert!(expr_satisfiable("MIT/Apache-2.0"));
    assert!(expr_satisfiable("Apache-2.0/MIT"));

    // A `WITH <exception>` reduces to its base license.
    assert!(expr_satisfiable("Apache-2.0 WITH LLVM-exception"));
    assert!(!expr_satisfiable("GPL-3.0 WITH Classpath-exception-2.0"));
}
