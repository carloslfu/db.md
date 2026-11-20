# db.md agent-memory benchmark (draft)

A draft protocol and harness for one question: does a db.md store, searched
lexically by an agent with no embeddings, work as memory for AI agents, and
does its recall improve as the operating model improves?

**Status.** Written on July 13, 2026, when Sevra planned to sell db.md hosting.
That hosting is retired. The protocol was never frozen and has never been run
at full scale. The only run is an 8-item smoke test of the plumbing, and smoke
numbers are never citable. The harness is outside the `dbmd` toolkit and its
build: it calls a model API with your own key.

| Path | What it is |
| --- | --- |
| [PROTOCOL.md](PROTOCOL.md) | The draft pre-registration: datasets, splits, lanes, judging and publication rules |
| [KILL-GATE.md](KILL-GATE.md) | An adversarial review of the draft and the amendments it forced |
| [harness/](harness/) | TypeScript harness: the db.md and closed-book adapters, judge, run loop and dataset fetcher |
| [runs/smoke-2026-07-13/](runs/smoke-2026-07-13/) | The one smoke run: config, summary and transcripts |

When this folder moved from Sevra's private repository on November 20, 2026,
the smoke run's absolute local dataset path was replaced with its
repository-relative path. Nothing else in the run files changed.

## Run it

Needs Node 22+ and the `dbmd` binary (`~/.dbmd/bin/dbmd`, or set `DBMD_BIN`).

```bash
cd bench
npm install
npm run typecheck
npm run fetch-datasets   # LongMemEval oracle file into bench/data/ (gitignored)
npm run smoke            # 8 oracle items on Haiku; needs ANTHROPIC_API_KEY
```

The key is read from the environment or from `bench/.env.local`, which is
gitignored. Every run stops at a hard spend ceiling: $0.80 by default, set with
`--max-spend-usd=N` or `BENCH_MAX_SPEND_USD`. See the header of
[harness/run.ts](harness/run.ts) for the full options.
