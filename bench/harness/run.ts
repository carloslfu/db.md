// SPDX-License-Identifier: Apache-2.0

// bench/harness/run.ts — the eval loop. Per-item JSONL transcripts + a run summary.
//
// Smoke (≤10 items, oracle artifact, Haiku, hard spend ceiling — PROTOCOL §10), from bench/:
//   npm run smoke
//
// General (from bench/):
//   npx tsx harness/run.ts --dataset=longmemeval|musique [--file=path] --adapter=dbmd|closedbook
//          [--limit=N] [--model=ID] [--judge-model=ID] [--max-rounds=N] [--run-id=NAME]
//
// Smoke runs exercise plumbing only and are never citable (PROTOCOL §10).
import { mkdirSync, rmSync, writeFileSync, appendFileSync } from "node:fs";
import { join, relative } from "node:path";
import { tmpdir } from "node:os";
import { SpendMeter } from "./anthropic";
import { ClosedBookAdapter } from "./adapters/closedbook";
import { DbmdAdapter } from "./adapters/dbmd";
import { loadLongMemEval, loadMusique, smokeSlice } from "./datasets";
import { DATA_DIR, REPO_ROOT, RUNS_DIR, loadEnvLocal, requireEnv } from "./env";
import { longmemVerdict, musiqueVerdict, abstentionVerdict } from "./judge";
import type { DatasetName, EvalItem, ItemRecord, RunSummary, SystemAdapter } from "./types";

const SMOKE_MODEL = "claude-haiku-4-5-20251001";

type Args = {
  smoke: boolean;
  dataset: DatasetName;
  file?: string;
  adapter: "dbmd" | "closedbook";
  limit: number;
  model: string;
  judgeModel: string;
  maxRounds: number;
  runId: string;
  maxSpendUsd: number;
};

function parseArgs(argv: string[]): Args {
  const get = (name: string): string | undefined => {
    const hit = argv.find((a) => a === `--${name}` || a.startsWith(`--${name}=`));
    if (!hit) return undefined;
    const eq = hit.indexOf("=");
    return eq === -1 ? "true" : hit.slice(eq + 1);
  };
  const smoke = get("smoke") !== undefined;
  const dataset = (get("dataset") ?? "longmemeval") as DatasetName;
  const model = get("model") ?? (smoke ? SMOKE_MODEL : SMOKE_MODEL);
  const today = new Date().toISOString().slice(0, 10);
  return {
    smoke,
    dataset,
    file: get("file"),
    adapter: (get("adapter") ?? "dbmd") as Args["adapter"],
    limit: Number(get("limit") ?? (smoke ? 8 : 0)) || (smoke ? 8 : Number.MAX_SAFE_INTEGER),
    model,
    judgeModel: get("judge-model") ?? SMOKE_MODEL,
    maxRounds: Number(get("max-rounds") ?? 8),
    runId: get("run-id") ?? (smoke ? `smoke-${today}` : `run-${today}-${Date.now()}`),
    maxSpendUsd: Number(get("max-spend-usd") ?? process.env.BENCH_MAX_SPEND_USD ?? 0.8),
  };
}

function datasetFile(args: Args): string {
  if (args.file) return args.file;
  if (args.dataset === "longmemeval") {
    // Smoke/engineering artifact. Reported runs pass --file=<longmemeval_s_cleaned.json>.
    return join(DATA_DIR, "longmemeval_oracle.json");
  }
  return join(DATA_DIR, "musique_ans_dev.jsonl");
}

function loadItems(args: Args): EvalItem[] {
  const file = datasetFile(args);
  const all = args.dataset === "longmemeval" ? loadLongMemEval(file) : loadMusique(file);
  if (args.smoke) {
    if (args.limit > 10) throw new Error("--smoke caps at 10 items");
    return smokeSlice(all, args.limit);
  }
  return all.slice(0, args.limit);
}

export async function main(): Promise<void> {
  loadEnvLocal();
  const args = parseArgs(process.argv.slice(2));
  const apiKey = requireEnv("ANTHROPIC_API_KEY");
  const meter = new SpendMeter(args.maxSpendUsd);

  const adapter: SystemAdapter =
    args.adapter === "dbmd"
      ? new DbmdAdapter({ apiKey, model: args.model, meter, maxRounds: args.maxRounds })
      : new ClosedBookAdapter({ apiKey, model: args.model, meter });

  const items = loadItems(args);
  const runDir = join(RUNS_DIR, args.runId);
  mkdirSync(runDir, { recursive: true });
  const transcriptsPath = join(runDir, "transcripts.jsonl");
  writeFileSync(transcriptsPath, "");
  const workRoot = join(tmpdir(), `bench-${args.runId}`);

  const startedAt = new Date().toISOString();
  const t0 = Date.now();
  const summaryItems: RunSummary["items"] = [];
  const byType: Record<string, { n: number; correct: number }> = {};
  let correct = 0;
  let aborted: string | undefined;
  let totalToolCalls = 0;

  writeFileSync(
    join(runDir, "config.json"),
    `${JSON.stringify({ ...args, datasetFile: relative(REPO_ROOT, datasetFile(args)), itemIds: items.map((i) => i.id), protocol: "DRAFT-2026-07-13" }, null, 2)}\n`,
  );

  console.log(
    `run ${args.runId}: ${items.length} items | ${args.dataset} | adapter=${adapter.name} | model=${args.model} | ceiling=$${args.maxSpendUsd}`,
  );

  for (const item of items) {
    const itemT0 = Date.now();
    try {
      const workDir = join(workRoot, item.id.replace(/[^a-zA-Z0-9_-]/g, "_"));
      mkdirSync(workDir, { recursive: true });
      const ingest = await adapter.ingest(item, workDir);
      const usdBefore = meter.totalUsd;
      const ans = await adapter.answer(item, workDir);
      const verdict = !item.answerable
        ? abstentionVerdict(ans.hypothesis)
        : item.dataset === "musique"
          ? musiqueVerdict(item, ans.hypothesis)
          : await longmemVerdict({
              apiKey,
              judgeModel: args.judgeModel,
              item,
              hypothesis: ans.hypothesis,
              meter,
            });
      const record: ItemRecord = {
        id: item.id,
        type: item.type,
        question: item.question,
        questionDate: item.questionDate,
        gold: item.gold,
        answerable: item.answerable,
        hypothesis: ans.hypothesis,
        verdict,
        ingest,
        usage: ans.usage,
        toolTrace: ans.toolTrace,
        messages: ans.messages,
        wallClockMs: Date.now() - itemT0,
      };
      appendFileSync(transcriptsPath, `${JSON.stringify(record)}\n`);
      totalToolCalls += ans.usage.toolCalls;
      if (verdict.correct) correct++;
      const bucket = (byType[item.type] ??= { n: 0, correct: 0 });
      bucket.n++;
      if (verdict.correct) bucket.correct++;
      summaryItems.push({
        id: item.id,
        type: item.type,
        correct: verdict.correct,
        costUsd: Number((meter.totalUsd - usdBefore).toFixed(5)),
      });
      console.log(
        `  ${verdict.correct ? "✓" : "✗"} ${item.id} [${item.type}] tools=${ans.usage.toolCalls} $${meter.totalUsd.toFixed(3)} — "${ans.hypothesis.slice(0, 70)}"`,
      );
      rmSync(workDir, { recursive: true, force: true });
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      if (msg.includes("spend ceiling")) {
        aborted = msg;
        console.error(`ABORT: ${msg}`);
        break;
      }
      appendFileSync(
        transcriptsPath,
        `${JSON.stringify({ id: item.id, type: item.type, error: msg })}\n`,
      );
      const bucket = (byType[item.type] ??= { n: 0, correct: 0 });
      bucket.n++;
      summaryItems.push({ id: item.id, type: item.type, correct: false, costUsd: 0 });
      console.error(`  ✗ ${item.id} ERROR: ${msg.slice(0, 200)}`);
    }
  }
  rmSync(workRoot, { recursive: true, force: true });

  const n = summaryItems.length;
  const summary: RunSummary = {
    runId: args.runId,
    startedAt,
    finishedAt: new Date().toISOString(),
    protocol: "DRAFT-2026-07-13 (pre-freeze; not citable)",
    smoke: args.smoke,
    dataset: args.dataset,
    datasetFile: relative(REPO_ROOT, datasetFile(args)),
    adapter: adapter.name,
    model: args.model,
    judgeModel: args.judgeModel,
    maxRounds: args.maxRounds,
    n,
    correct,
    accuracy: n ? Number((correct / n).toFixed(4)) : 0,
    byType,
    totals: {
      inputTokens: meter.inputTokens,
      outputTokens: meter.outputTokens,
      apiCalls: meter.apiCalls,
      toolCalls: totalToolCalls,
      costUsd: Number(meter.totalUsd.toFixed(4)),
    },
    wallClockMs: Date.now() - t0,
    ...(aborted ? { aborted } : {}),
    items: summaryItems,
  };
  writeFileSync(join(runDir, "summary.json"), `${JSON.stringify(summary, null, 2)}\n`);
  console.log(
    `\n${args.runId}: ${correct}/${n} (${(summary.accuracy * 100).toFixed(1)}%) | ` +
      `${meter.apiCalls} API calls, ${meter.inputTokens} in / ${meter.outputTokens} out tokens, ` +
      `$${meter.totalUsd.toFixed(4)} | ${(summary.wallClockMs / 1000).toFixed(0)}s`,
  );
  console.log(`summary: ${join(runDir, "summary.json")}`);
}

if (require.main === module) {
  main().catch((e) => {
    console.error(e instanceof Error ? e.stack || e.message : e);
    process.exit(1);
  });
}
