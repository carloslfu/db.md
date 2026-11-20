// SPDX-License-Identifier: Apache-2.0

// bench/harness/fetch-datasets.ts — download eval artifacts into bench/data/ (gitignored)
// and record SHA-256 checksums (bench/data/checksums.json) for the pre-registration pin.
//
// Usage (from bench/):
//   npm run fetch-datasets -- [--s] [--musique[=N]]
//
// Default: longmemeval_oracle.json only (15.4 MB — the smoke/engineering artifact).
//   --s          also fetch longmemeval_s_cleaned.json (~277 MB)
//   --musique=N  fetch N MuSiQue-Ans dev rows via the HF datasets-server mirror (default 200).
//                NOTE: mirror convenience only — the freeze pins the official Google Drive
//                artifact hash (PROTOCOL §2.2); the mirror is fine for plumbing.
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { DATA_DIR } from "./env";

const LME_BASE = "https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main";
const MUSIQUE_ROWS =
  "https://datasets-server.huggingface.co/rows?dataset=dgslibisey%2FMuSiQue&config=default&split=validation";

function sha256(buf: Buffer): string {
  return createHash("sha256").update(buf).digest("hex");
}

function recordChecksum(file: string, hash: string, bytes: number): void {
  const path = join(DATA_DIR, "checksums.json");
  const existing = existsSync(path)
    ? (JSON.parse(readFileSync(path, "utf8")) as Record<string, unknown>)
    : {};
  existing[file] = { sha256: hash, bytes, fetchedAt: new Date().toISOString() };
  writeFileSync(path, `${JSON.stringify(existing, null, 2)}\n`);
}

async function download(url: string, destName: string): Promise<void> {
  const dest = join(DATA_DIR, destName);
  if (existsSync(dest)) {
    console.log(`✓ ${destName} already present (delete to re-fetch)`);
    return;
  }
  console.log(`↓ ${url}`);
  const res = await fetch(url);
  if (!res.ok) throw new Error(`fetch failed ${res.status} for ${url}`);
  const buf = Buffer.from(await res.arrayBuffer());
  writeFileSync(dest, buf);
  const hash = sha256(buf);
  recordChecksum(destName, hash, buf.length);
  console.log(`✓ ${destName} (${(buf.length / 1e6).toFixed(1)} MB, sha256=${hash.slice(0, 16)}…)`);
}

type HfRowsResponse = { rows: Array<{ row: Record<string, unknown> }>; num_rows_total?: number };

async function fetchMusique(count: number): Promise<void> {
  const destName = "musique_ans_dev.jsonl";
  const dest = join(DATA_DIR, destName);
  if (existsSync(dest)) {
    console.log(`✓ ${destName} already present (delete to re-fetch)`);
    return;
  }
  const lines: string[] = [];
  const pageSize = 100;
  for (let offset = 0; offset < count; offset += pageSize) {
    const length = Math.min(pageSize, count - offset);
    const url = `${MUSIQUE_ROWS}&offset=${offset}&length=${length}`;
    console.log(`↓ musique rows ${offset}..${offset + length}`);
    const res = await fetch(url);
    if (!res.ok) throw new Error(`fetch failed ${res.status} for ${url}`);
    const data = (await res.json()) as HfRowsResponse;
    for (const { row } of data.rows) lines.push(JSON.stringify(row));
    if (data.rows.length < length) break; // end of split
  }
  const buf = Buffer.from(`${lines.join("\n")}\n`);
  writeFileSync(dest, buf);
  recordChecksum(destName, sha256(buf), buf.length);
  console.log(`✓ ${destName} (${lines.length} rows)`);
}

export async function fetchDatasets(argv: string[]): Promise<void> {
  mkdirSync(DATA_DIR, { recursive: true });
  await download(`${LME_BASE}/longmemeval_oracle.json`, "longmemeval_oracle.json");
  if (argv.includes("--s")) {
    await download(`${LME_BASE}/longmemeval_s_cleaned.json`, "longmemeval_s_cleaned.json");
  }
  const musiqueArg = argv.find((a) => a === "--musique" || a.startsWith("--musique="));
  if (musiqueArg) {
    const n = Number(musiqueArg.split("=")[1] ?? 200);
    await fetchMusique(Number.isFinite(n) && n > 0 ? n : 200);
  }
}

if (require.main === module) {
  fetchDatasets(process.argv.slice(2)).catch((e) => {
    console.error(e instanceof Error ? e.message : e);
    process.exit(1);
  });
}
