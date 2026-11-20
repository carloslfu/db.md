# Kill-gate — adversarial review of bench/PROTOCOL.md

**Run:** 2026-07-13, against the same-day PROTOCOL.md draft. Reviewer stance: an HN skeptic who
watched the LoCoMo season, plus an academic reviewer. Each attack ends in one of:
**SURVIVES** (protocol already answers it) / **AMENDED** (protocol changed because of the
attack — the amendment is folded into PROTOCOL.md) / **KILLS** (structural; do not ship that
part as designed).

**Verdict, up front: DO NOT SHIP AS ORIGINALLY CONCEIVED — ship the amended two-lane design.**
The single-headline design ("db.md beats mem0/Zep/Letta on LongMemEval, says the company that
sells db.md hosting") does not survive attack #1 no matter how clean the harness is; the
LoCoMo season proves the genre is radioactive. What survives scrutiny is: **Lane A** (the
model curve — self-comparative, no competitor ranking, directly tests the actual thesis) plus
the **cost table**, shippable on our own authority; **Lane B** (cross-system) shippable only
as "vendor-run, fully reproducible, right-of-reply exercised, awaiting independent
replication" — never as a bare leaderboard. MuSiQue is demoted from co-headline to
contamination-adjusted rigor anchor. With those amendments (all now in PROTOCOL.md), the
remaining attacks are survivable. Six pre-registration gaps must close before freeze (§end).

---

## Attack 1 — "The vendor ran the benchmark. Next."

*mem0 ranked itself SOTA; Zep re-scored mem0's numbers; Letta couldn't reproduce either. Why
would anyone read line one of a Sevra-run table where db.md wins?*

The strongest attack, and partially fatal. No amount of raw-transcript publishing makes a
vendor-run ranking neutral — publishing transcripts makes it *auditable*, not *credible*;
nobody audits, everybody screenshots the table.

**Verdict: AMENDED (structurally).** (a) Two-lane split: the headline announcement is Lane A,
the model curve on db.md alone — a claim about *our own system's scaling behavior* with
falsifiable pre-registered predictions (P1–P3), where vendor bias has no competitor to injure.
(b) Lane B carries mandatory framing ("vendor-run… awaiting independent replication"), the
right-of-reply protocol, not-run-instead-of-zero, and one-command repro. (c) Claim discipline
written into publication rules (§9.6): Sevra marketing may not cite Lane B rankings as fact
until an external replication exists. Residual risk accepted: some readers dismiss Lane B
anyway. That is the correct price.

## Attack 2 — "MuSiQue is in every pretraining set on earth."

*Wikipedia-derived, public since 2021, quoted in hundreds of RAG papers. Your model answers
from weights, your memory system takes the credit.*

Correct, and the original draft's "MuSiQue as rigor anchor" undersold it. Also: the only
usable split is **dev** (test answers are leaderboard-withheld), the most-published split.

**Verdict: AMENDED.** Closed-book lane per model (the floor is measured, not assumed);
headline repeated on the non-parametric subset; 100-item support-ablation with a published
drop-list; MuSiQue explicitly reframed as the cross-record-join anchor whose
contamination-adjusted view is the only citable one. **If the closed-book floor lands above
~60% for frontier models, MuSiQue is dead for this protocol** — that tripwire is now written
down in advance (pre-registration gap G5: measure the floor on 50 items before freeze; cost
≈ $1).

## Attack 3 — "LLM judge grading its own family."

*Claude judges Claude answers about Claude-operated stores. The 97% human-agreement number is
the LongMemEval authors', on their outputs, with gpt-4o — not yours.*

**Verdict: AMENDED.** Dual-judge from different providers on every Lane B cell with published
agreement; Lane B judge drawn from a family not ranked in the table where possible; our own
human calibration (κ ≥ 0.8 on 100 items) gates publication — the authors' 97% is not
inherited; judge never sees the system name; MuSiQue is judge-free EM/F1 (half the benchmark
has zero judge surface). Residual: Lane A judges its own family at some tiers (unavoidable
when the curve includes Anthropic models); mitigated by the same fixed judge across all tiers
of a curve — judge bias becomes a constant offset, and the *curve shape* (the actual claim) is
invariant to it.

## Attack 4 — "Apples to oranges: these systems don't even claim the same job."

*mem0 pays LLM tokens at write time to compress; Zep builds a graph; db.md pays at read time;
full-context pays maximally. A single accuracy column erases the design space.*

**Verdict: AMENDED.** The cost table became **co-primary** (§4): ingest tokens and query
tokens reported separately for every cell, USD both phases, wall-clock; systems labeled by
class (extraction-pipeline / temporal-graph / agent-filesystem / baseline). No composite
"memory score" is ever computed. The publishable claim shape is a *frontier* (accuracy vs.
cost), not a ranking. This also converts db.md's zero-LLM-ingestion property from hidden
advantage into an audited number.

## Attack 5 — "You tuned on the test set. Everyone does."

*500 public questions, no held-out split, and you'll iterate prompts until the number looks
good — then 'pre-register.'*

**Verdict: AMENDED, with an honest residual.** Engineering slice pinned and enumerated (oracle
file + 30 seeded S-items, IDs published; flagged rows in every table); post-freeze changes
version-bump and void prior runs; deviations log required, emptiness part of the claim.
**Residual, stated plainly in the protocol:** the protocol authors have read both datasets;
design-time contamination of the authors is not fully controllable for any public benchmark.
The mitigations are the enumerated slice + all-cells-publish + transcripts. A privately
authored refresh set is the real fix and is named as the stretch lane.

## Attack 6 — "n=1 run, temperature whatever, and a 2-point 'win.'"

**Verdict: SURVIVES (already in draft, tightened).** Bootstrap CIs over items on every cell;
3 independent runs on primary cells (mean ± range); pinned params; the no-claim-on-overlapping-
CIs rule is explicit in §4. Per-item JSONL means anyone can recompute any statistic.

## Attack 7 — "Why these two datasets? Convenient picks."

*LoCoMo is discredited, fine — but LongMemEval is a retrieval benchmark (Letta's point), and
MuSiQue isn't a memory benchmark at all.*

**Verdict: SURVIVES, with the scope fence.** The choice is defensible on the record:
LongMemEval is the human-curated successor the field itself reached for after LoCoMo fell
(typed abilities, abstention, an oracle lane, a cleaned 2025-09 release); MuSiQue is the
hardest standard compositional multi-hop set and maps to the *database* half of the claim
(cross-record joins), not the chat-memory half. The §1 scope fence concedes Letta's critique
explicitly: this protocol measures read-side memory over a fixed history; it does not measure
agentic memory management, and LongMemEval-v2 is named as the follow-on lane. What would NOT
survive: claiming "best agent memory" off these two — the claim discipline rule (§9.6)
prevents that sentence.

## Attack 8 — "Your smoke run is on the oracle file — retrieval with the haystack removed."

**Verdict: SURVIVES (by disclosure).** True and stated in §10: oracle smoke = plumbing
verification, never citable, never aggregated with real cells. Real cells run on the S
haystacks.

## Attack 9 — "BYO-agent means the harness IS the memory system — and you wrote the harness."

*The db.md 'system' is really your agent loop + prompts. You'll polish that loop; competitors
get their defaults.*

Sharp, and half-true by construction: in the db.md thesis the agent loop *is* the product
surface (the store is plain files; the intelligence is the operator).

**Verdict: AMENDED.** The loop is published as part of the system-under-test (prompts, tool
budget, params — pinned at freeze); competitors run their own *vendor-recommended* loops (not
our reimplementation) under rule §3.1–3.3, with right-of-reply; the engineering-slice fence
(attack 5) bounds how much polish happens against test items; and the same fixed loop runs
across every Lane A tier, so curve-shape claims are loop-invariant. Residual: cross-system
Lane B remains partly a comparison of *integration effort* — a known, disclosed property of
every systems benchmark, and another reason Lane B is not the headline.

## Attack 10 — "LongMemEval-S is near-saturated at the frontier; you're measuring noise."

*Vendors already report 63–90%+; frontier full-context may clear 85%. Your deltas will live
inside the CI.*

**Verdict: SURVIVES — the design profits from it.** Lane A's prediction P3 *is* the
saturation story: if frontier db.md converges to the full-context ceiling at a fraction of the
cost, saturation of accuracy makes cost the discriminator, which is the honest end-state of
memory benchmarks. Cross-system Lane B deltas inside overlapping CIs are reported as ties
(§4). If the whole S-table saturates for every system, the M-variant (500-session haystacks)
is the pre-registered escalation, budget permitting.

---

## What this gate could not test (honesty section)

- Whether the amended protocol *actually* survives public contact — this file is
  self-adversarial review, not the independent review the launch line promises. The Act-1
  line ("no recall claims until it survives adversarial review") is satisfied only when
  external skeptics have had their pass — treat this gate as the entry ticket, not the
  verdict.
- Whether vendors reply within the right-of-reply window (external-party-gated).
- The real closed-book contamination floor (G5 below) and the real saturation level — both are
  cheap pre-freeze measurements, not opinions.

## Pre-registration gaps — must close before the freeze commit

- **G1.** Vendor the official LongMemEval per-type judge prompts verbatim into the harness
  (interim rubric is marked interim); pin judge snapshot IDs.
- **G2.** Pin the full model-tier list with snapshot IDs from ≥2 providers (Lane A), incl. the
  non-Anthropic lane's auth/runner path.
- **G3.** Enumerate the seeded item-ID lists (30 LongMemEval engineering items; 500+100
  MuSiQue sample) and commit them.
- **G4.** Implement + dry-run one external adapter (mem0 OSS is the cheapest) to prove the
  cross-system JSONL contract holds beyond our own two adapters, and draft the right-of-reply
  email.
- **G5.** Measure the closed-book floors (50 items × frontier model, both datasets, ≈ $1–2);
  apply the MuSiQue tripwire from attack 2.
- **G6.** Founder sign-off on the §8 budget; then record the freeze commit hash in
  PROTOCOL.md.

**Final ruling:** the benchmark is shippable **only** in the amended two-lane form, and only
after G1–G6. The original one-table conception is dead on attack 1 and stays dead.
