#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""perf.py — repeated-timing driver behind tests/PERF.md (the 10k tier).

Produces the published numbers: min/p50/mean/max wall-time per `dbmd` op
against the generated `corpus-d-scale` 10k corpus. This is the committed
form of the previously-throwaway driver, so "reproduce" is one command.

Method (matches PERF.md § Method):
  * Repeated timing around `subprocess.run` only — the `dbmd` process
    spawn IS included (it is part of every real agent call); the driver's
    own overhead is excluded.
  * Warm cache: each op runs discarded warmup passes, then timed passes.
  * Read-only ops and sweeps run against the canonical corpus, which must
    be at the index-rebuild fixed point (`dbmd index rebuild` once after
    `gen-scale`; `validate --all` then reports 0 errors).
  * Mutating ops (`fm set`, `write`) and the grown working-set validates
    run against a fresh copy in a temp dir, so the canonical corpus stays
    pristine and iterations don't collide.
  * The working-set tiers time `validate --since 2020-01-01` (the anchor
    bypass) after growing the active `log.md` changed set with real
    `dbmd log update` appends — the lifecycle's own op, no hand-written
    log lines.

Usage:
  rustc -O tests/gen-scale.rs -o /tmp/gen-scale
  /tmp/gen-scale 10k tests/corpora/corpus-d-scale
  (cd tests/corpora/corpus-d-scale && ../../../target/release/dbmd index rebuild)
  python3 tests/perf.py --bin target/release/dbmd \
      --corpus tests/corpora/corpus-d-scale
"""

import argparse
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

def run_once(bin_path, args, cwd):
    t0 = time.perf_counter()
    r = subprocess.run([bin_path, *args], cwd=cwd,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    dt = (time.perf_counter() - t0) * 1000.0
    if r.returncode not in (0, 6):  # 6 = validation-failed is a legal outcome
        sys.exit(f"FATAL: dbmd {' '.join(args)} exited {r.returncode}")
    return dt

def measure(bin_path, args, cwd, warmup, iters, mutate_arg=None):
    for i in range(warmup):
        a = mutate_arg(i) if mutate_arg else args
        run_once(bin_path, a, cwd)
    times = []
    for i in range(iters):
        a = mutate_arg(warmup + i) if mutate_arg else args
        times.append(run_once(bin_path, a, cwd))
    return {
        "min": min(times),
        "p50": statistics.median(times),
        "mean": statistics.fmean(times),
        "max": max(times),
    }

def fmt_row(name, m, budget_ms):
    verdict = "PASS" if m["p50"] <= budget_ms else "OVER"
    return (f"| `{name}` | **{m['p50']:.1f} ms** | {m['mean']:.1f} | "
            f"{m['max']:.1f} | {budget_ms:,} ms | {verdict} |")

def first_entity(corpus, folder):
    files = sorted((corpus / folder).glob("*.md"))
    files = [f for f in files if f.name != "index.md"]
    if not files:
        sys.exit(f"FATAL: no entity files under {folder}")
    return str(files[0].relative_to(corpus)).removesuffix(".md")

def grow_working_set(bin_path, cwd, targets):
    for t in targets:
        r = subprocess.run([bin_path, "log", "update", t, "-m", "perf grow"],
                           cwd=cwd, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
        if r.returncode != 0:
            sys.exit(f"FATAL: log update {t} exited {r.returncode}")

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--iters-fast", type=int, default=12)
    ap.add_argument("--iters-slow", type=int, default=6)
    args = ap.parse_args()

    bin_path = str(Path(args.bin).resolve())
    corpus = Path(args.corpus).resolve()
    fast, slow = args.iters_fast, args.iters_slow

    company = first_entity(corpus, "records/companies")
    contact = first_entity(corpus, "records/contacts")
    rows = []

    print(f"# corpus: {corpus}", file=sys.stderr)
    print(f"# company={company} contact={contact}", file=sys.stderr)

    # Startup baseline.
    m = measure(bin_path, ["--version"], corpus, 2, fast)
    print(f"# startup (dbmd --version): p50 {m['p50']:.1f} ms", file=sys.stderr)

    # ── Loop ops, read-only, canonical corpus ──
    loop = [
        ("query --where status=active --type company",
         ["query", "--where", "status=active", "--type", "company"], 300),
        ("search Kickoff --type email",
         ["search", "Kickoff", "--type", "email"], 300),
        ("search Kickoff (free-text)", ["search", "Kickoff"], 300),
        ("log tail 20", ["log", "tail", "20"], 50),
        (f"graph backlinks <company> (unscoped)",
         ["graph", "backlinks", company], 200),
        (f"graph backlinks <company> --type contact",
         ["graph", "backlinks", company, "--type", "contact"], 200),
        (f"graph neighborhood <company> --hops 1",
         ["graph", "neighborhood", company, "--hops", "1"], 200),
        ("validate (working set, empty)", ["validate"], 1000),
    ]
    for name, a, budget in loop:
        rows.append(fmt_row(name, measure(bin_path, a, corpus, 2, fast), budget))

    # ── Sweep ops, canonical corpus (rebuild at fixed point rewrites
    #    byte-identical output, so the corpus stays canonical) ──
    sweeps = [
        ("validate --all", ["validate", "--all"], 5000),
        ("index rebuild (full)", ["index", "rebuild"], 10000),
        ("stats", ["stats"], 5000),
    ]
    for name, a, budget in sweeps:
        rows.append(fmt_row(name, measure(bin_path, a, corpus, 1, slow), budget))

    # ── Mutating ops on a throwaway copy ──
    with tempfile.TemporaryDirectory(prefix="dbmd-perf-") as td:
        copy = Path(td) / "corpus"
        shutil.copytree(corpus, copy)

        rows.append(fmt_row(
            "fm set status=<alt> <contact>",
            measure(bin_path, None, copy, 2, fast,
                    mutate_arg=lambda i: ["fm", "set", f"{contact}.md",
                                          f"status={'active' if i % 2 else 'paused'}"]),
            100))
        rows.append(fmt_row(
            "write <new email source>",
            measure(bin_path, None, copy, 2, fast,
                    mutate_arg=lambda i: ["write", f"perf-{i}.md", "--type",
                                          "email", "--summary", f"perf probe {i}"]),
            100))

        # ── Working-set tiers: grow the active log with real `log update`
        #    appends, time `validate --since` (anchor bypass, repeatable) ──
        pool = sorted((copy / "records" / "contacts").glob("*.md"))
        pool = [str(p.relative_to(copy)).removesuffix(".md")
                for p in pool if p.name != "index.md"]
        vs = ["validate", "--since", "2020-01-01"]
        grown = 0
        for tier in (14, 64, 264):
            need = tier - grown
            grow_working_set(bin_path, copy, pool[grown:grown + need])
            grown = tier
            rows.append(fmt_row(f"validate --since (~{tier} changed)",
                                measure(bin_path, vs, copy, 1, slow), 1000))

    print("\n| op | p50 | mean | max | budget | verdict |")
    print("|---|---:|---:|---:|---:|:---|")
    for r in rows:
        print(r)

if __name__ == "__main__":
    main()
