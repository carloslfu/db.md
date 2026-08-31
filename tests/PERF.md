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
| Machine | Apple M5 Pro, 18 cores, 48 GB RAM |
| OS | macOS 26.6.2 |
| Toolchain | rustc 1.96.0 |
| Binary | `target/release/dbmd` **0.13.4** (`--release`: LTO, codegen-units=1, strip) |
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

## Results — loop ops (budgets @10k, measured 2026-08-31 on 0.13.4)

The `0.6.1` column is the previous run of this table (2026-07-02, Apple M3 Pro
/ 12 cores). It is a different machine, so read it for shape, not for a precise
delta — except where a row moved by far more than a machine can explain, which
is the point of keeping it.

| op | p50 | mean | max | budget | verdict | 0.6.1 published |
|---|---:|---:|---:|---:|:---|---:|
| `query --where status=active --type company` ¹ | **45.4 ms** | 46.9 | 54.6 | 300 ms | PASS | 27.9 |
| `search Kickoff --type email` | **81.4 ms** | 81.6 | 85.9 | 300 ms | PASS | 73.5 |
| `search Kickoff` (free-text, whole store) | **170.6 ms** | 170.7 | 176.9 | 300 ms | PASS | 179.4 |
| `log tail 20` | **4.3 ms** | 4.3 | 4.5 | 50 ms | PASS | 3.4 |
| `graph backlinks <company>` (unscoped) | **187.5 ms** | 185.9 | 194.1 | 200 ms | PASS | 180.2 |
| `graph backlinks <company> --type contact` | **53.2 ms** | 53.8 | 60.6 | 200 ms | PASS | 49.2 |
| `graph neighborhood <company> --hops 1` | **175.0 ms** | 175.9 | 184.7 | 200 ms | PASS | 181.1 |
| `fm set status=<alt> <contact>` | **31.3 ms** | 31.2 | 31.9 | 100 ms | PASS ² | 60.1 |
| `write <new email source>` | **39.2 ms** | 39.0 | 39.7 | 100 ms | PASS ² | 65.5 |
| `validate` (working set, **empty** → full sweep) ³ | **961.8 ms** | 956.8 | 995.9 | 1,000 ms | PASS | 907.7 |
| `validate --since` (~14 changed) | **221.8 ms** | 222.4 | 226.4 | 1,000 ms | PASS | 219.8 |
| `validate --since` (~64 changed) | **327.7 ms** | 328.7 | 334.7 | 1,000 ms | PASS | 320.0 |
| `validate --since` (~264 changed) | **610.1 ms** | 609.5 | 614.1 | 1,000 ms | PASS | 711.7 |

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

| op | p50 | mean | max | budget | verdict | 0.6.1 published |
|---|---:|---:|---:|---:|:---|---:|
| `validate --all` ⁴ | **1,602.7 ms** | 1,619.6 | 1,719.8 | 5,000 ms | PASS | 1,454.9 |
| `index rebuild` (full) | **378.0 ms** | 380.5 | 395.3 | 10,000 ms | PASS | 478.2 |
| `stats` | **342.9 ms** | 343.0 | 347.4 | 5,000 ms | PASS | 295.9 |

⁴ `validate --all` grew ~60% across 0.4–0.6 as it gained checks (loose-file
layer sidecars, `FM_BAD_ID`/`DUP_ID` on the id contract, jsonl desync
classes) — honestly O(store) with a rising constant.

It then spent 0.8.3 through 0.13.3 at **~9,200 ms** on this corpus, a 4×
regression nothing caught, and this row is the reason it was eventually
found: the number here is the only record of what the op used to cost.
`ac12a06` (0.8.3) replaced the wiki-link exact-casing check with a walk that
re-read whole directories per link — O(links × directory size), and this
corpus carries 25,696 links over type folders holding thousands of entries.
0.13.4 scoped a directory-listing cache to the sweep and brought it back to
the number above. Full account: [§ the 0.8.3 regression](#the-083-regression-a-security-fix-that-cost-4x).

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

## The 0.8.3 regression — a security fix that cost 4x

The same shape as the 0.6.0 interlude above, two years of releases apart, and
it survived far longer because the guard that should have caught it was
looking at the wrong number.

`ac12a06` (0.8.3, 2026-07-30, "security: harden dbmd trust and filesystem
boundaries") replaced the wiki-link exact-casing check. That check earns its
keep: on case-insensitive macOS, `[[Records/Foo]]` opens a file that Linux
would call broken, so `validate` confirms the on-disk spelling character for
character and reports the same broken links everywhere.

- **Before**: `canonicalize()` the target and the root, `strip_prefix`,
  compare. Two syscall chains per link. O(path depth).
- **After**: for each path component, a full `readdir` of the directory, then
  `directory_contains_exact_regular` scanning the same directory again, then
  on descent a third scan checking for a nested `DB.md`. Always restarting
  from the store root. **O(directory size) per component, per link.**

`resolve_wiki_target` calls it for every wiki-link and up to twice each
(literal path, then `.md`-appended). This corpus carries 25,696 links over
only 3,967 distinct targets, in type folders holding thousands of entries.
`validate --all` went **2,435 ms → 9,820 ms in that one commit** and stayed
there through 0.13.3. Verified by bisect on release builds, with both sides
reporting identical results (36 issues, 0 errors).

**Why nothing tripped.** `perf_budget.rs` asserted `5 s × BUDGET_SLACK 6` =
a 30 s guard, so a 4x regression fit inside it with room to spare. Worse, the
slack was being spent unevenly: measured against the plan budgets, headroom
ranged from 79x on `log tail` to 2.1x on `validate`, so the guard was
simultaneously too loose to catch a real regression and tight enough on the
validate rows to flake under load. It did flake, and chasing that flake is
how this was found.

**Fixed in 0.13.4** — a directory-listing cache scoped to one sweep
(`fsx::DirListingScope`, opened by `validate_all` and `validate_working_set`).
The fd-based descent, `O_NOFOLLOW`, and the nested-`DB.md` boundary check are
untouched; only the repeated reads are removed, and the `fstatat` that decides
regular-file-ness is deliberately still per call. The cache is off unless a
scope is open, because `Store` outlives a single operation in `dbmd watch` and
`dbmd api` — a listing cached across a process's life would report a file
that exists as missing for as long as the process ran. Three tests pin that
scoping, and each was verified to fail when the cache is allowed to leak.

`validate --all`: 9,236 → **1,602 ms**. Working set (~264 changed):
712 → **610 ms**.

`perf_budget.rs`'s budgets are now the measured debug medians rather than the
plan's aspirational numbers, so `BUDGET_SLACK` (3) means one thing on every
row. The plan budgets remain the contract and live here and in
`plans/db-md-rust-toolkit.md`; meeting them is a separate question from not
regressing, and that file only answers the second.

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
and `stats` ≤ 360 s). Run it after any change to the scan engine, the
containment gate, or the sidecar layout.

**First 0.6.1 run (2026-07-02, this machine):** corpus generated
(1,000,000 files, ~2 min) and rebuilt to the fixed point; `log tail` and
`query` passed their 1M guards; **`search --type email` measured 86 s
against its 12 s guard — FAIL.** The projection above the fix (~9–10 s) was
naive linear extrapolation from warm 10k numbers: at the 1m tier the emails
type is 400,000 files (~40 % of the store), and per-candidate cost rises
well past the 10k figure once the candidate set outruns page-cache warmth
(~215 µs/file measured vs ~25 µs warm @10k). This is the busy-type caveat
from § Scaling classes made concrete: typed search is honestly O(type), and
a type holding 40 % of a million-file store is a sweep, not a loop op.
Queued for the next toolkit release: re-model the 1M assertion around a
per-candidate budget (the "flat" label is wrong for a type that scales with
the store by construction), and/or the append+compact jsonl + parallel-scan
fix family. `validate --all` / `stats` did not get measured (the test
aborts at first failure); re-run after the re-model.
