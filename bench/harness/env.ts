// SPDX-License-Identifier: Apache-2.0

// bench/harness/env.ts — self-load bench/.env.local (scripts self-load, never dotenv).
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";

export const REPO_ROOT = resolve(__dirname, "..", "..");
export const BENCH_DIR = resolve(REPO_ROOT, "bench");
export const DATA_DIR = resolve(BENCH_DIR, "data");
export const RUNS_DIR = resolve(BENCH_DIR, "runs");

export function loadEnvLocal(): void {
  const envPath = resolve(BENCH_DIR, ".env.local");
  if (!existsSync(envPath)) return;
  for (const rawLine of readFileSync(envPath, "utf8").split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    const eq = line.indexOf("=");
    if (eq <= 0) continue;
    const key = line.slice(0, eq).trim();
    let value = line.slice(eq + 1).trim();
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1);
    }
    if (!(key in process.env)) process.env[key] = value;
  }
}

export function requireEnv(key: string): string {
  const value = process.env[key];
  if (!value) {
    throw new Error(`${key} is not set (expected in environment or bench/.env.local)`);
  }
  return value;
}
