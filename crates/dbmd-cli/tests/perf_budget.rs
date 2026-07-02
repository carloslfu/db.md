//! perf_budget.rs — the performance / budget regression guard.
//!
//! WHY THIS EXISTS
//! ---------------
//! The toolkit's central scaling claim is a contract, not a vibe: the plan
//! (`plans/db-md-rust-toolkit.md` § "Performance targets are budgets" and
//! Block 6 § `corpus-d-scale`) and the Hard rules fix concrete budgets at the
//! 10k tier —
//!
//!   * loop ops are O(changed), flat in store size:
//!       - `dbmd fm query`        < 300 ms @10k
//!       - `dbmd search --type`   < 300 ms @10k
//!       - `dbmd log tail 20`     <  50 ms
//!   * sweep ops are O(store), off the loop:
//!       - `dbmd validate --all`  < 5 s  @10k
//!       - `dbmd index rebuild`   < 10 s @10k
//!       - `dbmd stats`           < 5 s  @10k
//!
//! Before this file existed, NOTHING exercised any of that. `tests/gen-scale.rs`
//! generated a 10k-file `corpus-d-scale` store, but it had zero `#[test]`
//! functions, was wired as a Cargo target by no `Cargo.toml`, and was referenced
//! by no test — an orphan generator producing an inert corpus. The budgets in
//! `tests/PERF.md` were measured once by hand and could silently rot on the next
//! refactor with no signal. This test closes that gap: it is the executable
//! contract for the scaling claim, and it de-orphans the generator by being the
//! thing that runs it.
//!
//! HOW IT WORKS
//! ------------
//!  1. **Build the corpus on demand** — honoring `.gitignore`'s stated policy
//!     ("the GENERATOR is tracked; the multi-thousand-file output it produces is
//!     not. CI rebuilds the 10k tier on demand"). We compile `tests/gen-scale.rs`
//!     with `rustc -O` (std-only, zero deps — see its header) into a tempdir and
//!     run its `10k` tier, producing a fresh ~10,021-file store. The generated
//!     store is never committed and never mutates the repo; it lives only for the
//!     test's lifetime under `target/`-adjacent temp space. This is also the only
//!     thing in the suite that compiles + runs `gen-scale.rs`, so a generator
//!     that stops compiling now fails CI instead of bit-rotting unnoticed.
//!  2. **Time the real `dbmd` binary** as a subprocess (the same path an agent
//!     drives), with a warmup pass to prime the page cache (PERF.md method) and a
//!     median over several timed iterations so a single GC/scheduler hiccup does
//!     not flip the verdict. Timing wraps `std::process::Command::status()` only
//!     — the child `dbmd` process spawn IS included (it is part of every real
//!     call); the assertion-builder overhead is not.
//!  3. **Assert the budget** per op. The budgets asserted are the plan's
//!     documented @10k numbers, widened by a fixed CI-variance headroom factor
//!     (`BUDGET_SLACK`) so the test catches an *order-of-magnitude* regression
//!     (the thing that matters: an O(changed) op going O(store), a sweep blowing
//!     past seconds) without flaking on a slow shared CI runner. The honest,
//!     tight measured numbers live in `tests/PERF.md`; this guard is the floor
//!     that must never be crossed, not the benchmark of record.
//!
//! WHAT IT DELIBERATELY DOES NOT ASSERT
//! ------------------------------------
//! `tests/PERF.md` documents ops that were measured OVER budget on the 10k tier
//! (`fm set` / `write` are marginally over an O(folder-jsonl) floor). Those are
//! known, documented gaps with a fix family already written down — asserting
//! them here would make this guard red on `main` and is out of scope for "wire
//! the corpus to a test." This file asserts the budgets the toolkit *currently
//! meets* and that define the architecture's headline promise (sidecar loop
//! reads stay flat; sweeps stay sub-budget). When a fix lands for an over-budget
//! op, add its assertion here — that is the regression net tightening, by design.
//!
//! Two findings that USED to live on the omitted list have been fixed and are
//! now asserted below:
//!   * The unscoped `graph backlinks` / `graph neighborhood` O(store) finding —
//!     both now ride a single embedded-ripgrep pass (the same scan class as
//!     `search` free-text); see `BUDGET_GRAPH_UNSCOPED`.
//!   * `validate` (working set) was `O(changed × store)` — a full store read per
//!     changed object (2.4 s at 14 changed, 31 s at 264). It now finds incoming
//!     linkers for the whole changed set in ONE embedded-ripgrep pass
//!     (`Store::find_links_to_any`), so it is flat in the changed-set size
//!     (~0.2 s at both 14 and 278 changed @10k). It is asserted below at a
//!     deliberately LARGE changed set — see `BUDGET_VALIDATE_WORKING` and
//!     `grow_changed_set` — so a revert to the per-object loop (tens of seconds
//!     at hundreds of changed objects) fails CI while the fixed single-pass cost
//!     stays comfortably inside the guard.
//!
//! OPT-OUT
//! -------
//! Set `DBMD_SKIP_PERF=1` to skip (e.g. a runner with no `rustc`, or a
//! deliberately resource-starved environment). It runs by default — being wired
//! and exercised is the entire point of the file.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;

// ─────────────────────────────────────────────────────────────────────────────
// Budgets — the plan's documented @10k numbers (Hard rules § "Performance
// targets are budgets" / Block 6). Widened by BUDGET_SLACK for CI-variance
// headroom so this guard catches order-of-magnitude regressions, not noise.
// ─────────────────────────────────────────────────────────────────────────────

/// Loop op: `fm query` resolves via the `index.jsonl` sidecar — must stay flat
/// in store size. Plan budget: 300 ms @10k.
const BUDGET_FM_QUERY: Duration = Duration::from_millis(300);
/// Loop op: `search --type` resolves candidates via the sidecar, then scans only
/// that set with embedded ripgrep. Plan budget: 300 ms @10k.
const BUDGET_SEARCH_TYPED: Duration = Duration::from_millis(300);

/// Loop op: `search` FREE-TEXT (unscoped) walks + scans every content file —
/// the documented whole-store scan class. Asserted since the 0.6.1 containment
/// fix: the 0.3.9 security gate silently tripled this op (2 realpath chains
/// per candidate, ~375 ms @10k vs the ~150 ms rg floor) and no CI budget
/// timed the free-text path, so nothing tripped. `StoreContainment` (root
/// canonicalized once, parent dirs memoized) restored the floor; this row
/// keeps the whole-store scan pinned to it.
const BUDGET_SEARCH_FREETEXT: Duration = Duration::from_millis(300);
/// Loop op: `log tail` reverse-reads from EOF, never a full parse. Plan budget:
/// 50 ms. (Held to a generous floor below because, unlike the other loop ops,
/// 50 ms is on the order of cold process-spawn on a shared CI box.)
const BUDGET_LOG_TAIL: Duration = Duration::from_millis(50);
/// Loop op: unscoped `graph backlinks` / `graph neighborhood --hops 1` — one
/// embedded-ripgrep pass for `[[<target>]]` over the store (the same scan class
/// `search` free-text rides), NOT a `read_to_string` of every content file once
/// per candidate. Plan budget: 200 ms @10k. This assertion exists because the
/// fix for the documented O(store) `graph` finding landed (PERF.md § "graph
/// backlinks (unscoped) and graph neighborhood are O(store)"); per this file's
/// "WHAT IT DELIBERATELY DOES NOT ASSERT" note, a fixed op gets its guard added
/// here so a revert to the per-candidate confirm-read fails CI.
const BUDGET_GRAPH_UNSCOPED: Duration = Duration::from_millis(200);
/// Loop op: `validate` (working set). The changed set + per-file checks are
/// O(changed); the incoming-linker discovery is a single embedded-ripgrep pass
/// over the store for the whole changed set at once (`Store::find_links_to_any`),
/// so the op is flat in the changed-set size. Plan budget: 1 s @10k (measured
/// "~10 changed files"). This assertion exists because the fix for the
/// documented `O(changed × store)` finding landed (PERF.md § "validate (working
/// set) is O(changed × store)"); it is exercised at a LARGE changed set (see
/// `grow_changed_set`) so a revert to the per-object full-store loop — tens of
/// seconds there — fails the `× BUDGET_SLACK` guard, which the flat ~0.2 s
/// fixed cost clears with wide headroom.
const BUDGET_VALIDATE_WORKING: Duration = Duration::from_millis(1000);
/// Sweep op: full-store validate. Plan budget: 5 s @10k.
const BUDGET_VALIDATE_ALL: Duration = Duration::from_secs(5);
/// Sweep op: full from-scratch index rebuild. Plan budget: 10 s @10k.
const BUDGET_INDEX_REBUILD: Duration = Duration::from_secs(10);
/// Sweep op: full-store stats. Plan budget: 5 s @10k.
const BUDGET_STATS: Duration = Duration::from_secs(5);

/// CI-variance headroom multiplier applied to every budget. A shared CI runner
/// is routinely 2–4× slower than the dev machine PERF.md was measured on, and a
/// cold process spawn dominates the tightest (50 ms) budget. We assert at
/// `budget × SLACK` so the guard fires on a *structural* regression (an
/// O(changed) op turning O(store), a sweep blowing into double-digit seconds)
/// rather than on scheduler noise. The tight, honest numbers are PERF.md's job.
const BUDGET_SLACK: u32 = 6;

/// Warmup iterations (discarded) to prime the page cache before timing — matches
/// the PERF.md method (cold-cache is the documented engine gap, not measured).
///
/// Two iteration profiles keep the whole test brisk: the ms-scale LOOP ops are
/// cheap, so we sample many for a stable median; the second-scale SWEEP ops are
/// expensive, so a smaller sample still pins the median well enough to catch an
/// order-of-magnitude regression. `(warmup, timed)`:
const LOOP_ITERS: (usize, usize) = (2, 5);
const SWEEP_ITERS: (usize, usize) = (1, 3);

// ─────────────────────────────────────────────────────────────────────────────
// Corpus build — compile + run tests/gen-scale.rs (the only place that does).
// ─────────────────────────────────────────────────────────────────────────────

/// `<repo-root>/tests/gen-scale.rs`. `CARGO_MANIFEST_DIR` is
/// `<repo-root>/crates/dbmd-cli`.
fn gen_scale_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("gen-scale.rs")
}

/// Compile `tests/gen-scale.rs` with `rustc -O` into `dir/gen-scale` and run its
/// `10k` tier into `dir/corpus-d-scale`, returning the generated store root.
/// The store is fresh, deterministic (seeded), and ~10,021 `.md` files. Panics
/// with a clear message if `rustc` is missing or the generator fails — those are
/// real "the perf guard cannot run" conditions, not skips (skip is the explicit
/// `DBMD_SKIP_PERF` env opt-out).
fn build_scale_corpus(dir: &Path) -> PathBuf {
    let src = gen_scale_src();
    assert!(
        src.is_file(),
        "tests/gen-scale.rs must exist at {} — it is the scale-corpus generator this guard runs",
        src.display()
    );

    // 1. Compile the std-only, zero-dep generator with optimizations.
    let bin = dir.join(if cfg!(windows) {
        "gen-scale.exe"
    } else {
        "gen-scale"
    });
    let compile = StdCommand::new("rustc")
        .arg("-O")
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "failed to spawn `rustc` to compile {}: {e}. \
                 Set DBMD_SKIP_PERF=1 to skip the perf guard on a runner without rustc.",
                src.display()
            )
        });
    assert!(
        compile.success(),
        "`rustc -O {}` failed (exit {:?}) — the scale generator must compile",
        src.display(),
        compile.code()
    );

    // 2. Run the 10k tier into a fresh store dir.
    let store = dir.join("corpus-d-scale");
    let run = StdCommand::new(&bin)
        .arg("10k")
        .arg(&store)
        .arg("--force") // idempotent if a previous attempt left a partial dir
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn the generated gen-scale binary: {e}"));
    assert!(
        run.success(),
        "gen-scale 10k failed (exit {:?}) — the scale corpus must generate cleanly",
        run.code()
    );

    // 3. Sanity-floor the corpus shape so a generator regression (e.g. it stops
    //    sharding, or emits a fraction of the files) is caught here rather than
    //    silently shrinking the thing the budgets are measured against.
    let md_count = count_md_files(&store);
    assert!(
        md_count >= 10_000,
        "scale corpus has {md_count} .md files, expected ~10,021 (>= 10,000) — \
         a 10k-tier corpus is the premise of every budget below"
    );
    assert!(
        store.join("DB.md").is_file(),
        "generated store is missing its DB.md marker — not a valid db.md store"
    );
    assert!(
        store.join("sources/emails").is_dir(),
        "generated store is missing the sources/emails overflow folder"
    );

    // 4. Normalize to the `index rebuild` FIXED POINT before timing anything.
    //
    //    `tests/PERF.md` describes the corpus as "a fixed point of `dbmd index
    //    rebuild`", and the read-only sweep budgets (`validate --all`, `stats`)
    //    are only meaningful against a *valid* store. The generator hand-emits
    //    each `index.jsonl`, and its forward-edge projection of typed link
    //    fields (e.g. a contact's `company: [[…]]`) currently differs from what
    //    `dbmd index rebuild` computes — so the raw generated store is
    //    index-stale (`dbmd validate --all` reports `INDEX_JSONL_STALE`). That
    //    generator↔toolkit projection drift is a SEPARATE, real defect tracked
    //    on its own; this perf guard deliberately does not depend on it. We run
    //    one rebuild to reach the canonical fixed point, then assert the store
    //    validates clean as the timing precondition. (This also means the first
    //    `index rebuild` timing below is an idempotent rebuild-of-a-rebuild,
    //    which is the honest steady-state cost.)
    let rebuild = dbmd_status(&[
        "index",
        "rebuild",
        "--dir",
        store.to_str().expect("store path is UTF-8"),
    ]);
    assert!(
        rebuild.success(),
        "`dbmd index rebuild` on the fresh scale corpus failed (exit {:?})",
        rebuild.code()
    );
    let validate = dbmd_status(&[
        "validate",
        "--all",
        store.to_str().expect("store path is UTF-8"),
    ]);
    assert!(
        validate.success(),
        "the scale corpus does not validate clean after `index rebuild` (exit {:?}) — \
         the perf guard requires a valid fixed-point store to time against",
        validate.code()
    );

    store
}

/// Run `dbmd <args>` to completion, discarding output, and return its exit
/// status. Used for the setup/normalization steps (rebuild, validate) that are
/// preconditions, not timed ops.
fn dbmd_status(args: &[&str]) -> std::process::ExitStatus {
    StdCommand::cargo_bin("dbmd")
        .expect("the `dbmd` binary builds for tests")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn dbmd")
}

/// Count `.md` files under `root`, recursively (the corpus-shape sanity floor).
fn count_md_files(root: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, n);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(root, &mut n);
    n
}

/// Pick a deterministic store-relative target for the `graph` ops: the
/// lexicographically-first `.md` under `records/companies/` (a flat entity
/// folder the generator always populates — see `gen-scale.rs::gen_companies`).
/// Companies are the highest-backlink entities in the corpus (contacts,
/// expenses, and meetings all link to a company), so an unscoped `backlinks`
/// against one is a representative, non-trivial whole-store scan. Returns the
/// bare store-relative path (the form `dbmd graph` accepts), e.g.
/// `records/companies/acme-co-29`.
fn first_company_target(store: &Path) -> String {
    let dir = store.join("records").join("companies");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("the 10k corpus has a records/companies folder")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|x| x.to_str()) == Some("md")
                && p.file_name().and_then(|n| n.to_str()) != Some("index.md")
        })
        .filter_map(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .collect();
    names.sort();
    let slug = names
        .first()
        .expect("records/companies has at least one company record");
    format!("records/companies/{slug}")
}

/// Recursive directory copy — gives the mutating `index rebuild` sweep its own
/// store so timing it never mutates the canonical generated/validated one.
fn copy_dir_all(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dest dir");
    for entry in std::fs::read_dir(src).expect("read source dir") {
        let entry = entry.expect("dir entry");
        let target = dst.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// Collect store-relative bare paths (no `.md`) of the first `limit` content
/// `.md` files under `sources/`/`records/`, in a deterministic
/// (sorted) order. Skips the `index.md` / `log.md` / `DB.md` meta files. Used to
/// name real, existing, already-clean targets for the grown changed set so the
/// working-set validate stays exit-0 (every named file is one the fixed-point
/// store already validates).
fn first_content_targets(store: &Path, limit: usize) -> Vec<String> {
    fn walk(dir: &Path, store: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, store, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(name, "index.md" | "log.md" | "DB.md") {
                    continue;
                }
                if let Ok(rel) = path.strip_prefix(store) {
                    let bare = rel.to_string_lossy().replace('\\', "/");
                    let bare = bare.strip_suffix(".md").unwrap_or(&bare).to_string();
                    out.push(bare);
                }
            }
        }
    }
    let mut out = Vec::new();
    for layer in ["records", "sources"] {
        walk(&store.join(layer), store, &mut out);
    }
    out.sort();
    out.truncate(limit);
    out
}

/// Append `count` `update` entries to the store's **active** `log.md`, each
/// naming a real existing content file, so the working-set changed set has
/// `count`-ish objects to scan for incoming linkers.
///
/// Why this is needed: the generated corpus's active `log.md` has only ~14
/// mutating entries, and the OLD `O(changed × store)` validate was ~2.4 s there
/// — UNDER the `1 s × 6` guard, so a small changed set would not catch the
/// regression. At hundreds of changed objects the old per-object full-store loop
/// is tens of seconds (PERF.md: 31 s at 264), far past the guard, while the
/// fixed single-pass cost stays ~0.2 s. So we grow the set, then time with
/// `--since 2020-01-01` (every mutating entry counts, independent of the
/// corpus's own `validate` entry).
///
/// Entries are timestamped inside the **anchor month** (`2026-05`, the active
/// `log.md` window — `changed_objects_since` reads only `log.md`, not the
/// rotated `log/` archives) and name files the fixed-point store already
/// validates clean, so the timed `validate` stays exit-0. The store is mutated
/// in place, so callers pass a private copy.
fn grow_changed_set(store: &Path, count: usize) {
    let targets = first_content_targets(store, count);
    assert!(
        targets.len() >= count,
        "the 10k corpus must have at least {count} content files to grow the \
         changed set; found {}",
        targets.len()
    );
    let log_path = store.join("log.md");
    let mut log = std::fs::read_to_string(&log_path).expect("read active log.md");
    if !log.ends_with('\n') {
        log.push('\n');
    }
    log.push('\n');
    for (i, bare) in targets.iter().enumerate() {
        // Day 01–28 (valid in every month), minute 00–59 — kept inside the
        // anchor month so the entry lands in the active log.md window.
        let day = (i % 28) + 1;
        let minute = i % 60;
        log.push_str(&format!(
            "## [2026-05-{day:02} 11:{minute:02}] update | [[{bare}]]\nTouched for the perf changed-set.\n\n"
        ));
    }
    std::fs::write(&log_path, log).expect("write grown log.md");
}

// ─────────────────────────────────────────────────────────────────────────────
// Timing harness.
// ─────────────────────────────────────────────────────────────────────────────

/// Build the args for one `dbmd` invocation. A closure so each timed iteration
/// gets a brand-new `Command` (a `Command` cannot be re-run).
type ArgsFn<'a> = dyn Fn() -> Vec<String> + 'a;

/// Run `dbmd <args>` once as a subprocess, asserting it exits 0, and return the
/// wall time of just the child process (spawn → exit). stdout/stderr are
/// discarded — we time the work, not the printing to a pipe the test ignores.
fn time_once(args: &[String]) -> Duration {
    let mut cmd = StdCommand::cargo_bin("dbmd").expect("the `dbmd` binary builds for tests");
    cmd.args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let start = Instant::now();
    let status = cmd.status().expect("spawn dbmd");
    let elapsed = start.elapsed();
    assert!(
        status.success(),
        "`dbmd {}` exited {:?} — a perf op must also be a CORRECT op (a crash/hang is the \
         worst regression)",
        args.join(" "),
        status.code()
    );
    elapsed
}

/// Warm up (discarded), then take the MEDIAN of the timed runs. Median, not
/// min/mean, so a single outlier neither helps nor hurts the verdict.
/// `iters` is `(warmup, timed)` — `LOOP_ITERS` or `SWEEP_ITERS`.
fn median_time(iters: (usize, usize), make_args: &ArgsFn) -> Duration {
    let (warmup, timed) = iters;
    for _ in 0..warmup {
        let _ = time_once(&make_args());
    }
    let mut samples: Vec<Duration> = (0..timed).map(|_| time_once(&make_args())).collect();
    samples.sort();
    samples[samples.len() / 2]
}

/// Assert one op's median time is within `budget × BUDGET_SLACK`. On failure the
/// message names the op, the median, and both the raw plan budget and the
/// slack-widened guard so a reader sees immediately whether it is a real
/// regression or just headroom exhausted.
fn assert_within_budget(label: &str, median: Duration, budget: Duration) {
    let guard = budget * BUDGET_SLACK;
    assert!(
        median <= guard,
        "PERF REGRESSION: `{label}` median {median:?} exceeds the guard {guard:?} \
         (plan budget {budget:?} × {BUDGET_SLACK} CI headroom) at the 10k tier. \
         This op is supposed to be {}. See tests/PERF.md for the measured baseline.",
        if budget >= Duration::from_secs(1) {
            "an O(store) sweep that stays sub-budget"
        } else {
            "an O(changed) loop read that stays flat in store size"
        }
    );
}

/// `true` when the perf guard should be skipped (explicit env opt-out only).
fn skip_perf() -> bool {
    std::env::var_os("DBMD_SKIP_PERF").is_some()
}

// ─────────────────────────────────────────────────────────────────────────────
// The test — one entry point so the (expensive) corpus build happens ONCE.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn budgets_hold_on_the_10k_scale_corpus() {
    if skip_perf() {
        eprintln!("[perf] DBMD_SKIP_PERF set — skipping the 10k budget guard");
        return;
    }

    // Build the corpus once into a tempdir that lives for the whole test.
    let tmp = tempfile::TempDir::new().expect("create tempdir for the scale corpus");
    let store = build_scale_corpus(tmp.path());
    let store_str = store.to_str().expect("store path is UTF-8").to_string();
    eprintln!(
        "[perf] generated 10k scale corpus at {} ({} .md files)",
        store.display(),
        count_md_files(&store)
    );

    // ── Loop ops — O(changed), must stay flat in store size ──────────────────

    // `query --where` — sidecar read, scoped to one type-folder's index.jsonl
    // (the former `fm query` dedup primitive).
    let median = median_time(LOOP_ITERS, &|| {
        vec![
            "query".into(),
            "--where".into(),
            "status=active".into(),
            "--type".into(),
            "company".into(),
            "--dir".into(),
            store_str.clone(),
        ]
    });
    eprintln!(
        "[perf] query --where status=active --type company: median {median:?} (budget {BUDGET_FM_QUERY:?})"
    );
    assert_within_budget(
        "query --where status=active --type company",
        median,
        BUDGET_FM_QUERY,
    );

    // `search --type` — sidecar candidate resolution + embedded-rg over the set.
    let median = median_time(LOOP_ITERS, &|| {
        vec![
            "search".into(),
            "Kickoff".into(),
            "--type".into(),
            "email".into(),
            "--dir".into(),
            store_str.clone(),
        ]
    });
    eprintln!(
        "[perf] search Kickoff --type email: median {median:?} (budget {BUDGET_SEARCH_TYPED:?})"
    );
    assert_within_budget("search Kickoff --type email", median, BUDGET_SEARCH_TYPED);

    // `search` free-text — the unscoped whole-store scan (walk + embedded-rg
    // over all ~10k content files; zero-hit term so match volume is nil and
    // the number is pure scan + containment cost).
    let median = median_time(LOOP_ITERS, &|| {
        vec![
            "search".into(),
            "zzz-perf-no-hit".into(),
            "--dir".into(),
            store_str.clone(),
        ]
    });
    eprintln!(
        "[perf] search zzz-perf-no-hit (free-text): median {median:?} (budget {BUDGET_SEARCH_FREETEXT:?})"
    );
    assert_within_budget(
        "search zzz-perf-no-hit (free-text)",
        median,
        BUDGET_SEARCH_FREETEXT,
    );

    // `log tail 20` — reverse-read from EOF, no full parse.
    let median = median_time(LOOP_ITERS, &|| {
        vec![
            "log".into(),
            "tail".into(),
            "20".into(),
            "--dir".into(),
            store_str.clone(),
        ]
    });
    eprintln!("[perf] log tail 20: median {median:?} (budget {BUDGET_LOG_TAIL:?})");
    assert_within_budget("log tail 20", median, BUDGET_LOG_TAIL);

    // `graph backlinks` (UNSCOPED) — one embedded-ripgrep pass over the store,
    // NOT a per-candidate `read_to_string` of every content file (the old
    // O(store) confirm-read this guard now pins against; PERF.md § graph finding).
    let target = first_company_target(&store);
    let median = median_time(LOOP_ITERS, &|| {
        vec![
            "graph".into(),
            "backlinks".into(),
            target.clone(),
            "--dir".into(),
            store_str.clone(),
        ]
    });
    eprintln!(
        "[perf] graph backlinks (unscoped): median {median:?} (budget {BUDGET_GRAPH_UNSCOPED:?})"
    );
    assert_within_budget("graph backlinks (unscoped)", median, BUDGET_GRAPH_UNSCOPED);

    // `graph neighborhood --hops 1` — at one hop this resolves the seed's
    // incoming edges with the SAME single ripgrep pass as backlinks (the
    // discovered nodes only seed the next, unrun level), so it shares the
    // backlinks budget. Inherits the fix; guards against the per-node O(store)
    // regression returning.
    let median = median_time(LOOP_ITERS, &|| {
        vec![
            "graph".into(),
            "neighborhood".into(),
            target.clone(),
            "--hops".into(),
            "1".into(),
            "--dir".into(),
            store_str.clone(),
        ]
    });
    eprintln!(
        "[perf] graph neighborhood --hops 1: median {median:?} (budget {BUDGET_GRAPH_UNSCOPED:?})"
    );
    assert_within_budget("graph neighborhood --hops 1", median, BUDGET_GRAPH_UNSCOPED);

    // `validate` (working set) — the O(changed × store) → single-pass fix.
    // Grow the changed set to a LARGE size in a private copy so the OLD
    // per-object full-store loop (tens of seconds at hundreds of changed
    // objects — PERF.md: 31 s at 264) would blow past the `1 s × 6` guard, while
    // the fixed single ripgrep pass stays flat (~0.2 s). Timed with
    // `--since 2020-01-01` so every appended `update` counts toward the changed
    // set regardless of the corpus's own `validate` log entry (PERF.md method).
    const GROWN_CHANGED: usize = 250;
    let validate_store = tmp.path().join("validate-working-target");
    copy_dir_all(&store, &validate_store);
    grow_changed_set(&validate_store, GROWN_CHANGED);
    let validate_str = validate_store
        .to_str()
        .expect("validate-working path is UTF-8")
        .to_string();
    let median = median_time(LOOP_ITERS, &|| {
        vec![
            "validate".into(),
            "--since".into(),
            "2020-01-01".into(),
            validate_str.clone(),
        ]
    });
    eprintln!(
        "[perf] validate (working set, ~{GROWN_CHANGED} changed): median {median:?} \
         (budget {BUDGET_VALIDATE_WORKING:?})"
    );
    assert_within_budget(
        "validate (working set, grown changed set)",
        median,
        BUDGET_VALIDATE_WORKING,
    );

    // ── Sweep ops — O(store), off the loop, must stay sub-budget ─────────────

    // `validate --all` — full-store sweep (read-only on the fixed-point store).
    let median = median_time(SWEEP_ITERS, &|| {
        vec!["validate".into(), "--all".into(), store_str.clone()]
    });
    eprintln!("[perf] validate --all: median {median:?} (budget {BUDGET_VALIDATE_ALL:?})");
    assert_within_budget("validate --all", median, BUDGET_VALIDATE_ALL);

    // `stats` — full-store sweep (read-only).
    let median = median_time(SWEEP_ITERS, &|| vec!["stats".into(), store_str.clone()]);
    eprintln!("[perf] stats: median {median:?} (budget {BUDGET_STATS:?})");
    assert_within_budget("stats", median, BUDGET_STATS);

    // `index rebuild` — MUTATES the index hierarchy, so time it against a private
    // copy rather than the canonical generated store. One copy, rebuilt
    // repeatedly: `index rebuild` is a deterministic from-scratch repair, so a
    // rebuild of an already-rebuilt store is the same O(store) work with no
    // cross-iteration write collision (matches PERF.md's single-`cp -R` method).
    let rebuild_store = tmp.path().join("rebuild-target");
    copy_dir_all(&store, &rebuild_store);
    let rebuild_str = rebuild_store
        .to_str()
        .expect("rebuild path is UTF-8")
        .to_string();
    let median = median_time(SWEEP_ITERS, &|| {
        vec![
            "index".into(),
            "rebuild".into(),
            "--dir".into(),
            rebuild_str.clone(),
        ]
    });
    eprintln!("[perf] index rebuild: median {median:?} (budget {BUDGET_INDEX_REBUILD:?})");
    assert_within_budget("index rebuild", median, BUDGET_INDEX_REBUILD);
}
