# dbmd performance — corpus-d 10k tier

Measured numbers for the `dbmd` loop ops and sweep ops against the
`corpus-d-scale` 10k tier, compared to the budgets in
`plans/db-md-rust-toolkit.md` (Hard rules § "Performance targets are budgets"
and Block 6 § corpus-d).

**The 1M tier is nightly / opt-in and is NOT run in CI.** It is wired as an
opt-in `#[ignore]`-d test (`perf_1m_loop_ops_stay_flat_and_sweeps_stay_in_budget`
in `crates/dbmd-cli/tests/agent_eval.rs`) — see [§ 1M tier](#1m-tier--opt-in-ignore-not-run-in-ci)
below for how to run it. Its budgets are listed below as targets, with the
extrapolation math stated per scaling class.

## Environment

| | |
|---|---|
| Machine | Apple M3 Pro, 12 cores, 18 GB RAM |
| OS | macOS 26.5.1 |
| Toolchain | rustc 1.96.0 |
| Binary | `target/release/dbmd` **0.6.1** (`--release`: LTO, codegen-units=1, strip) |
| Corpus | `tests/corpora/corpus-d-scale` — 10,020 `.md` files (10,000 content: sources + records across date shards, two-layer v0.3+ layout) |
| Precondition | corpus regenerated (`tests/gen-scale.rs`, deterministic seed), then `dbmd index rebuild` once to the fixed point → `validate --all` = 0 errors |
| Startup floor | bare `dbmd --version` ≈ 3.2 ms (process spawn, included in every number) |

## Method

The driver is **committed**: [`tests/perf.py`](perf.py) (the 0.3.5-era numbers
came from a throwaway `/tmp` script; "reproduce" is now one command — see the
driver's header). Repeated timing around `subprocess.run` only; the `dbmd`
process spawn IS included (it is part of every real agent call). Warm cache
(discarded warmup passes, then 12 timed iterations for loop ops / 6 for
sweeps; `min`/`p50`/`mean`/`max` in ms). Read-only ops and sweeps run against
the canonical corpus at the rebuild fixed point; mutating ops and the grown
working-set validates run against a fresh copy in a temp dir. The working-set
tiers grow the active `log.md` with real `dbmd log update` appends and time
`validate --since 2020-01-01` (the anchor bypass, so the run is repeatable).

```
rustc -O tests/gen-scale.rs -o /tmp/gen-scale
/tmp/gen-scale 10k tests/corpora/corpus-d-scale
(cd tests/corpora/corpus-d-scale && ../../../target/release/dbmd index rebuild)
python3 tests/perf.py --bin target/release/dbmd --corpus tests/corpora/corpus-d-scale
```

## Results — loop ops (budgets @10k, measured 2026-07-02 on 0.6.1)

| op | p50 | mean | max | budget | verdict | 0.3.5 published |
|---|---:|---:|---:|---:|:---|---:|
| `query --where status=active --type company` ¹ | **27.9 ms** | 27.8 | 28.3 | 300 ms | PASS | 3.5 (`fm query`) |
| `search Kickoff --type email` | **73.5 ms** | 73.6 | 74.6 | 300 ms | PASS | 43 |
| `search Kickoff` (free-text, whole store) | **179.4 ms** | 180.0 | 184.2 | 300 ms | PASS | 151 |
| `log tail 20` | **3.4 ms** | 3.4 | 3.6 | 50 ms | PASS | 2.0 |
| `graph backlinks <company>` (unscoped) | **180.2 ms** | 180.4 | 182.9 | 200 ms | PASS | 210 |
| `graph backlinks <company> --type contact` | **49.2 ms** | 49.2 | 49.7 | 200 ms | PASS | 35 |
| `graph neighborhood <company> --hops 1` | **181.1 ms** | 181.3 | 182.4 | 200 ms | PASS | 218 |
| `fm set status=<alt> <contact>` | **60.1 ms** | 61.2 | 69.0 | 100 ms | **PASS** ² | 108 (OVER) |
| `write <new email source>` | **65.5 ms** | 65.6 | 66.8 | 100 ms | **PASS** ² | 123 (OVER) |
| `validate` (working set, **empty** → full sweep) ³ | **907.7 ms** | 907.2 | 910.8 | 1,000 ms | PASS | 1.9 (stale row) |
| `validate --since` (~14 changed) | **219.8 ms** | 219.6 | 221.0 | 1,000 ms | PASS | 180 |
| `validate --since` (~64 changed) | **320.0 ms** | 320.0 | 322.5 | 1,000 ms | PASS | 220 |
| `validate --since` (~264 changed) | **711.7 ms** | 710.7 | 818.9 | 1,000 ms | PASS | 370 |

¹ Not comparable 1:1 to the 0.3.5 row: `fm query` printed paths off one
sidecar; the 0.5.0 read-surface fold replaced it with `dbmd query`, which
assembles complete records. 28 ms for the richer op is comfortably flat.

² The 0.3.5 run's one standing finding — `fm set`/`write` marginally over
their 100 ms budget on the O(folder-jsonl) read+rewrite — **cleared**: the
write-path work landed across 0.4.x–0.6.0 roughly halved both. The budget is
met with the compacted-rewrite design intact.

³ By design, not a regression — see
[§ validate's empty-set sweep](#validates-empty-set-sweep-is-by-design).

## Results — sweep ops (budgets @10k, off-loop)

| op | p50 | mean | max | budget | verdict | 0.3.5 published |
|---|---:|---:|---:|---:|:---|---:|
| `validate --all` ⁴ | **1,454.9 ms** | 1,461.2 | 1,495.4 | 5,000 ms | PASS | 903 |
| `index rebuild` (full) | **478.2 ms** | 478.5 | 480.2 | 10,000 ms | PASS | 515 |
| `stats` | **295.9 ms** | 295.8 | 297.9 | 5,000 ms | PASS | 366 |

⁴ `validate --all` grew ~60% across 0.4–0.6 as it gained checks (loose-file
layer sidecars, `FM_BAD_ID`/`DUP_ID` on the id contract, jsonl desync
classes) — honestly O(store) with a rising constant, still 3.4× inside
budget.

## The 0.6.0 interlude — how a regression hid, and the fix (0.6.1)

Re-measuring on 0.6.0 (2026-07-02, first re-run since 0.3.5) found free-text
`search` at **402 ms — over its 300 ms budget** — and typed search at 143 ms.
Root cause (verified by decomposition, not guessed): the 0.3.9 security pass
(`d195550`) added the per-candidate containment gate
(`ensure_path_within_store`) to the scan loop, and its implementation paid
**two full `realpath(3)` chains per candidate — including re-canonicalizing
the same store root once per file**. The scan engine itself never regressed:
`rg -j1` over the same tree is ~150 ms, exactly the 0.3.5 measurement, and a
zero-hit term cost the same 400 ms (per-candidate syscalls, not match
volume). It went unnoticed because CI's `perf_budget.rs` timed only
`--type`-scoped search.

**Fixed in 0.6.1** — `StoreContainment` (dbmd-core): the root is
canonicalized once per search and parent-directory resolutions are memoized
(candidates cluster into a few dozen type/shard folders), so the common
candidate costs one `lstat(2)` + a prefix check. Symlink leaves, missing
files, and every other corner still take the original full peel-resolution —
the acceptance/rejection set is identical, pinned by an equivalence test
(`store_containment_matches_single_shot_gate`) and the existing
poisoned-sidecar regression tests. Free-text: 402 → **179 ms**; typed:
143 → **74 ms**. CI now asserts the free-text scan too
(`BUDGET_SEARCH_FREETEXT`), so this class of drift trips the gate next time.

## validate's empty-set sweep is by design

The 0.3.5 table's "1.9 ms empty working set" row was **stale on its own
publication day**: it was measured 2026-05-30, and `c9f0cc5` (2026-06-03,
inside the v0.3.5 tag) deliberately changed the empty case — *"an externally
edited or freshly copied store cannot pass vacuously"* — so `dbmd validate`
with an empty changed set and no `--since` falls back to a full content
sweep (`validate_content_sweep`: read + frontmatter parse + lints on every
content file, ~900 ms @10k). The loop contract is unaffected: an agent that
just wrote files has a non-empty working set and pays the O(changed) path
(the `--since` rows above — one union-regex incoming-link pass over the
store plus per-changed-file checks, flat-ish in the changed count). The
empty-set sweep fires exactly when there is nothing cheaper worth proving.
If a ms-class quiet-store `validate` is ever needed in the loop, the
documented options are: sweep only when no validate anchor exists at all, a
cheap freshness probe (walk + counts vs sidecars), or downgrading the
fallback to the presence-scan class (~185 ms) — a deliberate design change,
not a perf patch.

## Scaling classes — what extrapolates and how

Every op belongs to one of three classes; the 1M expectations follow from
the class, and the flat classes are what the architecture exists to provide:

- **Flat (O(1)-ish):** `log tail` (reverse-read from EOF), startup. Same
  cost at any store size.
- **Folder/changed-scoped (O(folder) / O(changed)):** `query`, typed
  `search`, scoped `graph`, `fm set`, `write`, `validate` with a working
  set. Cost follows the folder sidecar or the changed set, **not** the
  store. The one caveat: a single busy type's `index.jsonl` aggregates
  across date shards (the emails jsonl at the 1m tier is 400k lines), so
  O(folder-jsonl) ops widen with type volume — that is the documented
  trade for rebuild-identical, git-diffable sidecars.
- **Store scans (O(store)):** free-text `search`, unscoped `graph`,
  `validate --all`/empty-sweep, `index rebuild`, `stats`. Linear:
  ~180 ms @10k ⇒ **~18 s @1M** for a free-text scan; `validate --all`
  ~1.5 s @10k ⇒ **~2.5 min @1M**. These are the off-loop / sweep paths by
  design — at 1M you scope your reads (that is what the sidecars are for)
  and schedule your sweeps.

## 1M tier — opt-in (`#[ignore]`), NOT run in CI

Per instructions the 1M tier is nightly/opt-in and is **not** executed by the
default test run. It is wired as an opt-in, `#[ignore]`-d test so it can be
run on demand without ever burdening CI:

```
# Generates a ~1M-file corpus-d-scale (minutes + several GB of disk), reaches
# the index-rebuild fixed point, then times loop + sweep ops against it.
cargo test -p dbmd-cli --test agent_eval -- --ignored perf_1m
```

The test asserts the plan's 1M budgets (`log tail` ≤ 300 ms, `query` and
typed `search` ≤ 12 s against the 400k-line emails sidecar, `validate --all`
and `stats` ≤ 360 s). Pre-0.6.1 the containment-gate cost would have pushed
typed search past its 12 s guard at 400k candidates; post-fix the projection
is ~9–10 s. Run it after any change to the scan engine, the containment
gate, or the sidecar layout.
