// Serve a presentation file as a fresh briefing from the debug binary (override with
// BRIEFING_BIN), fully isolated from the user's config, hub and briefing store: `present`
// returns at once, and the briefing lives in the on-demand hub it starts in its own state dir
// on a free loopback port. `stopAll` kills those hubs and removes their state.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const BINARY = process.env.BRIEFING_BIN || join(here, "../../target/debug/briefing");

const servers = [];

/** Present `file` and return its page URL. */
export async function serve(file) {
  const dir = mkdtempSync(join(tmpdir(), "briefing-browser-"));
  const config = join(dir, "config.toml");
  writeFileSync(config, "");
  const state = join(dir, "briefings");
  const env = { ...process.env, BRIEFING_CONFIG: config, XDG_STATE_HOME: join(dir, "state"), BRIEFING_STATE_DIR: state, BRIEFING_PORT: "0" };
  for (const key of Object.keys(env)) if (key.startsWith("BRIEFING_") && !["BRIEFING_CONFIG", "BRIEFING_STATE_DIR", "BRIEFING_PORT"].includes(key)) delete env[key];
  // Nor does the agent session running these get stamped on the briefing.
  delete env.CLAUDE_CODE_SESSION_ID;
  delete env.CODEX_THREAD_ID;
  servers.push({
    stop: () => {
      try { process.kill(JSON.parse(readFileSync(join(state, ".hub.json"), "utf8")).pid, "SIGKILL"); } catch { /* never started */ }
      rmSync(dir, { recursive: true, force: true });
    },
  });
  const stdout = await new Promise((resolve, reject) => {
    const child = spawn(BINARY, ["present", file, "--json", "--bind", "local", "--open", "false"], { env, stdio: ["ignore", "pipe", "inherit"] });
    let out = "";
    child.stdout.on("data", chunk => (out += chunk));
    child.on("error", reject);
    child.on("exit", code => (code === 0 ? resolve(out) : reject(new Error(`briefing present exited with ${code}`))));
  });
  const { url } = JSON.parse(stdout);
  assert.match(url, /^http:\/\/127\.0\.0\.1:/, "never reach a real hub");
  return url;
}

export function stopAll() {
  for (const server of servers.splice(0)) server.stop();
}
