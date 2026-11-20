// SPDX-License-Identifier: Apache-2.0

import { existsSync } from "node:fs";
import { spawn, type ChildProcess } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import react from "@vitejs/plugin-react";
import { defineConfig, type Plugin } from "vite";

// Dev convenience ONLY — this is not a backend. The page talks straight to
// `dbmd api` (the db.md toolkit's own local app server); vite serves static
// files and, in dev/preview, spawns that server beside itself so `npm run
// dev` stays one command. Kill vite and run `dbmd api --dir db` yourself and
// the built page works identically from any static host.
function spawnDbmdApi(): Plugin {
  let child: ChildProcess | undefined;
  const dir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "db");
  const installed = path.join(os.homedir(), ".dbmd", "bin", "dbmd");
  const bin = process.env.DBMD_BIN ?? (existsSync(installed) ? installed : "dbmd");

  const start = (withHarness = true) => {
    if (child) return;
    // --ask/--do serve the ask view (toolkit 0.11+); an older installed
    // dbmd rejects the flags with a clap usage error (exit 2), so fall
    // back to the plain api — every view but ask still works.
    const args = withHarness
      ? ["api", "--ask", "--do", "--addr", "127.0.0.1:3261", "--dir", dir]
      : ["api", "--addr", "127.0.0.1:3261", "--dir", dir];
    child = spawn(bin, args, {
      stdio: "inherit",
    });
    child.on("error", (error) => {
      console.error(
        `[dbmd api] could not start \`${bin}\` (${error.message}) — install dbmd or set DBMD_BIN, or run it yourself: dbmd api --dir db`,
      );
      child = undefined;
    });
    child.on("exit", (code) => {
      child = undefined;
      if (withHarness && code === 2) {
        console.error(
          `[dbmd api] \`${bin}\` predates the embedded harness (dbmd 0.11+) — restarting without --ask/--do; the ask view will show its setup notice. Upgrade dbmd or set DBMD_BIN to a newer build.`,
        );
        start(false);
        return;
      }
      if (code !== null && code !== 0) {
        console.error(
          `[dbmd api] exited with code ${code} — port 127.0.0.1:3261 in use? If another \`dbmd api\` already serves db/, this is fine.`,
        );
      }
    });
  };
  const stop = () => {
    child?.kill();
    child = undefined;
  };

  return {
    name: "todomvc-dbmd-spawn-api",
    configureServer(server) {
      start();
      server.httpServer?.once("close", stop);
    },
    configurePreviewServer(server) {
      start();
      server.httpServer.once("close", stop);
    },
  };
}

export default defineConfig({
  plugins: [react(), spawnDbmdApi()],
  server: { port: 5183, strictPort: true },
  preview: { port: 5183, strictPort: true },
});
