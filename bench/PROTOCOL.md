# Agent-memory benchmark protocol — pre-registration DRAFT

> **November 20, 2026:** moved here from Sevra's private repository. Sevra no longer offers
> db.md hosting. The ownership and conflict-of-interest notes below describe the July 2026
> draft as written. See [README.md](README.md).

**Status:** DRAFT — NOT FROZEN. Nothing in this file has been run at full scale and no recall
claims may be made from it. This draft exists to be attacked ([KILL-GATE.md](KILL-GATE.md)) and
amended. It becomes a pre-registration only when (a) every kill-gate amendment is folded in,
(b) the founder signs the compute budget, and (c) the freeze commit hash is recorded below.
**Freeze commit:** _none yet_.

**Owner:** Sevra (VibeCraft Inc). **Drafted:** 2026-07-13.
**Conflict of interest, stated up front:** Sevra sells hosting for db.md stores, and db.md is one
of the systems under test. This protocol is written knowing that vendor-run memory benchmarks
have a public credibility graveyard (see Appendix B, the LoCoMo dispute). Every design decision
below that exists to manage that conflict is marked **[COI]**.

---

## 1. What is being tested

**The claim under test:** a plain-text store (db.md: markdown files + frontmatter + lexical
search, **no vectors, no embeddings**) operated agentically is a competitive memory substrate
for AI agents, and — the signature claim — **its recall rides the model curve**: as the
operating model improves, plain-text memory improves with it, without re-engineering, because
the intelligence lives in the agent rather than in a retrieval pipeline.

**What is explicitly NOT tested** (scope fence, cite it when someone stretches a result):

- Write-time curation quality (what an agent chooses to remember). Both datasets hand the
  system a fixed history; ingestion is verbatim.
- Agentic memory-in-the-loop over long-horizon tasks (Letta's critique of retrieval-style
  benchmarks is correct as far as it goes — see Appendix B). LongMemEval-v2 (May 2026,
  agent trajectories) is the future lane for that, not this protocol.
- Multimodal memory.
- Latency at serving scale (we report wall-clock, but no p99 claims).

## 2. Task sets and splits (exact)

### 2.1 LongMemEval (headline dataset)

- **Artifact:** `longmemeval_s_cleaned.json` from HF `xiaowu0162/longmemeval-cleaned`
  (277 MB; the 2025-09 "cleaned" release, which removed interfering history sessions).
  MIT license. SHA-256 recorded in `bench/data/checksums.json` at fetch time and pinned
  in the freeze commit.
- **Population:** all **500** questions. Six types (`single-session-user`,
  `single-session-assistant`, `single-session-preference`, `temporal-reasoning`,
  `knowledge-update`, `multi-session`) plus the `*_abs` abstention variants.
- **Splits:** there is no held-out test split (the dataset is fully public). Therefore:
  - **Engineering slice:** `longmemeval_oracle.json` (evidence-only haystacks, 15.4 MB) plus
    at most **30** S-questions selected by seeded RNG (`seed=20260713`), IDs enumerated in the
    freeze commit. Prompts, tool budgets, and judge wording may be tuned ONLY against these.
  - **Reported set:** all 500 questions of `longmemeval_s_cleaned.json`, run once per cell
    after freeze (3× on primary cells, §5). The 30 engineering IDs are flagged in every
    results table; headline numbers are reported with and without them.
- LongMemEval-M (2.74 GB, ~500-session haystacks) is a stretch lane, budget-gated; if run,
  same rules.

### 2.2 MuSiQue (rigor anchor — contamination-adjusted only)

- **Artifact:** MuSiQue-Ans **dev** split (~2,417 items; the test split's answers are withheld
  for the official leaderboard, so dev is the only local option — acknowledged as the weaker
  position and mitigated in §6). CC BY 4.0. Official distribution is the StonyBrookNLP Google
  Drive artifact; the fetch script also supports the HF mirror `dgslibisey/MuSiQue` for
  convenience — **the freeze pins the official artifact's SHA-256**, and any mirror fetch is
  verified against the pinned per-item content hash before use.
- **Population:** **500** answerable items sampled from dev by seeded RNG (`seed=20260713`),
  stratified by hop count (2/3/4-hop in the dev split's natural proportions), plus **100**
  unanswerable twins from MuSiQue-Full dev (abstention lane). Item IDs enumerated at freeze.
- **Framing:** MuSiQue is NOT a memory benchmark; it is a multi-hop composition benchmark that
  we use to test **cross-record joins** — the database half of the db.md claim. Every system
  ingests the same 20 paragraphs per item as records and must answer from the store.

### 2.3 Ingestion contract (identical for every system)

- LongMemEval: each haystack session becomes one unit (role-tagged turns, session date
  attached). MuSiQue: each paragraph becomes one unit (title + text).
- Systems ingest units verbatim through their own documented ingestion API. No system may see
  the question at ingestion time. Ingestion-time LLM calls are allowed (that is mem0/Zep's
  design) but are **metered and reported** (§4).

## 3. Systems and per-system config rules

Lanes (Lane B, cross-system):

| System | Class | Config source |
|---|---|---|
| **db.md** (`dbmd` CLI + agent loop) | agent-filesystem, no embeddings | this repo, `bench/harness/adapters/dbmd.ts` |
| **mem0** (OSS, Apache-2.0, `mem0ai`) | extraction pipeline + vector/graph | mem0's own quickstart + their `memory-benchmarks` repo defaults |
| **Letta** (Apache-2.0, ex-MemGPT) | stateful agent w/ filesystem memory | Letta's documented filesystem-memory config (their own LoCoMo setup) |
| **Zep / Graphiti** (Apache-2.0 core) | temporal knowledge graph | Graphiti self-hosted quickstart; Zep Cloud only if Zep supplies the config |
| **full-context** baseline | no memory system; entire history in context | this repo |
| **closed-book** baseline | no history at all (contamination control) | this repo |

**[COI] Config rules — the mem0-dispute repellent:**

1. Every system runs **its own vendor-published best-effort config**, version-pinned, committed
   to this repo verbatim, with a link to the vendor doc it came from.
2. **Right of reply:** each named vendor is sent their config + raw transcripts ≥7 calendar
   days (external-party-gated) before publication. A vendor-supplied replacement config is run
   as-is, credited, and replaces ours. Correspondence (or non-response) is published.
3. We never author "our best guess at their tuning." If a system cannot run a lane without
   undocumented choices, the lane is reported as **not-run** with the blocker, never a zero
   (this is the exact failure Letta documented in mem0's MemGPT numbers — Appendix B).
4. Systems that mandate a bundled/managed model for extraction keep it (reported); the
   **answering** model is the same per cell for all systems.

## 4. Metrics

- **Primary (LongMemEval):** judge accuracy overall + per question type. Abstention questions
  reported both inside the official aggregate and as a separate line.
- **Primary (MuSiQue):** answer F1 (SQuAD normalization, `answer` + `answer_aliases`);
  EM secondary; unanswerable-abstention accuracy separate. **No LLM judge on MuSiQue.**
- **Co-primary (both): total cost of memory.** Per 100 questions: ingest LLM tokens, query LLM
  tokens, USD at list price, wall-clock. A memory system's claim is precisely that it beats
  full-context on cost without giving up accuracy — hiding either half of that tradeoff is the
  standard vendor trick; we report both halves for every cell, including ingestion-time spend
  (db.md ingestion is mechanical: 0 LLM tokens — that shows up as a reported advantage, not
  buried; extraction pipelines pay at write time — that is reported, not mocked).
- **Contamination-adjusted (both):** headline repeated on the **non-parametric subset** —
  items the same model answers correctly closed-book are flagged per model and excluded in the
  adjusted view (§6). Raw and adjusted are always published together.
- **Variance:** bootstrap 95% CIs over items on every cell; 3 independent runs on primary
  cells with mean ± range. No cross-system delta is claimed when CIs overlap.

## 5. The model curve (Lane A — the signature)

Same 500 + 500 + abstention items, db.md lane only, across model tiers:

- **Tiers (pinned at freeze; snapshot IDs, never aliases):** small / mid / frontier from
  **at least two providers** — e.g. `claude-haiku-4-5-20251001`, a Sonnet-tier snapshot, an
  Opus-tier snapshot, plus the equivalent small + frontier of one non-Anthropic provider
  (db.md is bring-your-own-agent; an Anthropic-only curve would test the wrong claim).
- Identical agent loop, prompts, tool budget (max 12 tool calls), and params per cell;
  the only variable is the model.
- **Pre-registered predictions (falsifiable):** (P1) db.md accuracy is monotonically
  non-decreasing in model tier on both datasets; (P2) the gap to the same-model full-context
  ceiling narrows as tier increases; (P3) at the frontier tier, db.md is within the
  full-context CI on LongMemEval-S at a small fraction of its query cost. If P1–P3 fail,
  that is published as the finding.
- Baselines (closed-book, full-context) run at every tier — the curve is meaningless without
  its floor and ceiling.

## 6. Contamination controls

1. **Closed-book lane per model** (the floor): any item the model answers correctly with no
   store is parametric-answerable for that model; flagged, excluded from the adjusted
   headline. Expected to bite hard on MuSiQue (Wikipedia-derived, public since 2021, in
   pretraining corpora and in hundreds of RAG papers) and weakly on LongMemEval
   (persona-synthetic facts) — but measured, not assumed, for both.
2. **Support-ablation spot check (MuSiQue):** on a 100-item sample, run with the 2–4
   supporting paragraphs removed (distractors only). Items still answered correctly join a
   published drop-list (evidence of memorized answers, not reasoning).
3. **Dataset publicity acknowledged:** LongMemEval public since 2024-10 (cleaned 2025-09);
   MuSiQue since 2021. Both may be verbatim-memorized by 2026 models. The closed-book lane is
   the only honest control available for public datasets; a private refresh set (freshly
   authored LongMemEval-style questions, embargoed) is the stretch mitigation, budget-gated.
4. **No tuning on test (§2.1 splits):** after freeze, any prompt/config/judge change bumps the
   protocol version and voids prior runs — every cell re-runs. The engineering-slice IDs are
   public so reviewers can check the flagged rows.

## 7. Judge design

- **LongMemEval:** LLM judge with the benchmark's own per-type rubric, adapted verbatim from
  the official evaluation scripts (`xiaowu0162/LongMemEval` repo) — the exact prompt text is
  vendored into the harness at freeze (open item; the smoke harness ships a faithful interim
  rubric clearly marked as such). Judge model pinned by snapshot ID, temperature 0.
  **[COI] Bias controls:** (a) the Lane B judge is drawn from a model family **not being
  ranked** in that table wherever possible, (b) a second judge from a different provider
  re-grades every Lane B cell — agreement published; disagreeing items adjudicated by human,
  (c) human calibration: 100 stratified items double-graded by a human before any publication;
  if judge–human agreement (Cohen's κ) < 0.8, the judge is redesigned and everything re-runs,
  (d) judge sees question, gold answer, hypothesis — never the system name.
- **MuSiQue:** mechanical EM/F1 only. No judge, no judge bias surface.

## 8. Compute budget (agent-gated build; spend is founder-gated)

Rough per-cell estimates at 2026-07 list prices, LongMemEval-S (500 q):

| Cell | Est. tokens | Est. USD |
|---|---|---|
| db.md lane, Haiku 4.5 | ~25M in / 1.5M out | ~$35 |
| db.md lane, Sonnet-tier | same | ~$100 |
| db.md lane, Opus-tier | same | ~$165 |
| full-context, per tier | ~58M in / 0.5M out | $60–$300 |
| closed-book, per tier | ~0.3M | <$5 |
| MuSiQue cells | ~2–5M | <$15 each |
| judges (dual) + calibration | — | ~$50 total |

Lane A (5 tiers × 3 lanes × 2 datasets) + Lane B (4 systems × 2 datasets at one pinned
mid-tier model) + 3× repeats on primary cells ≈ **$1.5k–3k** all-in (batch API halves the
full-context lanes). **No full run starts before the founder signs this number.** The smoke
lane (≤10 items, Haiku) costs cents and is the only thing this draft has executed.

## 9. Publication rules — the no-cherry-picking contract

1. **Everything ships:** raw per-item JSONL transcripts (every message, tool call, token count,
   judge verdict), all configs, seeds, item-ID lists, judge prompts, fetch scripts with
   checksums, and the harness — one command reproduces any cell.
2. **All pre-registered cells publish, whatever they say.** If db.md loses a cell, the cell
   publishes. Dropped items publish with reasons (drop-lists are data, not footnotes).
3. **The headline metric is named here, in advance:** LongMemEval-S overall judge accuracy,
   contamination-adjusted view first, full view beside it. Everything else is secondary.
4. **No post-hoc metrics.** New views of the data after freeze go in a clearly-marked
   exploratory appendix, never in the headline.
5. **Deviations log:** any post-freeze change (with date, reason, and what re-ran) is a
   numbered entry in this file. An empty deviations log is part of the claim.
6. **[COI] Claim discipline:** until an independent third party reproduces a Lane B cell,
   public Sevra copy may cite Lane A (the model curve) and the cost table, and must attribute
   Lane B rankings as "vendor-run, reproducible, awaiting independent replication."
7. **Transcripts are the product.** The launch-plan line stands: receipts, not vibes.

## 10. Harness (skeleton shipped with this draft)

`bench/harness/` — TypeScript, run via ts-node (repo conventions), no new dependencies:

- `types.ts` — the `SystemAdapter` contract (`ingest(item)` / `answer(question)` / metered
  usage). External systems (mem0, Letta, Zep) integrate either as adapters or over the same
  per-item JSONL contract from their own runners — the contract, not the language, is pinned.
- `adapters/dbmd.ts` — the db.md lane: builds a fresh store per item via the `dbmd` CLI
  (verbatim session/paragraph files under `sources/`), then answers with an agentic
  tool loop (`search` = `dbmd search` lexical/regex, `read`, `list`; max 8 rounds in smoke).
- `adapters/closedbook.ts` — the contamination-control lane.
- `fetch-datasets.ts` — downloads + checksums artifacts into `bench/data/` (gitignored).
- `judge.ts` — interim LongMemEval rubric judge + MuSiQue EM/F1.
- `run.ts` — eval loop; `--smoke` = ≤10 oracle items end-to-end on
  `claude-haiku-4-5-20251001`; writes `bench/runs/<id>/{config,summary}.json` +
  `transcripts.jsonl`.

Smoke runs exercise plumbing on the **oracle** (evidence-only) file: tiny stores, trivial
retrieval. **A smoke number is never citable** — it exists so the pipeline is real before the
protocol freezes.

---

## Appendix A — dataset facts (researched 2026-07-13)

- **LongMemEval** (Wu et al., ICLR 2025; arXiv:2410.10813). 500 human-curated questions, six
  types + abstention; S ≈ 115k-token haystacks (~40–50 sessions), M ≈ 500 sessions; official
  judge gpt-4o-2024-08-06 with reported >97% human agreement; MIT.
  Repo: <https://github.com/xiaowu0162/LongMemEval>. Cleaned data (2025-09):
  <https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned> (oracle 15.4 MB / S 277 MB /
  M 2.74 GB). v2 (2026-05, agentic trajectories, 451 q, Apache-2.0):
  <https://huggingface.co/datasets/xiaowu0162/longmemeval-v2> — out of scope here (§1).
- **MuSiQue** (Trivedi et al., TACL 2022; arXiv:2108.00573). 2–4-hop questions composed from
  single-hop seeds; MuSiQue-Ans ~25k answerable, MuSiQue-Full adds unanswerable twins; test
  answers withheld (leaderboard); CC BY 4.0; official download = Google Drive artifact.
  Repo: <https://github.com/StonyBrookNLP/musique>. HF mirror:
  <https://huggingface.co/datasets/dgslibisey/MuSiQue>. The repo itself ships
  `dev_test_singlehop_questions_v1.0.json` to manage seed-dataset leakage — contamination is a
  known property, not a surprise.

## Appendix B — the competitive field and its cautionary tale (researched 2026-07-13)

- **mem0** — OSS Apache-2.0, `pip install mem0ai`, largest community (~47k–60k stars).
  Published LoCoMo results claiming SOTA (arXiv:2504.19413; later versions claim ~92 on
  LoCoMo). Public eval suite: <https://github.com/mem0ai/memory-benchmarks>. Open reproduction
  complaints: <https://github.com/mem0ai/mem0/issues/2800>,
  <https://github.com/mem0ai/mem0/issues/3944>.
- **Zep / Graphiti** — Graphiti temporal-knowledge-graph core is Apache-2.0
  (<https://github.com/getzep/graphiti>, ~27k stars); Zep Cloud commercial. Zep's paper
  (arXiv:2501.13956) reports LongMemEval gains of up to +18.5% accuracy vs a full-context
  baseline with ~90% latency reduction. Zep's rebuttal of mem0's LoCoMo claims:
  <https://blog.getzep.com/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/>.
- **Letta (ex-MemGPT)** — Apache-2.0 (<https://github.com/letta-ai/letta>). Their result is
  the closest precedent for the db.md thesis: **filesystem memory + grep at 74.0% on LoCoMo
  (gpt-4o-mini) beat mem0's reported 68.5% graph variant**, and they could not reproduce
  mem0's MemGPT numbers at all:
  <https://www.letta.com/blog/benchmarking-ai-agent-memory/>. They argue retrieval-style
  benchmarks miss agentic memory management — the §1 scope fence exists because that critique
  is partly right.
- **The LoCoMo dispute** (why this protocol is paranoid): three funded vendors, same
  benchmark, same season — mem0 claims SOTA, Zep claims mem0's evaluation of Zep was wrong,
  Letta claims a filesystem matches both and mem0's MemGPT numbers are unexplainable.
  Skeptic synthesis: <https://essays.bloo-mind.ai/posts/2026-05-20-mem-eval/>. Others
  reporting LongMemEval numbers today: Supermemory (<https://supermemory.ai/research/longmembench/>),
  Backboard (<https://github.com/Backboard-io/Backboard-longmemEval-results>) — vendor-run,
  mutually incomparable configs; exactly the gap a neutral pre-registered harness addresses.
