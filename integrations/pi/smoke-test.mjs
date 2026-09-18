import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import { createJiti } from "jiti";

const jiti = createJiti(import.meta.url, { moduleCache: false, interopDefault: true });
const extensionPath = fileURLToPath(new URL("./briefing.ts", import.meta.url));
const extension = await jiti.import(extensionPath, { default: true });

const baseTools = ["read", "bash", "brief_user"];

function createHarness() {
  const handlers = new Map();
  const commands = new Map();
  const messages = [];
  let activeTools = [...baseTools];

  const pi = {
    on(event, handler) {
      handlers.set(event, handler);
    },
    registerTool() {},
    registerCommand(name, command) {
      commands.set(name, command);
    },
    getActiveTools() {
      return activeTools;
    },
    setActiveTools(names) {
      activeTools = names;
    },
    sendUserMessage(message) {
      messages.push(message);
    },
  };

  extension(pi);

  return {
    commands,
    handlers,
    messages,
    get activeTools() {
      return activeTools;
    },
  };
}

const ctx = {
  mode: "tui",
  isIdle: () => true,
  ui: {
    notify() {},
  },
};

const harness = createHarness();

assert.ok(harness.handlers.has("before_agent_start"), "registers before_agent_start handler");
assert.ok(harness.handlers.has("agent_settled"), "registers agent_settled handler");
assert.ok(harness.handlers.has("session_shutdown"), "registers session_shutdown handler");
assert.ok(harness.commands.has("brief"), "registers /brief command");
assert.ok(harness.commands.has("brief-demo"), "registers /brief-demo command");
assert.ok(harness.commands.has("brief-result"), "registers /brief-result command");

const forcedPrompt = async () => (await harness.handlers.get("before_agent_start")({ systemPrompt: "base" }))?.systemPrompt;

// Clearing with nothing queued must be a no-op, not a throw.
await harness.handlers.get("agent_settled")();
assert.equal(await forcedPrompt(), undefined, "no queued instruction without a command");

// /brief queues a prompt through the same mechanism, but enables no extra tool.
await harness.commands.get("brief").handler("summarise the design", ctx);
assert.deepEqual(harness.messages, ["summarise the design"]);
assert.deepEqual(harness.activeTools, baseTools, "/brief enables no command-only tool");
assert.match(await forcedPrompt(), /brief_user/);
await harness.handlers.get("agent_settled")();
assert.equal(await forcedPrompt(), undefined, "agent_settled clears the queued /brief prompt");

await harness.commands.get("brief-result").handler("abc123", ctx);
assert.equal(harness.messages.at(-1), "Recover briefing abc123.");
assert.ok(harness.activeTools.includes("briefing_result"), "/brief-result enables the result tool");

const resultPrompt = await forcedPrompt();
assert.match(resultPrompt, /briefing_result/);
assert.match(resultPrompt, /abc123/);

// Forcing again without an intervening agent_settled must swap, not stack: this is the
// de-duplication forceNextTurn() relies on to keep the active-tool list clean.
await harness.commands.get("brief-demo").handler("", ctx);
assert.equal(harness.messages.at(-1), "Open the bundled briefing demo.");
assert.ok(harness.activeTools.includes("briefing_demo"), "/brief-demo enables the demo tool");
assert.ok(!harness.activeTools.includes("briefing_result"), "the superseded result tool is torn down");

const demoPrompt = await forcedPrompt();
assert.match(demoPrompt, /briefing_demo/);
assert.doesNotMatch(demoPrompt, /abc123/, "the superseded result prompt does not leak into the next turn");

await harness.handlers.get("session_shutdown")();
assert.deepEqual(harness.activeTools, baseTools, "session_shutdown restores the original tool list");
assert.equal(await forcedPrompt(), undefined, "session_shutdown clears the queued prompt");

console.log("briefing pi extension smoke passed");
