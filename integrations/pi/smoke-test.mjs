import assert from "node:assert/strict";
import { afterEach, before, test } from "node:test";
import { fileURLToPath } from "node:url";

import { createJiti } from "jiti";

// The extension reads BRIEFING_BIN when it loads, so point it at the fake CLI first.
process.env.BRIEFING_BIN = fileURLToPath(new URL("./fake-briefing.mjs", import.meta.url));

const SECTION = "briefing_command";
const INTERACTION_TOOLS = ["brief_user", "briefing_demo", "briefing_result"];
const baseTools = ["read", "bash", "brief_user"];

let extension;
let pendingBriefingId;
let supportsPi;
before(async () => {
  const jiti = createJiti(import.meta.url, { moduleCache: false, interopDefault: true });
  const path = fileURLToPath(new URL("./briefing.ts", import.meta.url));
  extension = await jiti.import(path, { default: true });
  ({ pendingBriefingId, supportsPi } = await jiti.import(path));
});
afterEach(() => { delete process.env.BRIEFING_TEST_STATUS; });

/** Load the extension against a minimal Pi host that records what it is asked to do. */
function createHarness({ branch = [] } = {}) {
  const handlers = new Map();
  const commands = new Map();
  const tools = new Map();
  const messages = [];
  const entries = [];
  let activeTools = [...baseTools];
  let terminalInput;
  let aborts = 0;

  extension({
    on: (event, handler) => handlers.set(event, handler),
    registerTool: (tool) => tools.set(tool.name, tool),
    registerCommand: (name, command) => commands.set(name, command),
    getActiveTools: () => activeTools,
    setActiveTools: (names) => { activeTools = names; },
    sendUserMessage: (message) => messages.push(message),
    appendEntry: (customType, data) => entries.push({ type: "custom", customType, data }),
  });

  const ctx = {
    mode: "tui",
    isIdle: () => true,
    abort: () => { aborts++; },
    sessionManager: { getBranch: () => branch },
    ui: {
      notify() {},
      setWorkingMessage() {},
      setStatus() {},
      setWidget() {},
      onTerminalInput(handler) {
        terminalInput = handler;
        return () => { terminalInput = undefined; };
      },
    },
  };

  return {
    tools,
    messages,
    entries,
    ctx,
    get activeTools() { return activeTools; },
    get aborts() { return aborts; },
    pressKey: (key) => terminalInput(key),
    command: (name, args = "") => commands.get(name).handler(args, ctx),
    emit: (event, payload) => handlers.get(event)(payload, ctx),
    run: (name, signal, onUpdate) => tools.get(name).execute("call", {}, signal, onUpdate, ctx),
    /** Run before_agent_start and return the command section it left, if any. */
    async commandSection() {
      const systemPromptOptions = { sections: { other_extension: "kept" } };
      const result = await handlers.get("before_agent_start")({ systemPrompt: "base", systemPromptOptions }, ctx);
      assert.equal(result?.systemPrompt, undefined, "never replaces the whole system prompt");
      assert.equal(systemPromptOptions.sections.other_extension, "kept", "leaves other extensions' sections alone");
      return systemPromptOptions.sections[SECTION];
    },
  };
}

test("refuses Pi releases older than tool exposure", () => {
  for (const version of ["0.99.1", "0.99.2", "0.100.0", "1.0.4", "1.0.0-beta.1"]) assert.ok(supportsPi(version), version);
  for (const version of ["0.99.0", "0.86.0", "0.9.9", "unknown"]) assert.ok(!supportsPi(version), version);
});

test("every interaction tool is model-only, and only brief_user starts active", async () => {
  const pi = createHarness();
  await pi.emit("session_start", { reason: "new" });
  for (const name of INTERACTION_TOOLS) assert.equal(pi.tools.get(name)?.exposure, "model-only", name);
  assert.deepEqual(pi.activeTools, baseTools);
  assert.deepEqual(pi.tools.get("brief_user").promptGuidelines, ["Fixture briefing guidance"]);
});

test("/brief adds a command section for one turn and enables no extra tool", async () => {
  const pi = createHarness();
  await pi.emit("agent_settled"); // nothing queued: a no-op, not a throw
  assert.equal(await pi.commandSection(), undefined);

  await pi.command("brief", "summarise the design");
  assert.deepEqual(pi.messages, ["summarise the design"]);
  assert.deepEqual(pi.activeTools, baseTools);
  assert.match(await pi.commandSection(), /brief_user/);

  await pi.emit("agent_settled");
  assert.equal(await pi.commandSection(), undefined, "the following ordinary turn has no command section");
});

test("a second command replaces the first instead of stacking", async () => {
  const pi = createHarness();
  await pi.command("brief-result", "abc123");
  assert.ok(pi.activeTools.includes("briefing_result"));
  assert.match(await pi.commandSection(), /briefing_result[\s\S]*abc123|abc123[\s\S]*briefing_result/);

  await pi.command("brief-demo");
  assert.ok(pi.activeTools.includes("briefing_demo"));
  assert.ok(!pi.activeTools.includes("briefing_result"), "the superseded tool is torn down");
  const section = await pi.commandSection();
  assert.match(section, /briefing_demo/);
  assert.doesNotMatch(section, /abc123/);

  await pi.emit("session_shutdown");
  assert.deepEqual(pi.activeTools, baseTools);
  assert.equal(await pi.commandSection(), undefined);
});

test("command tools tear down on completion, cancellation and failure", async () => {
  const pi = createHarness();
  await pi.emit("session_start", { reason: "new" });
  const settledAfter = async (label) => {
    assert.deepEqual(pi.activeTools, baseTools, `${label} clears the command-only tool`);
    assert.equal(await pi.commandSection(), undefined, `${label} clears the command section`);
  };

  await pi.command("brief-demo");
  await pi.run("briefing_demo");
  await settledAfter("demo completion");

  await pi.command("brief-result", "stored-id");
  const recovered = await pi.run("briefing_result");
  assert.equal(recovered.details.briefingId, "stored-id", "the recovery id comes from the command, not the model");
  await settledAfter("recovery completion");

  for (const [status, error] of [["cancelled", /cancelled/], ["failed", /fixture failure/]]) {
    process.env.BRIEFING_TEST_STATUS = status;
    await pi.command("brief-result", "stored-id");
    await assert.rejects(pi.run("briefing_result"), error);
    await settledAfter(status);
  }
  assert.equal(pi.aborts, 1, "cancellation aborts the agent; a CLI failure does not");
  assert.equal(pendingBriefingId(pi.entries), undefined, "every finished briefing is settled");
});

for (const action of ["escape", "brief-cancel", "abort", "shutdown"]) {
  const detaches = action === "shutdown";
  test(`${action} ${detaches ? "detaches from" : "cancels"} an open briefing`, async () => {
    process.env.BRIEFING_TEST_STATUS = "wait";
    const pi = createHarness();
    await pi.emit("session_start", { reason: "new" });
    await pi.command("brief-demo");
    const controller = new AbortController();
    let timedOut = false;
    const safety = setTimeout(() => { timedOut = true; controller.abort(); }, 5000);
    try {
      let ready;
      const isReady = new Promise((resolve) => { ready = resolve; });
      const execution = pi.run("briefing_demo", controller.signal, ready);
      // Attach the expectation before signalling, so the exit cannot surface as unhandled.
      const rejected = assert.rejects(execution, detaches ? /exited with 130/ : /cancelled/);
      await isReady;
      if (action === "escape") pi.pressKey("\u001b");
      else if (action === "brief-cancel") await pi.command("brief-cancel");
      else if (action === "abort") controller.abort();
      else await pi.emit("session_shutdown");
      await rejected;
    } finally {
      clearTimeout(safety);
    }
    assert.equal(timedOut, false, "the safety timeout did not do the cancelling");
    assert.deepEqual(pi.activeTools, baseTools);
    assert.equal(await pi.commandSection(), undefined);
    assert.equal(pendingBriefingId(pi.entries), detaches ? "fixture-id" : undefined,
      "a detached briefing stays pending; a cancelled one is settled");
  });
}

for (const reason of ["resume", "new", "fork"]) {
  test(`a ${reason} session ${reason === "resume" ? "reattaches to" : "ignores"} a pending briefing`, async () => {
    const pi = createHarness({ branch: [{ type: "custom", customType: "briefing-pending", data: { id: "resume-id" } }] });
    await pi.emit("session_start", { reason });
    await new Promise((resolve) => setTimeout(resolve, 10));
    const section = await pi.commandSection();
    if (reason === "resume") {
      assert.equal(pi.messages.length, 1);
      assert.match(pi.messages[0], /resume-id/);
      assert.match(section, /resume-id/);
      assert.ok(pi.activeTools.includes("briefing_result"));
    } else {
      assert.deepEqual(pi.messages, []);
      assert.equal(section, undefined);
    }
    await pi.emit("session_shutdown");
    assert.deepEqual(pi.activeTools, baseTools);
  });
}

test("session entries decide which briefing a resumed session reattaches to", () => {
  const entry = (customType, id) => ({ type: "custom", customType, data: { id } });
  assert.equal(pendingBriefingId([]), undefined);
  assert.equal(pendingBriefingId([entry("briefing-pending", "a"), { type: "message" }, entry("briefing-settled", "a")]), undefined,
    "a settled briefing is not resumed");
  assert.equal(pendingBriefingId([entry("briefing-pending", "a"), entry("briefing-pending", "b"), entry("briefing-settled", "b")]), "a",
    "the latest briefing still open is resumed");
  assert.equal(pendingBriefingId([entry("briefing-pending", "a"), entry("briefing-settled", "a"), entry("briefing-pending", "a")]), "a",
    "a reopened briefing is open again");
  assert.equal(pendingBriefingId([entry("other", "x"), entry("briefing-pending", undefined)]), undefined, "foreign entries are ignored");
});
