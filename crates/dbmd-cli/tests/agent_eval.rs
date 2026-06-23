//! Block 7 — end-to-end **agent-eval** harness (deterministic, CI-stable).
//!
//! The plan (plans/db-md-rust-toolkit.md, Block 7, lines 536-552) asks for an
//! eval that "wires a Claude Code session (or scripted equivalent) to a temp
//! store via `dbmd`" and "records every `dbmd` invocation … so lifecycle
//! ordering can be asserted." It explicitly permits "a Claude Code session OR
//! scripted agent." We use a **scripted curator** so the eval is a stable,
//! repeatable CI gate with zero LLM dependency — the determinism the whole
//! toolkit was built for (see the `DBMD_NOW` reproducibility hook in
//! `crates/dbmd-core/src/time.rs`) is what makes a byte-for-byte golden
//! possible at all.
//!
//! This file drives the **real release binary** (`target/release/dbmd`, per the
//! Block 7 brief) as a subprocess for every step, with the temp store as the
//! working directory (the way a real agent session runs — `cd` into the store).
//! It covers, in one place:
//!
//!   1. **`corpus-e` end-to-end** — a fixed, lifecycle-ordered sequence of real
//!      `dbmd` invocations (warm-up `log tail` → fold shipped sources → per
//!      entity `fm query` dedup → `write` → block-form links → `log` → working
//!      -set `validate`) against a temp copy of `tests/corpora/corpus-e-agent`,
//!      recording every invocation (args + exit code) to an in-test command log.
//!      The produced store is asserted (a) byte-for-byte against the committed
//!      `EXPECTED/` golden AND (b) against golden-independent **intent
//!      properties** derived from `NOTES.md` + the SPEC (so a golden that was
//!      itself regenerated from buggy output would still be caught).
//!   2. **Session-lifecycle assertions** over the recorded command log (first
//!      call is `log tail`; an `fm query` precedes each contact write; full-path
//!      wiki-links only; a `log` follows each write; `validate` in the back
//!      half; zero `index rebuild` in the operating loop; a final `log`).
//!   3. **Supporting evals** — `search` over corpus-a (20 representative
//!      queries incl. `--type`/`--in`/`--updated-after`, diffed vs the committed
//!      golden), `validate --all` over corpus-b (diffed vs `EXPECTED/
//!      validate.json`), `extract` over corpus-c (diffed vs known-good `.txt`),
//!      and the policy-refusal eval (a write against a frozen page is refused
//!      with `POLICY_FROZEN_PAGE` and leaves the file byte-identical).
//!   4. **Perf 1M tier** — an opt-in `#[ignore]` test (the 10k tier lives in
//!      `perf_budget.rs`; the 1M tier is documented in `tests/PERF.md` and never
//!      generated/run in CI).
//!
//! Run: `cargo test -p dbmd-cli --test agent_eval`
//! 1M tier: `cargo test -p dbmd-cli --test agent_eval -- --ignored perf_1m`

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::OnceLock;

use common::{copy_store_to_temp, corpora_dir, corpus_a, corpus_b};

// ─────────────────────────────────────────────────────────────────────────────
// The release binary (`target/release/dbmd`) — the Block 7 brief drives THIS,
// not the debug bin `assert_cmd` would pick. We (re)build it under the workspace
// target dir on every test-process start (rebuild-if-stale, NOT build-if-absent)
// so the suite always drives code built from the current tree — never a stale
// leftover from an earlier commit, even under `cargo test --workspace` (which
// builds debug) or a repeated local run where a release binary already exists.
// ─────────────────────────────────────────────────────────────────────────────

/// Absolute path to a **freshly-built** `target/release/dbmd`.
///
/// `CARGO_MANIFEST_DIR` is `<repo>/crates/dbmd-cli`; the workspace target dir is
/// `<repo>/target`. The brief is explicit that the eval drives the *optimized
/// release* artifact (not the debug bin `assert_cmd`/`cargo_bin` would pick, and
/// not via a new build-driver dependency like `escargot`), so we invoke `cargo
/// build --release -p dbmd-cli` ourselves and hand back the artifact path.
///
/// **The build is unconditional, by design.** It is NOT guarded on
/// `bin.is_file()`. A stale release binary left over from an earlier commit is a
/// soundness hazard: this whole file (the flagship byte-for-byte golden plus
/// every supporting eval) would otherwise run against pre-edit code and report
/// green while a regression ships — the failure mode is silent, not loud. Cargo
/// is the staleness oracle: when the binary is genuinely up to date the build is
/// a sub-second no-op; when any source under the dependency graph changed, cargo
/// rebuilds before we return the path. Skipping the build "because a binary
/// already exists" is exactly the bug this guards against — do not re-add an
/// `if !bin.is_file()` short-circuit.
///
/// The build runs **once per test process**, memoized in a `OnceLock`: the first
/// of the file's tests to reach here triggers the (rebuild-if-stale) build, all
/// others observe the completed result. That keeps the guarantee — every test
/// drives a binary built from the current tree — while a parallel `cargo test`
/// run does not fire seven redundant, lock-contending `cargo build` no-ops.
fn release_dbmd() -> PathBuf {
    static DBMD: OnceLock<PathBuf> = OnceLock::new();
    DBMD.get_or_init(build_release_dbmd).clone()
}

/// Run `cargo build --release -p dbmd-cli` unconditionally and return the
/// artifact path, asserting the build succeeded and the on-disk binary is
/// current with the sources cargo just saw. Called once via [`release_dbmd`].
fn build_release_dbmd() -> PathBuf {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let exe = if cfg!(windows) { "dbmd.exe" } else { "dbmd" };
    let bin = repo_root.join("target").join("release").join(exe);

    // Build the CLI in release. `--release` is mandatory (the eval drives the
    // optimized artifact). Inherit stdio so a build failure is visible in the
    // test log rather than swallowed. Cargo no-ops when truly up to date and
    // rebuilds when any input changed, giving rebuild-if-stale semantics
    // without a hand-rolled (and provably wrong) freshness check.
    let status = StdCommand::new(env!("CARGO"))
        .args(["build", "--release", "-p", "dbmd-cli"])
        .current_dir(&repo_root)
        .status()
        .expect("spawn `cargo build --release -p dbmd-cli`");
    assert!(
        status.success(),
        "`cargo build --release -p dbmd-cli` failed — the agent eval drives \
         the release binary at {}",
        bin.display()
    );

    // The build reported success, so the artifact must now exist and — because
    // cargo is the staleness oracle — is current with the sources cargo just
    // saw: if anything in the dependency graph changed cargo rebuilt before
    // returning, and if not the binary was already up to date. Either way we are
    // driving code built from the *current* tree, never a stale leftover from an
    // earlier commit. We assert presence (the loud form of "build produced the
    // artifact"); we do NOT assert anything about the mtime moving forward — on
    // macOS/APFS `cargo` uplifts a cached `target/release/deps/` artifact while
    // *preserving its mtime*, so the uplifted binary's mtime legitimately moves
    // BACKWARDS relative to a previously-linked one. A monotonic-mtime assertion
    // is therefore false here and flakes intermittently; cargo's own up-to-date
    // tracking is the correct (and sufficient) freshness guarantee.
    let _ = std::fs::metadata(&bin).unwrap_or_else(|e| {
        panic!(
            "release binary absent after a successful `cargo build --release`: \
             {} ({e})",
            bin.display()
        )
    });

    bin
}

/// Regression: the release-binary build helper must hand back an existing,
/// executable artifact and must NOT flake on the artifact's mtime.
///
/// `build_release_dbmd` used to assert the binary's mtime never moved backwards
/// across a build. That invariant is false: on macOS/APFS `cargo` uplifts a
/// cached `target/release/deps/` artifact while preserving its mtime, so the
/// on-disk binary's mtime legitimately moves backwards relative to a previously
/// linked one, and the assertion failed intermittently. Driving the helper
/// twice (the second call is a no-op rebuild — the regime that exposes the
/// non-monotonic mtime) must succeed both times and return the same path; the
/// only guarantees we keep are existence + executability, which cargo's own
/// up-to-date tracking backs.
#[test]
fn release_dbmd_build_helper_is_mtime_flake_free() {
    let first = build_release_dbmd();
    assert!(
        first.is_file(),
        "build_release_dbmd must return an existing artifact: {}",
        first.display()
    );

    // A second build (now guaranteed up to date — possibly served via an APFS
    // mtime-preserving uplift) must still succeed and return the same path,
    // never panicking on a non-monotonic mtime.
    let second = build_release_dbmd();
    assert_eq!(
        first, second,
        "build_release_dbmd must return a stable artifact path across calls"
    );
    assert!(
        second.is_file(),
        "the artifact must still exist after a no-op rebuild: {}",
        second.display()
    );

    // The artifact is executable (Unix: at least one exec bit set).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&second)
            .expect("metadata for the release binary")
            .permissions()
            .mode();
        assert!(
            mode & 0o111 != 0,
            "the release binary must be executable; mode = {mode:o}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Command-log-recording invocation harness.
//
// Every `dbmd` call the scripted curator makes goes through `Session::run`,
// which records (args, exit code) so the lifecycle ordering can be asserted —
// the plan's "the harness records every `dbmd` invocation" requirement. The
// store is always the working directory (no `--dir`): that is how a real agent
// session runs, AND it is mandatory for the `log` append form, whose external
// -subcommand parsing cannot accept a `--dir` flag (it always operates on cwd).
// ─────────────────────────────────────────────────────────────────────────────

/// One recorded `dbmd` invocation: the argument vector (sans the binary path)
/// and the process exit code.
#[derive(Debug, Clone)]
struct Invocation {
    args: Vec<String>,
    exit_code: i32,
}

impl Invocation {
    /// The first arg that is a subcommand verb (skips the global `--json` /
    /// `--color` flags), e.g. `"write"`, `"log"`, `"fm"`, `"validate"`.
    fn verb(&self) -> Option<&str> {
        self.args
            .iter()
            .find(|a| !a.starts_with('-'))
            .map(String::as_str)
    }

    /// For a two-level command, the sub-verb after the top verb (e.g. `fm
    /// query` → `"query"`, `log tail` → `"tail"`, `index rebuild` →
    /// `"rebuild"`). `None` for single-level commands.
    fn subverb(&self) -> Option<&str> {
        let mut positionals = self.args.iter().filter(|a| !a.starts_with('-'));
        positionals.next(); // the top verb
        positionals.next().map(String::as_str)
    }

    /// `true` if any argument contains the substring `needle`.
    fn arg_contains(&self, needle: &str) -> bool {
        self.args.iter().any(|a| a.contains(needle))
    }
}

/// A scripted curator session against one temp store: holds the store root, the
/// release binary path, and the append-only command log.
struct Session {
    bin: PathBuf,
    store: PathBuf,
    /// The recorded invocations, in execution order.
    log: Vec<Invocation>,
}

impl Session {
    /// Start a session against a fresh temp copy of the `corpus-e-agent` INPUTS
    /// (`DB.md` + `sources/**` only — the agent produces everything else).
    fn open_corpus_e() -> (tempfile::TempDir, Session) {
        let src = corpora_dir().join("corpus-e-agent");
        let tmp = tempfile::TempDir::new().expect("tempdir for the corpus-e session");
        let store = tmp.path().join("store");
        std::fs::create_dir_all(&store).expect("create store dir");
        // Copy ONLY the inputs, not NOTES.md / EXPECTED/ (those are the contract,
        // not the store the agent operates on).
        copy_into(&src.join("DB.md"), &store.join("DB.md"));
        copy_tree(&src.join("sources"), &store.join("sources"));
        let session = Session {
            bin: release_dbmd(),
            store,
            log: Vec::new(),
        };
        (tmp, session)
    }

    /// Run `dbmd <args>` with the store as cwd at a pinned `DBMD_NOW`, recording
    /// the invocation. Returns `(stdout, exit_code)`. Does NOT assert success —
    /// some steps (a refusal eval, a `validate` on a store with an expected
    /// `info`) legitimately exit non-zero; callers assert what they need.
    fn run(&mut self, now: &str, args: &[&str]) -> (String, i32) {
        let output = StdCommand::new(&self.bin)
            .args(args)
            .current_dir(&self.store)
            .env("DBMD_NOW", now)
            .output()
            .expect("spawn release dbmd");
        let exit_code = output.status.code().unwrap_or(-1);
        self.log.push(Invocation {
            args: args.iter().map(|s| s.to_string()).collect(),
            exit_code,
        });
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            exit_code,
        )
    }

    /// `run`, asserting the call succeeded (exit 0). For the write/log/query
    /// steps where a non-zero exit is a real failure.
    fn run_ok(&mut self, now: &str, args: &[&str]) -> String {
        let (stdout, code) = self.run(now, args);
        assert_eq!(
            code,
            0,
            "expected success from `dbmd {}` (cwd {}), got exit {code}; stdout:\n{stdout}",
            args.join(" "),
            self.store.display()
        );
        stdout
    }

    /// Write a markdown body to a temp file and return its path, for `--body
    /// -file`. The file lives under the store's tempdir parent so it is cleaned
    /// up with the session.
    fn body_file(&self, contents: &str) -> PathBuf {
        // Use a unique name under the store's parent (the TempDir root).
        let dir = self.store.parent().expect("store has a parent");
        let mut path;
        let mut n = 0u32;
        loop {
            path = dir.join(format!("body-{n}.md"));
            if !path.exists() {
                break;
            }
            n += 1;
        }
        std::fs::write(&path, contents).expect("write body file");
        path
    }
}

/// Copy a single file, creating parents.
fn copy_into(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst.parent().expect("dst has a parent")).expect("create parents");
    std::fs::copy(src, dst)
        .unwrap_or_else(|e| panic!("copy {} → {}: {e}", src.display(), dst.display()));
}

/// Recursive directory copy (files + subdirs).
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dest dir");
    for entry in std::fs::read_dir(src).unwrap_or_else(|e| panic!("read {}: {e}", src.display())) {
        let entry = entry.expect("dir entry");
        let target = dst.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The scripted curator session over corpus-e.
//
// A FIXED, lifecycle-ordered sequence. The `DBMD_NOW` for each step advances
// monotonically so timestamps are deterministic AND distinct (exercising the
// index recency ordering). Every payload — fields, summaries, bodies, links —
// is authored from the source evidence + DB.md schemas/policies/instructions
// (the curator's judgment), NOT copied from any tool output.
// ─────────────────────────────────────────────────────────────────────────────

/// Drive the full curator session against `s`. After this returns, `s.store`
/// holds the produced store and `s.log` holds the ordered command log.
fn run_curator_session(s: &mut Session) {
    // ── 1. Open + warm up ────────────────────────────────────────────────────
    // The FIRST dbmd call is `log tail` (lifecycle step 2). The log is absent
    // initially, so this returns an empty list and exit 0.
    s.run_ok("2026-05-29T17:00:00Z", &["--json", "log", "tail", "20"]);
    // The shipped `sources/` is the store's initial state — a "bulk external
    // drop" in SPEC terms. Fold it into the catalog with a SINGLE full rebuild
    // during warm-up, BEFORE the operating loop. This is the one SPEC-sanctioned
    // rebuild; the operating loop below never calls rebuild (write-through only).
    s.run_ok("2026-05-29T17:00:30Z", &["--json", "index", "rebuild"]);
    log_entry(
        s,
        "2026-05-29T17:00:40Z",
        "ingest",
        "-",
        "folded shipped sources into the catalog",
    );

    // ── 2. Operate: companies (create the company before its contacts) ───────
    // Tideform — the client. Dedup by domain first.
    s.run_ok(
        "2026-05-29T17:01:00Z",
        &[
            "--json",
            "fm",
            "query",
            "domain=tideform.com",
            "--type",
            "company",
        ],
    );
    write_record(
        s,
        "2026-05-29T17:01:00Z",
        "records/companies/tideform",
        "company",
        "Tideform — tide-and-surf forecasting app; phase-one mobile rebrand client",
        &[
            "name=Tideform",
            "domain=tideform.com",
            "industry=Consumer mobile app",
            "relationship=client",
        ],
        None,
    );
    log_entry(
        s,
        "2026-05-29T17:01:00Z",
        "create",
        "records/companies/tideform.md",
        "client company for the rebrand",
    );

    // Helio Type — the vendor.
    s.run_ok(
        "2026-05-29T17:02:00Z",
        &[
            "--json",
            "fm",
            "query",
            "domain=heliotype.com",
            "--type",
            "company",
        ],
    );
    write_record(
        s,
        "2026-05-29T17:02:00Z",
        "records/companies/helio-type",
        "company",
        "Helio Type Foundry — annual studio typeface licence vendor",
        &[
            "name=Helio Type Foundry",
            "domain=heliotype.com",
            "industry=Type foundry",
            "relationship=vendor",
        ],
        None,
    );
    log_entry(
        s,
        "2026-05-29T17:02:00Z",
        "create",
        "records/companies/helio-type.md",
        "type licence vendor",
    );

    // Northgate Coffee — the prospect.
    s.run_ok(
        "2026-05-29T17:03:00Z",
        &[
            "--json",
            "fm",
            "query",
            "domain=northgatecoffee.co",
            "--type",
            "company",
        ],
    );
    write_record(
        s,
        "2026-05-29T17:03:00Z",
        "records/companies/northgate-coffee",
        "company",
        "Northgate Coffee — small-batch roastery; packaging-redesign prospect",
        &[
            "name=Northgate Coffee",
            "domain=northgatecoffee.co",
            "industry=Coffee roastery",
            "relationship=prospect",
        ],
        None,
    );
    log_entry(
        s,
        "2026-05-29T17:03:00Z",
        "create",
        "records/companies/northgate-coffee.md",
        "inbound packaging prospect",
    );

    // Lumen Labs — "us". The schema makes `contact.company` required and Theo
    // is an independent contractor with a gmail address; anchoring him to the
    // studio (NOTES "resolution (a)") gives the required link a real target.
    // `relationship` is `partner` (NOT client/vendor/prospect — "us" is not a
    // counterparty), which the enum permits.
    write_record(
        s,
        "2026-05-29T17:04:00Z",
        "records/companies/lumen-labs",
        "company",
        "Lumen Labs — the studio (us); five-person product-design practice",
        &[
            "name=Lumen Labs",
            "domain=lumenlabs.studio",
            "industry=Product design studio",
            "relationship=partner",
        ],
        None,
    );
    log_entry(
        s,
        "2026-05-29T17:04:00Z",
        "create",
        "records/companies/lumen-labs.md",
        "own-company anchor",
    );

    // ── 3. Operate: contacts (fm query dedup precedes EACH contact write) ────
    s.run_ok(
        "2026-05-29T17:05:00Z",
        &[
            "--json",
            "fm",
            "query",
            "email=daniel.osei@tideform.com",
            "--type",
            "contact",
        ],
    );
    write_record(
        s,
        "2026-05-29T17:05:00Z",
        "records/contacts/daniel-osei",
        "contact",
        "Head of Product at Tideform; economic buyer on the Lumen rebrand engagement",
        &[
            "name=Daniel Osei",
            "email=daniel.osei@tideform.com",
            "role=Head of Product",
            "company=[[records/companies/tideform]]",
            "first_touch=2026-04-09",
            "last_touch=2026-04-14",
        ],
        None,
    );
    log_entry(
        s,
        "2026-05-29T17:05:00Z",
        "create",
        "records/contacts/daniel-osei.md",
        "Tideform buyer",
    );

    s.run_ok(
        "2026-05-29T17:06:00Z",
        &[
            "--json",
            "fm",
            "query",
            "email=mara@tideform.com",
            "--type",
            "contact",
        ],
    );
    write_record(
        s,
        "2026-05-29T17:06:00Z",
        "records/contacts/mara-lindqvist",
        "contact",
        "Design Lead at Tideform; Lumen's day-to-day contact on the rebrand",
        &[
            "name=Mara Lindqvist",
            "email=mara@tideform.com",
            "role=Design Lead",
            "company=[[records/companies/tideform]]",
            "first_touch=2026-04-09",
            "last_touch=2026-04-14",
        ],
        None,
    );
    log_entry(
        s,
        "2026-05-29T17:06:00Z",
        "create",
        "records/contacts/mara-lindqvist.md",
        "Tideform design lead",
    );

    s.run_ok(
        "2026-05-29T17:07:00Z",
        &[
            "--json",
            "fm",
            "query",
            "email=sofia@northgatecoffee.co",
            "--type",
            "contact",
        ],
    );
    write_record(
        s,
        "2026-05-29T17:07:00Z",
        "records/contacts/sofia-reyes",
        "contact",
        "Founder of Northgate Coffee; packaging-redesign enquiry",
        &[
            "name=Sofia Reyes",
            "email=sofia@northgatecoffee.co",
            "role=Founder",
            "company=[[records/companies/northgate-coffee]]",
            "first_touch=2026-05-04",
            "last_touch=2026-05-04",
        ],
        None,
    );
    log_entry(
        s,
        "2026-05-29T17:07:00Z",
        "create",
        "records/contacts/sofia-reyes.md",
        "Northgate founder",
    );

    s.run_ok(
        "2026-05-29T17:08:00Z",
        &[
            "--json",
            "fm",
            "query",
            "email=theo.vance@gmail.com",
            "--type",
            "contact",
        ],
    );
    write_record(
        s,
        "2026-05-29T17:08:00Z",
        "records/contacts/theo-vance",
        "contact",
        "Freelance motion designer contracted by Lumen for the Tideform rebrand",
        &[
            "name=Theo Vance",
            "email=theo.vance@gmail.com",
            "role=Freelance Motion Designer",
            "company=[[records/companies/lumen-labs]]",
            "first_touch=2026-04-15",
            "last_touch=2026-04-14",
        ],
        None,
    );
    log_entry(
        s,
        "2026-05-29T17:08:00Z",
        "create",
        "records/contacts/theo-vance.md",
        "contracted motion designer",
    );

    // ── 4. Operate: meeting / invoice / expense ──────────────────────────────
    let meeting_body = "# Tideform rebrand kickoff\n\nKickoff call confirming the phase-one scope and the $45k fixed fee. Derived from the transcript [[sources/transcripts/2026/04/2026-04-14-tideform-kickoff]] and confirmed by the SOW [[sources/docs/2026-04-14-tideform-sow]].\n";
    let mb = s.body_file(meeting_body);
    write_record(
        s,
        "2026-05-29T17:09:00Z",
        "records/meetings/2026-04-14-tideform-kickoff",
        "meeting",
        "Tideform rebrand kickoff; scope, 8-week term, $45k phase-one fee confirmed",
        &[
            "date=2026-04-14",
            // Block-form attendees list of full-path wiki-links (schema:
            // `attendees (required, link to records/contacts/)`).
            "attendees=[[[records/contacts/daniel-osei]], [[records/contacts/mara-lindqvist]], [[records/contacts/theo-vance]]]",
            "location=Video call",
            "duration_min=48",
        ],
        Some(&mb),
    );
    log_entry(
        s,
        "2026-05-29T17:09:00Z",
        "create",
        "records/meetings/2026/04/2026-04-14-tideform-kickoff.md",
        "kickoff from transcript",
    );

    let invoice_body = "# Invoice HT-2026-0417\n\nAnnual Helio Type studio licence. Source: [[sources/emails/2026/04/2026-04-22-helio-type-invoice]]. Payment confirmed by [[sources/docs/2026-05-06-helio-type-receipt]].\n";
    let ib = s.body_file(invoice_body);
    write_record(
        s,
        "2026-05-29T17:10:00Z",
        "records/invoices/2026-04-22-helio-type-ht-2026-0417",
        "invoice",
        "Helio Type HT-2026-0417 annual licence; $1,188 USD; paid 2026-05-06",
        &[
            "date=2026-04-22",
            "amount=1188.00",
            "vendor=[[records/companies/helio-type]]",
            "status=paid",
            "paid_at=2026-05-06",
        ],
        Some(&ib),
    );
    log_entry(
        s,
        "2026-05-29T17:10:00Z",
        "create",
        "records/invoices/2026/04/2026-04-22-helio-type-ht-2026-0417.md",
        "vendor invoice paid",
    );

    // The DB.md rule: every vendor invoice gets a MATCHING expense, linked to
    // the invoice it paid.
    let expense_body = "# Helio Type annual licence\n\nPaid via company card. Settles invoice [[records/invoices/2026/04/2026-04-22-helio-type-ht-2026-0417]].\n";
    let eb = s.body_file(expense_body);
    write_record(
        s,
        "2026-05-29T17:11:00Z",
        "records/expenses/2026-05-06-helio-type-1188",
        "expense",
        "Helio Type annual type licence; $1,188 paid 2026-05-06 via company card",
        &[
            "date=2026-05-06",
            "amount=1188.00",
            "currency=USD",
            "category=Software / type licences",
            "vendor=[[records/companies/helio-type]]",
        ],
        Some(&eb),
    );
    log_entry(
        s,
        "2026-05-29T17:11:00Z",
        "create",
        "records/expenses/2026/05/2026-05-06-helio-type-1188.md",
        "matching expense for the invoice",
    );

    // ── 5. Operate: conclusion records (synthesis; British English per DB.md) ─
    // The former wiki pages are now `meta-type: conclusion` records under
    // records/: the project synthesis is a `project`, the people bios `profile`s.
    let project_body = "# Tideform rebrand\n\nPhase-one mobile rebrand for [[records/companies/tideform]]: a new visual language, a reusable component library, a marketing-site refresh, and motion design. The studio organised the work around an eight-week term with a **$45,000** fixed fee; the first design review is the week of 5 May.\n\nThe economic buyer is [[records/contacts/daniel-osei]]; [[records/contacts/mara-lindqvist]] is the day-to-day design lead and [[records/contacts/theo-vance]] handles motion. Scope and budget were confirmed at the kickoff [[records/meetings/2026/04/2026-04-14-tideform-kickoff]].\n\nDerived from [[sources/emails/2026/04/2026-04-09-tideform-project-intro]], [[sources/transcripts/2026/04/2026-04-14-tideform-kickoff]], and [[sources/docs/2026-04-14-tideform-sow]].\n";
    let wp = s.body_file(project_body);
    write_record(
        s,
        "2026-05-29T17:12:00Z",
        "records/projects/tideform-rebrand",
        "project",
        "Tideform phase-one mobile rebrand; $45k fixed fee, eight-week term",
        &[
            "meta-type=conclusion",
            "topic=Tideform rebrand",
            "derived_from=[[[sources/emails/2026/04/2026-04-09-tideform-project-intro]], [[sources/transcripts/2026/04/2026-04-14-tideform-kickoff]], [[sources/docs/2026-04-14-tideform-sow]]]",
        ],
        Some(&wp),
    );
    log_entry(
        s,
        "2026-05-29T17:12:00Z",
        "create",
        "records/projects/tideform-rebrand.md",
        "flagship project synthesis",
    );

    let daniel_body = "# Daniel Osei\n\nHead of Product at [[records/companies/tideform]] and the economic buyer for the Lumen rebrand. He set the phase-one scope and budget, then delegated the day-to-day to [[records/contacts/mara-lindqvist]]. See the kickoff [[records/meetings/2026/04/2026-04-14-tideform-kickoff]] and the project [[records/projects/tideform-rebrand]].\n\nDerived from [[sources/emails/2026/04/2026-04-09-tideform-project-intro]], [[sources/transcripts/2026/04/2026-04-14-tideform-kickoff]], and [[sources/docs/2026-04-14-tideform-sow]].\n";
    let dp = s.body_file(daniel_body);
    write_record(
        s,
        "2026-05-29T17:13:00Z",
        "records/profiles/daniel-osei",
        "profile",
        "Tideform Head of Product; economic buyer on the Lumen rebrand",
        &[
            "meta-type=conclusion",
            "topic=Daniel Osei",
            "derived_from=[[[sources/emails/2026/04/2026-04-09-tideform-project-intro]], [[sources/transcripts/2026/04/2026-04-14-tideform-kickoff]], [[sources/docs/2026-04-14-tideform-sow]]]",
        ],
        Some(&dp),
    );
    log_entry(
        s,
        "2026-05-29T17:13:00Z",
        "create",
        "records/profiles/daniel-osei.md",
        "buyer bio",
    );

    let mara_body = "# Mara Lindqvist\n\nDesign Lead at [[records/companies/tideform]] and Lumen Labs day-to-day contact on the rebrand. She prioritised the component library and organised the brand-asset handover. See the kickoff [[records/meetings/2026/04/2026-04-14-tideform-kickoff]] and the project [[records/projects/tideform-rebrand]].\n\nDerived from [[sources/emails/2026/04/2026-04-09-tideform-project-intro]] and [[sources/transcripts/2026/04/2026-04-14-tideform-kickoff]].\n";
    let mp = s.body_file(mara_body);
    write_record(
        s,
        "2026-05-29T17:14:00Z",
        "records/profiles/mara-lindqvist",
        "profile",
        "Tideform design lead; Lumen's day-to-day contact on the rebrand",
        &[
            "meta-type=conclusion",
            "topic=Mara Lindqvist",
            "derived_from=[[[sources/emails/2026/04/2026-04-09-tideform-project-intro]], [[sources/transcripts/2026/04/2026-04-14-tideform-kickoff]]]",
        ],
        Some(&mp),
    );
    log_entry(
        s,
        "2026-05-29T17:14:00Z",
        "create",
        "records/profiles/mara-lindqvist.md",
        "design-lead bio",
    );

    // ── 6. Validate (working set, back half) + close ─────────────────────────
    let (_v, _code) = s.run("2026-05-29T17:20:00Z", &["--json", "validate"]);
    log_entry(
        s,
        "2026-05-29T17:20:00Z",
        "validate",
        "-",
        "working-set check",
    );
}

/// Issue a `dbmd write` for a content file with `--summary` + `--fm` pairs and
/// an optional `--body-file`, recording the invocation. The `--summary` flag is
/// ALWAYS passed (lifecycle pre-write check #4). Returns the resolved
/// store-relative path the writer printed (JSON `written`).
fn write_record(
    s: &mut Session,
    now: &str,
    path: &str,
    type_: &str,
    summary: &str,
    fm: &[&str],
    body_file: Option<&Path>,
) -> String {
    let mut args: Vec<String> = vec![
        "--json".into(),
        "write".into(),
        path.into(),
        "--type".into(),
        type_.into(),
        "--summary".into(),
        summary.into(),
    ];
    for kv in fm {
        args.push("--fm".into());
        args.push((*kv).into());
    }
    if let Some(bf) = body_file {
        args.push("--body-file".into());
        args.push(bf.to_string_lossy().into_owned());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let stdout = s.run_ok(now, &arg_refs);
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("`dbmd write {path}` did not emit JSON ({e}): {stdout:?}"));
    v["written"].as_str().unwrap_or_default().to_string()
}

/// Append a `dbmd log <kind> <object> -m <note>` entry, recording it. `object`
/// of `"-"` is the store-wide sentinel.
fn log_entry(s: &mut Session, now: &str, kind: &str, object: &str, note: &str) {
    s.run_ok(now, &["--json", "log", kind, object, "-m", note]);
}

// ─────────────────────────────────────────────────────────────────────────────
// 1 — corpus-e end to end: produce the store, assert intent properties AND the
//     byte-for-byte golden.
// ─────────────────────────────────────────────────────────────────────────────

/// The companies the eval REQUIRES (NOTES.md "Summary of the required entity
/// set"): the three counterparties + the studio anchor.
const REQUIRED_COMPANIES: &[&str] = &[
    "records/companies/tideform.md",
    "records/companies/helio-type.md",
    "records/companies/northgate-coffee.md",
    "records/companies/lumen-labs.md",
];

/// The contacts the eval REQUIRES.
const REQUIRED_CONTACTS: &[&str] = &[
    "records/contacts/daniel-osei.md",
    "records/contacts/mara-lindqvist.md",
    "records/contacts/sofia-reyes.md",
    "records/contacts/theo-vance.md",
];

/// The non-contact/company records the eval REQUIRES.
const REQUIRED_EVENT_RECORDS: &[&str] = &[
    "records/meetings/2026/04/2026-04-14-tideform-kickoff.md",
    "records/invoices/2026/04/2026-04-22-helio-type-ht-2026-0417.md",
    "records/expenses/2026/05/2026-05-06-helio-type-1188.md",
];

/// The conclusion records (former wiki pages) the eval REQUIRES, each as
/// `(path, type)` — the project synthesis is a `project`, the bios `profile`s.
/// All carry `meta-type: conclusion`.
const REQUIRED_CONCLUSIONS: &[(&str, &str)] = &[
    ("records/projects/tideform-rebrand.md", "project"),
    ("records/profiles/daniel-osei.md", "profile"),
    ("records/profiles/mara-lindqvist.md", "profile"),
];

/// Addresses that must NOT become contacts (NOTES.md negative cases): bare
/// role / no-reply / own-inbox addresses, and the newsletter sender.
const NON_CONTACT_ADDRESSES: &[&str] = &[
    "billing@heliotype.com",
    "newsletter@designweekly.email",
    "hello@lumenlabs.studio",
    "accounts@lumenlabs.studio",
];

#[test]
fn corpus_e_agent_session_produces_the_expected_store() {
    let (_tmp, mut s) = Session::open_corpus_e();
    run_curator_session(&mut s);
    let store = s.store.clone();

    // ── A. Golden-INDEPENDENT intent properties (NOTES.md + SPEC) ────────────
    // Each required entity exists as a file with the right `type` in frontmatter.
    for (paths, expect_type) in [
        (REQUIRED_COMPANIES, "company"),
        (REQUIRED_CONTACTS, "contact"),
    ] {
        for rel in paths {
            assert_file_type(&store, rel, expect_type);
        }
    }
    assert_file_type(&store, REQUIRED_EVENT_RECORDS[0], "meeting");
    assert_file_type(&store, REQUIRED_EVENT_RECORDS[1], "invoice");
    assert_file_type(&store, REQUIRED_EVENT_RECORDS[2], "expense");
    for (rel, expect_type) in REQUIRED_CONCLUSIONS {
        assert_file_type(&store, rel, expect_type);
        // Each conclusion record carries `meta-type: conclusion`.
        let body = read(&store, rel);
        assert!(
            body.contains("meta-type: conclusion"),
            "{rel} must carry `meta-type: conclusion`; got:\n{body}"
        );
    }

    // The invoice is `paid` with `paid_at` the receipt date (NOTES requires it).
    let invoice = read(&store, REQUIRED_EVENT_RECORDS[1]);
    assert!(
        invoice.contains("status: paid"),
        "the Helio Type invoice must be status: paid (the receipt confirms payment); got:\n{invoice}"
    );
    assert!(
        invoice.contains("paid_at: '2026-05-06'") || invoice.contains("paid_at: 2026-05-06"),
        "the invoice must carry paid_at: 2026-05-06; got:\n{invoice}"
    );

    // The expense links to the invoice it paid (DB.md invoice→expense rule).
    let expense = read(&store, REQUIRED_EVENT_RECORDS[2]);
    assert!(
        expense.contains("[[records/invoices/2026/04/2026-04-22-helio-type-ht-2026-0417]]"),
        "the expense must wiki-link the invoice it settles; got:\n{expense}"
    );

    // Every contact links its company via a full-path wiki-link.
    for rel in REQUIRED_CONTACTS {
        let c = read(&store, rel);
        assert!(
            c.contains("company: '[[records/companies/")
                || c.contains("company: \"[[records/companies/"),
            "{rel} must link its company via a full-path wiki-link; got:\n{c}"
        );
    }

    // The flagship project conclusion record links its evidence (records +
    // sources) and states the $45k fee, in British English ("organised").
    let project = read(&store, REQUIRED_CONCLUSIONS[0].0);
    for needle in [
        "[[records/companies/tideform]]",
        "[[records/contacts/daniel-osei]]",
        "[[records/meetings/2026/04/2026-04-14-tideform-kickoff]]",
        "[[sources/transcripts/2026/04/2026-04-14-tideform-kickoff]]",
        "$45,000",
        "organised",
    ] {
        assert!(
            project.contains(needle),
            "the Tideform project conclusion record must contain {needle:?}; got:\n{project}"
        );
    }

    // NEGATIVE: no contact for any bare-role / own-inbox / newsletter address.
    let contacts_dir = store.join("records/contacts");
    let contact_blob = read_all_md(&contacts_dir);
    for addr in NON_CONTACT_ADDRESSES {
        assert!(
            !contact_blob.contains(addr),
            "no contact may be created for the bare-role/own-inbox/newsletter address {addr:?} \
             (DB.md agent instructions); but it appears in records/contacts/"
        );
    }

    // NEGATIVE: nothing in records/ (incl. the conclusion records) is derived
    // from the newsletter, and the Tideform $45k fee is never modelled as an expense.
    let records_blob = read_all_md(&store.join("records"));
    assert!(
        !records_blob.contains("designweekly") && !records_blob.contains("Design Weekly"),
        "no record (incl. conclusion records) may be derived from the newsletter (Ignored type + transient)"
    );
    // No expense names Tideform (the $45k SOW fee is a receivable, not a cost).
    let expenses_blob = read_all_md(&store.join("records/expenses"));
    assert!(
        !expenses_blob.to_lowercase().contains("tideform"),
        "the Tideform $45k fee must NOT be modelled as an expense (it is a receivable)"
    );

    // The full index hierarchy + a well-formed log exist.
    for rel in ["index.md", "log.md", "sources/index.md", "records/index.md"] {
        assert!(
            store.join(rel).is_file(),
            "{rel} must exist after the session"
        );
    }
    for type_folder in [
        "sources/docs",
        "sources/emails",
        "sources/transcripts",
        "records/companies",
        "records/contacts",
        "records/meetings",
        "records/invoices",
        "records/expenses",
        "records/profiles",
        "records/projects",
    ] {
        assert!(
            store.join(type_folder).join("index.md").is_file(),
            "{type_folder}/index.md (type-folder index) must exist"
        );
        assert!(
            store.join(type_folder).join("index.jsonl").is_file(),
            "{type_folder}/index.jsonl (the complete twin) must exist"
        );
    }

    // ── B. validate --all is clean (zero errors / zero warnings) ─────────────
    // The lone expected signal is the `info`-level POLICY_IGNORED_TYPE_PRESENT
    // for the newsletter source — asserted explicitly, never silently ignored.
    let (vout, vcode) = s.run("2026-05-29T17:25:00Z", &["--json", "validate", "--all"]);
    assert_eq!(
        vcode, 0,
        "validate --all must exit 0 (zero errors); stdout:\n{vout}"
    );
    let report: serde_json::Value = serde_json::from_str(vout.trim())
        .unwrap_or_else(|e| panic!("validate --all must emit JSON ({e}): {vout:?}"));
    assert_eq!(
        report["summary"]["errors"], 0,
        "zero errors required; report:\n{report:#}"
    );
    assert_eq!(
        report["summary"]["warnings"], 0,
        "zero warnings required; report:\n{report:#}"
    );
    let issues = report["issues"].as_array().expect("issues array");
    assert_eq!(
        issues.len(),
        1,
        "exactly one (info) issue expected; report:\n{report:#}"
    );
    assert_eq!(issues[0]["severity"], "info");
    assert_eq!(issues[0]["code"], "POLICY_IGNORED_TYPE_PRESENT");
    assert!(
        issues[0]["file"]
            .as_str()
            .unwrap_or_default()
            .contains("designweekly-digest"),
        "the lone info must be the newsletter source; got {}",
        issues[0]
    );

    // ── C. Byte-for-byte golden — the produced store equals EXPECTED/ ────────
    // Catches any regression in the write / index-write-through / log-append /
    // canonical-serialization paths. (Run AFTER validate --all so the validate
    // call's working-set bookkeeping does not perturb the comparison — the
    // `index rebuild` during warm-up means validate --all is read-only here.)
    assert_store_matches_golden(&store);
}

/// Compare the produced store against `corpus-e-agent/EXPECTED/` byte-for-byte.
/// Only the agent-produced files are golden (records/, the index hierarchy,
/// log.md); `DB.md` + `sources/**` content files are inputs and are excluded
/// from the golden tree, so we compare exactly the set EXPECTED ships.
fn assert_store_matches_golden(store: &Path) {
    let expected = corpora_dir().join("corpus-e-agent").join("EXPECTED");
    assert!(
        expected.is_dir(),
        "the golden tree {} must be committed",
        expected.display()
    );

    // Every file under EXPECTED/ (minus README.md, which documents the golden
    // and is not part of the store) must exist in the store and be byte-equal.
    let mut golden_rels: BTreeSet<PathBuf> = BTreeSet::new();
    for rel in walk_rel(&expected) {
        if rel == Path::new("README.md") {
            continue;
        }
        golden_rels.insert(rel.clone());
        let got = store.join(&rel);
        let want_bytes = std::fs::read(expected.join(&rel))
            .unwrap_or_else(|e| panic!("read golden {}: {e}", rel.display()));
        let got_bytes = std::fs::read(&got).unwrap_or_else(|_| {
            panic!(
                "the produced store is missing {} which EXPECTED/ pins",
                rel.display()
            )
        });
        assert!(
            want_bytes == got_bytes,
            "BYTE MISMATCH vs golden at {}:\n--- EXPECTED ---\n{}\n--- GOT ---\n{}",
            rel.display(),
            String::from_utf8_lossy(&want_bytes),
            String::from_utf8_lossy(&got_bytes),
        );
    }

    // Converse: the store must not have produced any records/index/log file
    // the golden does NOT account for (a stray extra write is a regression too).
    // We scope this to the agent-produced surface (skip DB.md + source CONTENT).
    for rel in walk_rel(store) {
        let first = rel.iter().next().and_then(|c| c.to_str()).unwrap_or("");
        let is_index_or_log = rel
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n == "index.md" || n == "index.jsonl" || n == "log.md")
            .unwrap_or(false);
        let in_records = first == "records";
        // Source CONTENT files are inputs; only the source INDEX files are golden.
        let is_source_index = first == "sources" && is_index_or_log;
        let golden_governed = in_records
            || is_source_index
            || (rel == Path::new("index.md"))
            || (rel == Path::new("log.md"));
        if golden_governed {
            assert!(
                golden_rels.contains(&rel),
                "the store produced {} which the golden does not pin — \
                 update EXPECTED/ or fix the writer",
                rel.display()
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 2 — session-lifecycle assertions over the recorded command log.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn corpus_e_command_log_satisfies_the_session_lifecycle() {
    let (_tmp, mut s) = Session::open_corpus_e();
    run_curator_session(&mut s);
    let log = &s.log;
    assert!(
        !log.is_empty(),
        "the session must have recorded invocations"
    );

    // Step 2 — the FIRST dbmd call is `log tail` (or `log since`).
    let first = &log[0];
    assert_eq!(first.verb(), Some("log"), "first call must be a `log` read");
    assert!(
        matches!(first.subverb(), Some("tail") | Some("since")),
        "first call must be `log tail` or `log since`, got `log {:?}`",
        first.subverb()
    );

    // Step 3 — for every `write` of a CONTACT, a preceding `fm query email=…`
    // exists earlier in the log (pre-write dedup check #1).
    for (i, inv) in log.iter().enumerate() {
        if is_contact_write(inv) {
            let has_preceding_email_query = log[..i].iter().any(|p| {
                p.verb() == Some("fm") && p.subverb() == Some("query") && p.arg_contains("email=")
            });
            assert!(
                has_preceding_email_query,
                "contact write at index {i} ({:?}) has no preceding `fm query email=…`",
                inv.args
            );
        }
    }

    // Step 3 — ZERO short-form wiki-links in any write / fm set (full paths
    // only). Factored into a shared guard so it can also be exercised against a
    // log that actually contains `fm set` invocations (see the focused
    // `fm_set_and_rename_*` test) — the curator session here happens to carry no
    // `fm set`, so the `fm set` arm would otherwise never be hit.
    assert_no_short_form_wiki_links(log);

    // Step 3 — every CONTENT-file `write` passes `--summary` (pre-write #4).
    for inv in log {
        if inv.verb() == Some("write") {
            assert!(
                inv.args.iter().any(|a| a == "--summary"),
                "content write {:?} must pass --summary",
                inv.args
            );
        }
    }

    // Step 3 — PER-MUTATION logging discipline: a `log <kind>` append follows
    // EVERY `write` *and* every `rename`, *immediately* (NOTES.md § Session
    // lifecycle: "a `dbmd log <kind> <object>` follows every `dbmd write` /
    // `dbmd rename`"). A weaker "some log append exists somewhere after this
    // mutation" is vacuous: the closing `log validate` append satisfies it for
    // every mutation regardless of whether each is individually logged.
    //
    // (a) Immediacy — factored into a shared guard so the `rename` arm, which the
    //     curator session never produces, is genuinely exercised by the focused
    //     `fm_set_and_rename_*` test rather than sitting vacuous here.
    assert_mutations_immediately_logged(log);

    // (b) Count parity — the number of `create`-kind `log` appends equals the
    //     number of writes. Each content write logs exactly one `log create`, so
    //     a dropped or duplicated write-log breaks the equality even if some
    //     OTHER append (the `ingest` warm-up or `validate` close) happens to sit
    //     adjacent. The `ingest`/`validate` appends are deliberately NOT
    //     `create`-kind, so they do not pad this count. (Curator-session-specific:
    //     this session performs only `write` mutations, all logged `log create`.)
    let write_count = log.iter().filter(|inv| inv.verb() == Some("write")).count();
    let create_log_appends = log
        .iter()
        .filter(|inv| inv.verb() == Some("log") && inv.subverb() == Some("create"))
        .count();
    assert_eq!(
        create_log_appends, write_count,
        "expected exactly one `log create` append per write ({write_count} writes), found \
         {create_log_appends} `log create` appends — per-write logging discipline is broken",
    );

    // Step 4 — at least one `validate` (working set) ran in the SECOND half.
    let half = log.len() / 2;
    let validate_in_back_half = log[half..].iter().any(|inv| inv.verb() == Some("validate"));
    assert!(
        validate_in_back_half,
        "a `validate` must run in the second half of the session (step 4)"
    );

    // Step 5 — ZERO `index rebuild` calls in the OPERATING LOOP. The one allowed
    // rebuild is during warm-up (the bulk-external-drop fold), which must occur
    // BEFORE the first content write. Any rebuild AT/AFTER the first write fails.
    let first_write = log
        .iter()
        .position(|inv| inv.verb() == Some("write"))
        .expect("the session performs writes");
    let rebuilds: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, inv)| inv.verb() == Some("index") && inv.subverb() == Some("rebuild"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        rebuilds.len(),
        1,
        "exactly one `index rebuild` (the warm-up sources fold) is expected; found at {rebuilds:?}"
    );
    assert!(
        rebuilds[0] < first_write,
        "the `index rebuild` (idx {}) must be in warm-up, before the first write (idx {first_write}) — \
         no rebuild in the operating loop",
        rebuilds[0]
    );

    // Step 6 — the FINAL recorded call is a well-formed `log` append (close).
    let last = log.last().unwrap();
    assert!(
        is_log_append(last),
        "the final recorded call must be a `log <kind>` append (close), got {:?}",
        last.args
    );

    // Sanity: every recorded invocation exited successfully (the lifecycle is a
    // sequence of CORRECT operations — a crash mid-session is the worst
    // regression). The single non-success-allowed call (`validate` may exit
    // non-zero if it finds issues) DID exit 0 here, so a blanket check holds.
    for inv in log {
        assert_eq!(
            inv.exit_code, 0,
            "every lifecycle call must exit 0; {:?} exited {}",
            inv.args, inv.exit_code
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 2b — focused lifecycle coverage for `fm set` + `rename`.
//
// The scripted curator session (`run_curator_session`) exercises `write` / `fm
// query` / `log`, but never `dbmd fm set` or `dbmd rename` — so the two
// lifecycle guards that name those commands (`assert_no_short_form_wiki_links`'s
// `fm set` arm and `assert_mutations_immediately_logged`'s `rename` arm, per
// NOTES.md § Session lifecycle: "follows every `dbmd write` / `dbmd rename`")
// would otherwise be evaluated against an empty input set and pass vacuously.
//
// This test drives REAL `fm set` and `rename` invocations through the same
// release binary + recording `Session`, then asserts BOTH directions:
//   (1) the guards PASS on a valid log that contains a full-path `fm set` value
//       and a `rename` immediately followed by a `log rename` append; AND
//   (2) the guards' non-panicking `check_*` cores return `Err` on adversarial
//       logs — a short-form `[[name]]` in an `fm set`, and a `rename` NOT
//       followed by a `log` — proving they actually bite the failure each is
//       meant to catch (without touching the process-global panic hook).
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn fm_set_and_rename_lifecycle_guards_fire_on_real_invocations() {
    let (_tmp, mut s) = Session::open_corpus_e();

    // ── Minimal real session: warm up, write a company + a contact (the rename
    //    target + its link), then `fm set` a full-path link and `rename` the
    //    contact, logging each mutation immediately. Every call hits the release
    //    binary, so the recorded log is exactly what a real agent would produce.
    s.run_ok("2026-05-29T17:00:00Z", &["--json", "log", "tail", "20"]);
    s.run_ok("2026-05-29T17:00:30Z", &["--json", "index", "rebuild"]);

    write_record(
        &mut s,
        "2026-05-29T17:01:00Z",
        "records/companies/tideform",
        "company",
        "Tideform — phase-one rebrand client",
        &[
            "name=Tideform",
            "domain=tideform.com",
            "industry=Consumer mobile app",
            "relationship=client",
        ],
        None,
    );
    log_entry(
        &mut s,
        "2026-05-29T17:01:00Z",
        "create",
        "records/companies/tideform.md",
        "client company",
    );

    // `fm query` dedup precedes the contact write (Step 3 #1).
    s.run_ok(
        "2026-05-29T17:02:00Z",
        &[
            "--json",
            "fm",
            "query",
            "email=daniel.osei@tideform.com",
            "--type",
            "contact",
        ],
    );
    write_record(
        &mut s,
        "2026-05-29T17:02:00Z",
        "records/contacts/daniel-osei",
        "contact",
        "Head of Product at Tideform",
        &[
            "name=Daniel Osei",
            "email=daniel.osei@tideform.com",
            "role=Head of Product",
            "company=[[records/companies/tideform]]",
        ],
        None,
    );
    log_entry(
        &mut s,
        "2026-05-29T17:02:00Z",
        "create",
        "records/contacts/daniel-osei.md",
        "Tideform buyer",
    );

    // `fm set` with a FULL-PATH wiki-link value (re-affirm the company link), then
    // log the update — exercises the `fm set` arm of the short-form guard with a
    // VALID link, and the `fm set` write-through path against a real store.
    let set_out = s.run_ok(
        "2026-05-29T17:03:00Z",
        &[
            "--json",
            "fm",
            "set",
            "records/contacts/daniel-osei.md",
            "company=[[records/companies/tideform]]",
        ],
    );
    let set_json: serde_json::Value = serde_json::from_str(set_out.trim())
        .unwrap_or_else(|e| panic!("`fm set` must emit JSON ({e}): {set_out:?}"));
    assert_eq!(
        set_json["index_updated"], true,
        "fm set must keep the index write-through current"
    );
    log_entry(
        &mut s,
        "2026-05-29T17:03:00Z",
        "update",
        "records/contacts/daniel-osei.md",
        "re-affirmed company link",
    );

    // `rename` the contact, then log it (NOTES.md: a `log` follows every rename).
    let rename_out = s.run_ok(
        "2026-05-29T17:04:00Z",
        &[
            "--json",
            "rename",
            "records/contacts/daniel-osei.md",
            "records/contacts/daniel-osei-hop.md",
        ],
    );
    let rename_json: serde_json::Value = serde_json::from_str(rename_out.trim())
        .unwrap_or_else(|e| panic!("`rename` must emit JSON ({e}): {rename_out:?}"));
    assert_eq!(
        rename_json["renamed"]["to"], "records/contacts/daniel-osei-hop.md",
        "rename must report the destination it moved to"
    );
    assert!(
        rename_json["links_rewritten"].is_number(),
        "rename must report a links_rewritten count; got {rename_json}"
    );
    assert!(
        s.store
            .join("records/contacts/daniel-osei-hop.md")
            .is_file()
            && !s.store.join("records/contacts/daniel-osei.md").is_file(),
        "rename must move the file on disk"
    );
    log_entry(
        &mut s,
        "2026-05-29T17:04:00Z",
        "rename",
        "records/contacts/daniel-osei-hop.md",
        "renamed contact to reflect role",
    );

    // The store is still clean after the fm-set + rename write-through (proves the
    // mutations kept indexes + links valid, not just that they ran).
    let (_v, vcode) = s.run("2026-05-29T17:05:00Z", &["--json", "validate", "--all"]);
    assert_eq!(
        vcode, 0,
        "validate --all must be clean after fm set + rename"
    );

    let log = &s.log;

    // ── Sanity: the log actually contains a real `fm set` and a real `rename`
    //    (otherwise this test would itself be vacuous — the exact trap it guards).
    assert!(
        log.iter()
            .any(|i| i.verb() == Some("fm") && i.subverb() == Some("set")),
        "the focused session must record at least one `fm set`"
    );
    assert!(
        log.iter().any(is_rename),
        "the focused session must record at least one `rename`"
    );

    // ── (1) The guards PASS on this valid log (no short-form links; every
    //    write/rename immediately logged).
    assert_no_short_form_wiki_links(log);
    assert_mutations_immediately_logged(log);

    // ── (2) The guards FIRE on adversarial logs — proving the `fm set` arm and
    //    the `rename` arm are not vacuous. We build minimal hand-rolled logs and
    //    assert each guard's non-panicking `check_*` core returns `Err`. (Using
    //    the `check_*` core rather than `catch_unwind` on the panicking wrapper
    //    keeps this test from touching the process-global panic hook, which would
    //    race the other tests running concurrently in this binary.)

    // (2a) A short-form `[[name]]` (no `/`) in an `fm set` value must be rejected
    //      by the short-form guard's `fm set` arm.
    let bad_fm_set = vec![Invocation {
        args: vec![
            "--json".into(),
            "fm".into(),
            "set".into(),
            "records/contacts/daniel-osei.md".into(),
            "company=[[tideform]]".into(), // short-form: no `/`
        ],
        exit_code: 0,
    }];
    let err = check_no_short_form_wiki_links(&bad_fm_set)
        .expect_err("a short-form wiki-link in an `fm set` value must be rejected");
    assert!(
        err.contains("[[tideform]]") || err.contains("tideform"),
        "the rejection must name the offending short-form link; got {err:?}"
    );

    // (2b) A `rename` NOT followed by any `log` append must be rejected by the
    //      per-mutation logging guard's `rename` arm. (A trailing `fm query` read
    //      is not a `log` append.)
    let unlogged_rename = vec![
        Invocation {
            args: vec![
                "rename".into(),
                "records/contacts/a.md".into(),
                "records/contacts/b.md".into(),
            ],
            exit_code: 0,
        },
        Invocation {
            args: vec!["fm".into(), "query".into(), "email=x@y.z".into()],
            exit_code: 0,
        },
    ];
    let err = check_mutations_immediately_logged(&unlogged_rename)
        .expect_err("a rename not immediately followed by a `log` append must be rejected");
    assert!(
        err.contains("rename"),
        "the rejection must name the unlogged rename; got {err:?}"
    );
}

/// `true` if `inv` is a `dbmd write … --type contact …`.
fn is_contact_write(inv: &Invocation) -> bool {
    if inv.verb() != Some("write") {
        return false;
    }
    // Find the value following `--type`.
    let mut it = inv.args.iter();
    while let Some(a) = it.next() {
        if a == "--type" {
            return it.next().map(String::as_str) == Some("contact");
        }
    }
    false
}

/// `true` if `inv` is a `log` APPEND (a `log <kind>` where `<kind>` is not a
/// read sub-verb). `log tail` / `log since` are reads, not appends.
fn is_log_append(inv: &Invocation) -> bool {
    inv.verb() == Some("log") && !matches!(inv.subverb(), Some("tail") | Some("since") | None)
}

/// Extract the `target` of every `[[target]]` / `[[target|display]]` occurrence
/// in a single CLI argument string. Used to assert full-path-only links.
fn extract_wiki_link_targets(arg: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = arg.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if let Some(close) = arg[i + 2..].find("]]") {
                let inner = &arg[i + 2..i + 2 + close];
                let target = inner.split('|').next().unwrap_or(inner).trim();
                if !target.is_empty() {
                    out.push(target.to_string());
                }
                i = i + 2 + close + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `true` if `inv` is a `dbmd rename <old> <new>`.
fn is_rename(inv: &Invocation) -> bool {
    inv.verb() == Some("rename")
}

/// Shared guard — Step 3 link discipline: ZERO short-form wiki-links in any
/// `write` OR `fm set` value (full store-relative paths only; a short-form link
/// is `[[name]]` with no `/`). Scans every arg of each such invocation for a
/// `[[…]]` target and asserts it contains `/`.
///
/// Both write surfaces that take a wiki-link value (`write --fm k=[[…]]` and
/// `fm set <file> k=[[…]]`) are covered. Pulled out of the curator-log test so
/// the `fm set` arm — which that session never produces — is exercised by the
/// focused `fm_set_and_rename_*` test against a log that actually contains one.
///
/// Split into a non-panicking `check_*` core (returns the first offending
/// invocation's message) and an `assert_*` wrapper, so the focused test can
/// assert the guard *fires* by inspecting an `Err` rather than swapping the
/// process-global panic hook (which would race concurrent tests).
fn check_no_short_form_wiki_links(log: &[Invocation]) -> Result<(), String> {
    for inv in log {
        let is_write = inv.verb() == Some("write");
        let is_fm_set = inv.verb() == Some("fm") && inv.subverb() == Some("set");
        if is_write || is_fm_set {
            for a in &inv.args {
                for link in extract_wiki_link_targets(a) {
                    if !link.contains('/') {
                        return Err(format!(
                            "short-form wiki-link {link:?} in {:?} — full store-relative paths only",
                            inv.args
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn assert_no_short_form_wiki_links(log: &[Invocation]) {
    if let Err(msg) = check_no_short_form_wiki_links(log) {
        panic!("{msg}");
    }
}

/// Shared guard — Step 3 per-mutation logging discipline: a `log <kind>` append
/// follows EVERY `write` *and* every `rename`, **immediately** (NOTES.md §
/// Session lifecycle: "a `dbmd log <kind> <object>` follows every `dbmd write` /
/// `dbmd rename`"). The immediate-next-call form is the non-vacuous one: a
/// weaker "some append exists later" is satisfied by the closing `log validate`
/// for every mutation regardless of whether each is individually logged.
///
/// The `rename` arm is the coverage the curator session lacks; the focused
/// `fm_set_and_rename_*` test drives a real `rename` through this guard. Same
/// `check_*`/`assert_*` split as above, for the same panic-hook-free reason.
fn check_mutations_immediately_logged(log: &[Invocation]) -> Result<(), String> {
    for (i, inv) in log.iter().enumerate() {
        if inv.verb() == Some("write") || is_rename(inv) {
            let next = log.get(i + 1);
            if !next.is_some_and(is_log_append) {
                return Err(format!(
                    "{} at index {i} ({:?}) is not IMMEDIATELY followed by a `log <kind>` append; \
                     next call was {:?} — each write/rename must be logged right after it",
                    inv.verb().unwrap_or("?"),
                    inv.args,
                    next.map(|n| &n.args),
                ));
            }
        }
    }
    Ok(())
}

fn assert_mutations_immediately_logged(log: &[Invocation]) {
    if let Err(msg) = check_mutations_immediately_logged(log) {
        panic!("{msg}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 3 — supporting evals.
// ─────────────────────────────────────────────────────────────────────────────

/// One case in the corpus-a agent-eval search golden.
#[derive(serde::Deserialize)]
struct SearchCase {
    query: String,
    #[serde(default)]
    args: Vec<String>,
    matches: Vec<String>,
}

#[derive(serde::Deserialize)]
struct SearchGolden {
    queries: Vec<SearchCase>,
}

/// `dbmd search` over corpus-a: 20 representative queries (incl. `--type` /
/// `--in` / `--updated-after`) each return EXACTLY the golden file set, in both
/// text and `--json` modes, driving the release binary.
#[test]
fn search_eval_over_corpus_a_matches_golden() {
    let bin = release_dbmd();
    let golden_path = corpus_a().join("EXPECTED").join("search-agent-eval.json");
    let raw = std::fs::read_to_string(&golden_path)
        .unwrap_or_else(|_| panic!("{} is committed", golden_path.display()));
    let golden: SearchGolden =
        serde_json::from_str(&raw).expect("search-agent-eval.json is valid JSON");
    assert_eq!(
        golden.queries.len(),
        20,
        "the agent-eval search golden pins exactly 20 representative queries"
    );

    // Coverage guarantee: the 20 must collectively exercise --type, --in, AND
    // --updated-after (the plan's explicit requirement).
    let all_args: Vec<&str> = golden
        .queries
        .iter()
        .flat_map(|q| q.args.iter().map(String::as_str))
        .collect();
    for required in ["--type", "--in", "--updated-after"] {
        assert!(
            all_args.contains(&required),
            "the 20-query set must include at least one {required} case"
        );
    }

    for case in &golden.queries {
        let want: BTreeSet<&str> = case.matches.iter().map(String::as_str).collect();

        // Text mode: `file:line: text`; collect the distinct file column.
        let text_out = run_capture(&bin, &corpus_a(), {
            let mut v = vec!["search", case.query.as_str()];
            v.extend(case.args.iter().map(String::as_str));
            v
        });
        let got_text: BTreeSet<String> = text_out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.split(':').next().unwrap_or("").to_string())
            .collect();
        let got_text_refs: BTreeSet<&str> = got_text.iter().map(String::as_str).collect();
        assert_eq!(
            got_text_refs, want,
            "text-mode search {:?} {:?} returned the wrong file set",
            case.query, case.args
        );

        // JSON mode: an array of objects each with a `file` (or `path`) field.
        let json_out = run_capture(&bin, &corpus_a(), {
            let mut v = vec!["--json", "search", case.query.as_str()];
            v.extend(case.args.iter().map(String::as_str));
            v
        });
        let parsed: serde_json::Value = serde_json::from_str(json_out.trim()).unwrap_or_else(|e| {
            panic!(
                "search --json {:?} {:?} must emit JSON ({e}): {json_out:?}",
                case.query, case.args
            )
        });
        let got_json: BTreeSet<String> = json_match_files(&parsed);
        let got_json_refs: BTreeSet<&str> = got_json.iter().map(String::as_str).collect();
        assert_eq!(
            got_json_refs, want,
            "json-mode search {:?} {:?} returned the wrong file set",
            case.query, case.args
        );
    }
}

/// `dbmd validate --all` over corpus-b reproduces EXACTLY the committed
/// `EXPECTED/validate.json` issue set (the designed-to-fail store's contract).
#[test]
fn validate_eval_over_corpus_b_matches_golden() {
    let bin = release_dbmd();
    let out = run_capture(&bin, &corpus_b(), vec!["--json", "validate", "--all"]);
    let report: serde_json::Value =
        serde_json::from_str(out.trim()).expect("validate --all emits JSON on corpus-b");

    let golden: serde_json::Value = {
        let raw = std::fs::read_to_string(corpus_b().join("EXPECTED").join("validate.json"))
            .expect("corpus-b EXPECTED/validate.json is committed");
        serde_json::from_str(&raw).expect("EXPECTED/validate.json is valid JSON")
    };

    // Compare the summary tallies and the issue multiset (the stable fields:
    // code, severity, file, line, key). Ordering is not contractual.
    assert_eq!(
        report["summary"], golden["summary"],
        "corpus-b validate --all summary must match the golden"
    );
    let live = issue_multiset(&report["issues"]);
    let want = issue_multiset(&golden["issues"]);
    assert_eq!(
        live, want,
        "corpus-b validate --all issue set must equal EXPECTED/validate.json"
    );
    // Sanity: this is the broken store — it MUST report errors (a clean result
    // would mean validation silently went blind).
    assert!(
        report["summary"]["errors"].as_u64().unwrap_or(0) > 0,
        "corpus-b is the designed-to-fail store; validate must report errors"
    );
}

/// `dbmd extract` over corpus-c: each text-bearing fixture's output matches its
/// known-good `.txt` (token-normalized — decoders agree on words, differ on
/// layout), the image-only PDF yields empty, and the encrypted PDF is refused.
/// (The exhaustive per-fixture pass lives in `extract_e2e.rs`; this is the
/// agent-eval slice the plan names — `dbmd extract over corpus-c diffed vs
/// known-good`.)
#[test]
fn extract_eval_over_corpus_c_matches_known_good() {
    let bin = release_dbmd();
    let docs = corpora_dir()
        .join("corpus-c-formats")
        .join("sources")
        .join("docs");

    // Text-bearing fixtures compared token-normalized (whitespace-run-agnostic).
    for fixture in [
        "text.pdf",
        "weird-fonts.pdf",
        "sample.docx",
        "sample.xlsx",
        "sample.epub",
        "sample.html",
    ] {
        let doc = docs.join(fixture);
        let known = docs.join(format!("{fixture}.txt"));
        assert!(doc.is_file(), "corpus-c fixture {fixture} must exist");
        assert!(
            known.is_file(),
            "corpus-c known-good {fixture}.txt must exist"
        );

        let out = run_capture(&bin, &docs, vec!["extract", doc.to_str().unwrap()]);
        let want = std::fs::read_to_string(&known).expect("read known-good");
        assert_eq!(
            normalize_tokens(&out),
            normalize_tokens(&want),
            "extract of {fixture} disagrees (token-normalized) with its known-good .txt"
        );
        assert!(
            !normalize_tokens(&out).is_empty(),
            "extract of {fixture} produced no text"
        );
    }

    // image-only.pdf: no text layer → empty out, never hallucinated text.
    let image_only = docs.join("image-only.pdf");
    if image_only.is_file() {
        let out = run_capture(&bin, &docs, vec!["extract", image_only.to_str().unwrap()]);
        assert!(
            out.trim().is_empty(),
            "image-only.pdf must extract to empty (no hallucinated text); got {out:?}"
        );
    }

    // encrypted.pdf: must FAIL cleanly with DOCUMENT_ENCRYPTED, emit nothing.
    let encrypted = docs.join("encrypted.pdf");
    if encrypted.is_file() {
        let output = StdCommand::new(&bin)
            .args(["--json", "extract", encrypted.to_str().unwrap()])
            .current_dir(&docs)
            .output()
            .expect("spawn dbmd extract on encrypted.pdf");
        assert!(
            !output.status.success(),
            "an encrypted PDF must be refused (non-zero exit)"
        );
        assert!(
            output.stdout.is_empty(),
            "a refused extract must emit nothing to stdout"
        );
        let err: serde_json::Value =
            serde_json::from_str(String::from_utf8_lossy(&output.stderr).trim())
                .expect("encrypted-refusal error is JSON under --json");
        assert_eq!(
            err["error"]["code"], "DOCUMENT_ENCRYPTED",
            "the refusal must carry the DOCUMENT_ENCRYPTED code; got {}",
            err["error"]
        );
    }
}

/// Policy-refusal eval: the agent attempts a `write` against corpus-b's frozen
/// page; it is refused with structured `POLICY_FROZEN_PAGE`, exits non-zero, the
/// file is byte-identical, and the recovery move is one of the two valid options
/// (escalate, or write to an alternate path).
#[test]
fn policy_refusal_eval_refuses_and_leaves_file_byte_identical() {
    let bin = release_dbmd();
    // Operate on a temp copy so the committed corpus is never touched.
    let (_tmp, store) = copy_store_to_temp(&corpus_b());
    let frozen_rel = "records/decisions/2026-q1-strategy.md";
    let frozen_abs = store.join(frozen_rel);
    assert!(
        frozen_abs.is_file(),
        "the frozen fixture must exist in corpus-b"
    );
    let before = std::fs::read(&frozen_abs).expect("read frozen before");

    // The agent attempts to overwrite the frozen decision page.
    let output = StdCommand::new(&bin)
        .args([
            "--json",
            "write",
            "records/decisions/2026-q1-strategy",
            "--type",
            "decision",
            "--summary",
            "overwrite attempt",
        ])
        .current_dir(&store)
        .output()
        .expect("spawn dbmd write on frozen page");

    // Refused: non-zero exit (exit 4 = ExitCode::Policy), structured error.
    assert!(
        !output.status.success(),
        "a write to a frozen page must be refused (non-zero exit)"
    );
    assert_eq!(
        output.status.code(),
        Some(4),
        "a frozen-page refusal exits 4 (ExitCode::Policy)"
    );
    assert!(
        output.stdout.is_empty(),
        "a refused write must print nothing to stdout (no success object)"
    );
    let err: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stderr).trim())
            .expect("the refusal error is JSON under --json");
    assert_eq!(
        err["error"]["code"], "POLICY_FROZEN_PAGE",
        "the refusal must carry the structured POLICY_FROZEN_PAGE code; got {}",
        err["error"]
    );
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(frozen_rel),
        "the refusal message must name the frozen path {frozen_rel:?}; got {}",
        err["error"]["message"]
    );

    // The frozen file is byte-for-byte unchanged (no write occurred).
    let after = std::fs::read(&frozen_abs).expect("read frozen after");
    assert!(
        before == after,
        "the frozen page must be byte-for-byte unchanged after a refused write"
    );

    // Recovery: writing to an ALTERNATE (non-frozen) path succeeds — one of the
    // two valid recovery moves (the other, escalate-to-operator, is a no-op
    // against the store). This proves the refusal is path-scoped, not a wedge.
    let recover = StdCommand::new(&bin)
        .args([
            "--json",
            "write",
            "records/decisions/2026-q1-strategy-revised",
            "--type",
            "decision",
            "--summary",
            "revised strategy (alternate, non-frozen path)",
        ])
        .current_dir(&store)
        .env("DBMD_NOW", "2026-05-29T18:00:00Z")
        .output()
        .expect("spawn dbmd write on the alternate path");
    assert!(
        recover.status.success(),
        "writing to a non-frozen alternate path must succeed (valid recovery); stderr:\n{}",
        String::from_utf8_lossy(&recover.stderr)
    );
    assert!(
        store
            .join("records/decisions/2026-q1-strategy-revised.md")
            .is_file(),
        "the alternate-path recovery write must have created the file"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 4 — perf 1M tier (opt-in, #[ignore]). The 10k tier lives in perf_budget.rs.
//     This is the documented, opt-in scale gate (tests/PERF.md "1M tier").
//     It is NEVER generated/run in CI — only via `-- --ignored perf_1m`.
// ─────────────────────────────────────────────────────────────────────────────

/// Opt-in 1M-tier perf gate: generate the ~1M-file `corpus-d-scale` and assert
/// the loop ops stay flat in store size (within the plan's 1M budgets) while the
/// sweep ops stay within their linear budgets. Minutes + several GB of disk;
/// `#[ignore]` so `cargo test` never runs it. Invoke explicitly:
/// `cargo test -p dbmd-cli --test agent_eval -- --ignored perf_1m`.
#[test]
#[ignore = "1M-tier perf: opt-in only (minutes + GB of disk); run with `-- --ignored perf_1m`"]
fn perf_1m_loop_ops_stay_flat_and_sweeps_stay_in_budget() {
    use std::time::{Duration, Instant};

    let bin = release_dbmd();
    let tmp = tempfile::TempDir::new().expect("tempdir for the 1M scale corpus");

    // 1. Compile + run the std-only generator at the `1m` tier.
    let gen_src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("gen-scale.rs");
    assert!(
        gen_src.is_file(),
        "tests/gen-scale.rs (the scale generator) must exist"
    );
    let gen_bin = tmp.path().join(if cfg!(windows) {
        "gen-scale.exe"
    } else {
        "gen-scale"
    });
    let compile = StdCommand::new("rustc")
        .args(["-O"])
        .arg(&gen_src)
        .arg("-o")
        .arg(&gen_bin)
        .status()
        .expect("compile gen-scale.rs");
    assert!(
        compile.success(),
        "gen-scale.rs must compile with `rustc -O`"
    );
    let store = tmp.path().join("corpus-d-scale-1m");
    let run = StdCommand::new(&gen_bin)
        .args(["1m"])
        .arg(&store)
        .arg("--force")
        .status()
        .expect("run gen-scale 1m");
    assert!(
        run.success(),
        "gen-scale 1m must generate the corpus cleanly"
    );

    // 2. Reach the index-rebuild fixed point (same precondition as the 10k gate)
    //    so the read-only sweeps time against a valid store.
    let rebuild = StdCommand::new(&bin)
        .args(["index", "rebuild"])
        .current_dir(&store)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("index rebuild on the 1M corpus");
    assert!(
        rebuild.success(),
        "index rebuild on the 1M corpus must succeed"
    );

    // 3. Time helper: median of a few subprocess runs (warm cache).
    let time_median = |args: &[&str]| -> Duration {
        for _ in 0..1 {
            let _ = StdCommand::new(&bin)
                .args(args)
                .current_dir(&store)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        let mut samples: Vec<Duration> = (0..3)
            .map(|_| {
                let start = Instant::now();
                let _ = StdCommand::new(&bin)
                    .args(args)
                    .current_dir(&store)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .expect("spawn dbmd");
                start.elapsed()
            })
            .collect();
        samples.sort();
        samples[samples.len() / 2]
    };

    // 4. Loop budgets @1M (plan line 501), with the same CI headroom factor the
    //    10k gate uses; these must stay FLAT in store size.
    const SLACK: u32 = 6;
    let log_tail = time_median(&["log", "tail", "20"]);
    assert!(
        log_tail <= Duration::from_millis(50) * SLACK,
        "log tail 20 @1M {log_tail:?} exceeds the flat budget (50ms × {SLACK})"
    );
    let fm_query = time_median(&["fm", "query", "status=active", "--type", "company"]);
    assert!(
        fm_query <= Duration::from_secs(2) * SLACK,
        "fm query @1M {fm_query:?} exceeds the flat budget (2s × {SLACK})"
    );
    let search = time_median(&["search", "Kickoff", "--type", "email"]);
    assert!(
        search <= Duration::from_secs(2) * SLACK,
        "search --type @1M {search:?} exceeds the flat budget (2s × {SLACK})"
    );

    // 5. Sweep budgets @1M (linear, off-loop).
    let validate_all = time_median(&["validate", "--all"]);
    assert!(
        validate_all <= Duration::from_secs(60) * SLACK,
        "validate --all @1M {validate_all:?} exceeds the linear budget (60s × {SLACK})"
    );
    let stats = time_median(&["stats"]);
    assert!(
        stats <= Duration::from_secs(60) * SLACK,
        "stats @1M {stats:?} exceeds the linear budget (60s × {SLACK})"
    );

    eprintln!(
        "[perf 1M] log_tail={log_tail:?} fm_query={fm_query:?} search={search:?} \
         validate_all={validate_all:?} stats={stats:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Small shared helpers.
// ─────────────────────────────────────────────────────────────────────────────

/// Run `dbmd <args>` with `dir` as cwd, return stdout (lossy UTF-8). Does not
/// assert success — search/validate legitimately exit non-zero with output.
fn run_capture(bin: &Path, dir: &Path, args: Vec<&str>) -> String {
    let out = StdCommand::new(bin)
        .args(&args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("spawn dbmd {args:?}: {e}"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Read a store file to a string (panicking with the path on failure).
fn read(store: &Path, rel: &str) -> String {
    std::fs::read_to_string(store.join(rel))
        .unwrap_or_else(|e| panic!("read {}: {e}", store.join(rel).display()))
}

/// Concatenate the text of every `.md` file directly under (and recursively
/// below) `dir` — for negative substring checks across a folder.
fn read_all_md(dir: &Path) -> String {
    let mut blob = String::new();
    fn walk(dir: &Path, blob: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, blob);
            } else if p.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Ok(s) = std::fs::read_to_string(&p) {
                    blob.push_str(&s);
                    blob.push('\n');
                }
            }
        }
    }
    walk(dir, &mut blob);
    blob
}

/// Assert a store file's frontmatter declares `type: <expect>`.
fn assert_file_type(store: &Path, rel: &str, expect: &str) {
    let abs = store.join(rel);
    assert!(abs.is_file(), "required file {rel} is missing");
    let text = read(store, rel);
    assert!(
        text.contains(&format!("type: {expect}\n")),
        "{rel} must declare `type: {expect}` in frontmatter; got:\n{text}"
    );
}

/// Every file path under `root`, relative to `root`, sorted.
fn walk_rel(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(p.strip_prefix(base).expect("under base").to_path_buf());
            }
        }
    }
    walk(root, root, &mut out);
    out.sort();
    out
}

/// The set of store-relative file paths a `search --json` result names. Search
/// JSON emits an array of objects each with a `file` field (and a `line`).
fn json_match_files(v: &serde_json::Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(f) = item.get("file").and_then(|f| f.as_str()) {
                out.insert(f.to_string());
            } else if let Some(p) = item.get("path").and_then(|p| p.as_str()) {
                out.insert(p.to_string());
            }
        }
    } else if let Some(arr) = v.get("matches").and_then(|m| m.as_array()) {
        // Tolerate a `{matches:[...]}` envelope shape.
        for item in arr {
            if let Some(f) = item.get("file").and_then(|f| f.as_str()) {
                out.insert(f.to_string());
            }
        }
    }
    out
}

/// Normalize a validate `issues` array into a sorted multiset of the stable
/// fields, so two reports compare regardless of issue ordering.
fn issue_multiset(issues: &serde_json::Value) -> Vec<(String, String, String, String, String)> {
    let mut v: Vec<(String, String, String, String, String)> = issues
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|i| {
                    (
                        i["code"].as_str().unwrap_or_default().to_string(),
                        i["severity"].as_str().unwrap_or_default().to_string(),
                        i["file"].as_str().unwrap_or_default().to_string(),
                        i.get("line").map(|l| l.to_string()).unwrap_or_default(),
                        i.get("key").map(|k| k.to_string()).unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Collapse every whitespace run (incl. newlines) to a single space and trim —
/// the layout-agnostic "same words, same order" comparison for extract output.
fn normalize_tokens(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
