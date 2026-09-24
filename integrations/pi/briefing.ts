// Pi extension: thin adapter over the `briefing` CLI.
//
// Install: `pi install git:github.com/JacobHayes/briefing` (the repo's package.json
// declares this extension). Requires the `briefing` binary on PATH.
//
// Pi has no tool timeout, so real briefings use a single blocking tool: `brief_user` spawns
// `briefing present --json`, shows the link in Pi's UI while the user works, and returns the
// feedback when they submit. Recovery/demo commands temporarily enable command-only tools so
// they exercise the same active-tool UI. Esc or /brief-cancel cancels. Briefings are mirrored to disk by
// the CLI, so `/brief-result <id>` recovers one after a crash (stored feedback, or a fresh
// link with the draft intact) and `/brief-status` lists them.
//
// Every briefing the extension opens is recorded in the Pi session (`briefing-pending`, then
// `briefing-settled` once it completes, is cancelled or fails). Ending or killing Pi while one is
// open leaves it open rather than cancelling it, and resuming that session reattaches to it
// through the same recovery path as `/brief-result`: the stored feedback if the user already
// submitted, otherwise a fresh link and a new wait.

import { spawn, type ChildProcess } from "node:child_process";
import { createInterface } from "node:readline";

import type { ExtensionAPI, ExtensionContext, SessionEntry } from "@earendil-works/pi-coding-agent";
import { matchesKey, Text } from "@earendil-works/pi-tui";

const BINARY = process.env.BRIEFING_BIN || "briefing";
const DEMO_TOOL_NAME = "briefing_demo";
const RESULT_TOOL_NAME = "briefing_result";
const PENDING_ENTRY = "briefing-pending";
const SETTLED_ENTRY = "briefing-settled";

type ReadyEvent = {
  event: "ready";
  id: string;
  url: string;
  scope: string;
  label: string;
  bindHost?: string;
  openedBrowser: boolean;
  diagnostics?: string;
};

type Feedback = {
  chunks: Array<{ title: string; status: string; checkpoint: string; note: string }>;
  decisions: Array<{ question: string; selected: string; note: string }>;
  annotations: Array<{ location: string; quote: string; comment: string; target?: Record<string, string> }>;
  notes: string[];
  overallNote: string;
};

/** What `briefing present|demo|await --json` prints on stdout. */
type CliResult = { briefingId: string } & (
  | { status: "pending" }
  | { status: "completed"; feedback: Feedback }
  | { status: "cancelled"; feedback: Feedback }
);

/** `detached` marks a child stopped because Pi is going away, which leaves its briefing open. */
type Active = { child: ChildProcess; ready?: ReadyEvent; detached?: boolean };

/** Run the CLI and return its stdout; rejects with stderr on a non-zero exit. */
function runCapture(args: string[]): Promise<string> {
  return new Promise<string>((resolve, reject) => {
    const child = spawn(BINARY, args, { stdio: ["ignore", "pipe", "pipe"] });
    let out = "";
    let err = "";
    child.stdout.on("data", (d) => (out += d));
    child.stderr.on("data", (d) => (err += d));
    child.on("error", reject);
    child.on("close", (code) => (code === 0 ? resolve(out) : reject(new Error(err || `briefing ${args[0]} exited ${code}`))));
  });
}

const describe = (error: unknown) => (error instanceof Error ? error.message : String(error));

function parseStringArray(text: string, label: string): string[] {
  const value = JSON.parse(text);
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string")) {
    throw new Error(`${label} returned an unexpected shape`);
  }
  return value;
}

/** The most recent briefing this session opened and never saw settle, if any. */
export function pendingBriefingId(entries: SessionEntry[]): string | undefined {
  const open: string[] = [];
  for (const entry of entries) {
    if (entry.type !== "custom") continue;
    const id = (entry.data as { id?: unknown } | undefined)?.id;
    if (typeof id !== "string") continue;
    const at = open.indexOf(id);
    if (at >= 0) open.splice(at, 1);
    if (entry.customType === PENDING_ENTRY) open.push(id);
  }
  return open.at(-1);
}

function summary(feedback: Feedback): string {
  const decisions = feedback.decisions.filter((d) => d.selected || d.note).length;
  const sections = feedback.chunks.filter((c) => c.note || c.checkpoint || c.status === "revisit").length;
  return `${decisions} decisions, ${sections} section responses, ${feedback.annotations.length} inline comments, ${feedback.notes.length} notes`;
}

export default function briefingExtension(pi: ExtensionAPI) {
  let active: Active | undefined;

  // `/brief`, `/brief-demo` and `/brief-result` all queue one instruction for the next turn: the
  // command records it, `before_agent_start` appends `prompt` to the system prompt, and
  // `agent_settled` tears it back down. Naming a `tool` additionally enables that command-only
  // tool for the turn and carries everything it needs (the recovery id), so the tool itself takes
  // no model-supplied parameters.
  type Forced =
    | { tool?: undefined; prompt: string }
    | { tool: typeof DEMO_TOOL_NAME; prompt: string }
    | { tool: typeof RESULT_TOOL_NAME; id: string; prompt: string };
  let forced: Forced | undefined;

  function forceNextTurn(next: Forced) {
    clearForced();
    forced = next;
    if (next.tool) pi.setActiveTools([...new Set([...pi.getActiveTools(), next.tool])]);
  }

  function clearForced() {
    const tool = forced?.tool;
    forced = undefined;
    if (tool) pi.setActiveTools(pi.getActiveTools().filter((name) => name !== tool));
  }

  function forcedResultId(): string | undefined {
    return forced?.tool === RESULT_TOOL_NAME ? forced.id : undefined;
  }

  /** Run the CLI to completion; `onReady` fires once the link is known. */
  function run(args: string[], stdin: string | undefined, ctx: ExtensionContext, signal?: AbortSignal, onReady?: (ready: ReadyEvent) => void): Promise<CliResult> {
    if (active) return Promise.reject(new Error("A briefing is already open; wait for it or /brief-cancel"));
    const child = spawn(BINARY, args, { stdio: [stdin === undefined ? "ignore" : "pipe", "pipe", "pipe"] });
    if (stdin !== undefined) child.stdin!.end(stdin);
    const record: Active = { child };
    active = record;
    // `briefing await <id>` knows its id before the ready event (and may fail without one).
    let briefingId = args[0] === "await" ? args[1] : undefined;
    const settle = (status: string) => {
      if (briefingId) pi.appendEntry(SETTLED_ENTRY, { id: briefingId, status });
    };

    ctx.ui.setWorkingMessage("Preparing briefing...");
    const disposeInterrupt = ctx.ui.onTerminalInput((data) => {
      if (matchesKey(data, "escape")) child.kill("SIGINT");
      return undefined;
    });
    const onAbort = () => child.kill("SIGINT");
    signal?.addEventListener("abort", onAbort, { once: true });

    let stdout = "";
    child.stdout!.on("data", (d) => (stdout += d));
    const stderrLines: string[] = [];
    createInterface({ input: child.stderr! }).on("line", (line) => {
      let event: any;
      try {
        event = JSON.parse(line);
      } catch {
        stderrLines.push(line);
        return;
      }
      if (event.event !== "ready") return;
      const ready = event as ReadyEvent;
      record.ready = ready;
      briefingId = ready.id;
      pi.appendEntry(PENDING_ENTRY, { id: ready.id });
      // Keep Pi UI chrome minimal: the working row already stays visible while the
      // briefing blocks, and /brief-reopen can redisplay the link if needed.
      const message = `Briefing: ${ready.url}`;
      ctx.ui.setWorkingMessage(message);
      onReady?.(ready);
    });

    return new Promise<CliResult>((resolve, reject) => {
      child.on("error", reject);
      child.on("close", (code) => {
        if (code !== 0 && code !== 2 && code !== 3) return reject(new Error(stderrLines.join("\n") || `briefing exited with ${code}`));
        try {
          resolve(JSON.parse(stdout) as CliResult);
        } catch (error) {
          reject(error);
        }
      });
    })
      .then(
        (result) => {
          if (result.status !== "pending") settle(result.status);
          return result;
        },
        (error) => {
          // A detached briefing is still open for the user; anything else is over.
          if (!record.detached) settle("failed");
          throw error;
        },
      )
      .finally(() => {
      signal?.removeEventListener("abort", onAbort);
      disposeInterrupt();
      if (active?.child === child) active = undefined;
      ctx.ui.setWorkingMessage();
      ctx.ui.setStatus("briefing", undefined);
      ctx.ui.setWidget("briefing", undefined);
    });
  }

  function recover(id: string, prompt: string, message: string) {
    forceNextTurn({
      tool: RESULT_TOOL_NAME,
      id,
      prompt: `${prompt} Call ${RESULT_TOOL_NAME} exactly once now; it takes no parameters and already has that id. Do not call brief_user for this request. After the tool returns completed feedback, respond only to that feedback.`,
    });
    pi.sendUserMessage(message);
  }

  /** Reattach to a briefing this session left open when Pi last stopped. */
  function resumePending(ctx: ExtensionContext) {
    const id = pendingBriefingId(ctx.sessionManager.getBranch());
    if (!id || active) return;
    ctx.ui.notify(`Reattaching to briefing ${id}, which was still open when this session stopped`, "info");
    // Let startup finish before starting a turn.
    setTimeout(() => {
      if (active || !ctx.isIdle()) return ctx.ui.notify(`Briefing ${id} is still open; /brief-result ${id} reattaches to it`, "info");
      recover(
        id,
        `Briefing ${JSON.stringify(id)} was still open when this Pi session stopped; the user has not seen its result in this conversation yet.`,
        `Reattach to briefing ${id}.`,
      );
    }, 0);
  }

  pi.on("session_start", async (event, ctx) => {
    if (ctx.mode !== "tui") return;
    let schema: any;
    let piGuidance: string[];
    try {
      [schema, piGuidance] = await Promise.all([
        runCapture(["schema"]).then(JSON.parse),
        runCapture(["guidance", "pi"]).then((text) => parseStringArray(text, "briefing guidance pi")),
      ]);
    } catch (error) {
      ctx.ui.notify(`briefing binary unavailable: ${describe(error)}`, "warning");
      return;
    }

    pi.registerTool({
      name: DEMO_TOOL_NAME,
      label: "Briefing Demo",
      description: "Open the bundled briefing demo and return the user's feedback. Used only by /brief-demo.",
      promptSnippet: "Open the bundled briefing demo when /brief-demo is requested",
      executionMode: "sequential",
      parameters: { type: "object", properties: {}, additionalProperties: false } as any,

      async execute(_toolCallId, _params, signal, onUpdate, toolCtx) {
        try {
          const result = await run(["demo", "--json"], undefined, toolCtx, signal, (ready) => {
            onUpdate?.({
              content: [{ type: "text", text: `Briefing: ${ready.url}` }],
              details: { status: "open", briefingId: ready.id, url: ready.url, scope: ready.scope },
            });
          });
          if (result.status !== "completed") {
            toolCtx.abort();
            throw new Error("Briefing demo cancelled by user");
          }
          const feedback = result.feedback;
          return {
            content: [{ type: "text", text: JSON.stringify({ status: "completed", briefingId: result.briefingId, feedback }) }],
            details: { status: "completed", briefingId: result.briefingId, feedback },
          };
        } finally {
          clearForced();
        }
      },

      renderCall(_args, theme) {
        return new Text(theme.fg("toolTitle", theme.bold("briefing_demo ")) + theme.fg("muted", "bundled demo"), 0, 0);
      },

      renderResult(result, { expanded, isPartial }, theme) {
        const details = result.details as { status?: string; url?: string; scope?: string; feedback?: Feedback } | undefined;
        if (isPartial && details?.url) {
          let text = theme.fg("warning", "Briefing: ") + theme.fg("accent", details.url);
          if (expanded) text += `\n${theme.fg("dim", "Esc or /brief-cancel to cancel")}`;
          return new Text(text, 0, 0);
        }
        if (isPartial) return new Text(theme.fg("warning", "Preparing briefing..."), 0, 0);
        if (!details?.feedback) return new Text(result.content[0]?.type === "text" ? result.content[0].text : "", 0, 0);
        return new Text(theme.fg("success", "✓ Briefing demo complete") + theme.fg("muted", ` - ${summary(details.feedback)}`), 0, 0);
      },
    });

    pi.registerTool({
      name: RESULT_TOOL_NAME,
      label: "Briefing Result",
      description: "Recover a briefing by id and return stored feedback or reopen it. Used only by /brief-result.",
      promptSnippet: "Recover a briefing result when /brief-result is requested",
      executionMode: "sequential",
      parameters: { type: "object", properties: {}, additionalProperties: false } as any,

      async execute(_toolCallId, _params, signal, onUpdate, toolCtx) {
        const id = forcedResultId();
        if (!id) throw new Error("Missing briefing id");
        try {
          const result = await run(["await", id, "--json"], undefined, toolCtx, signal, (ready) => {
            onUpdate?.({
              content: [{ type: "text", text: `Briefing: ${ready.url}` }],
              details: { status: "open", briefingId: ready.id, url: ready.url, scope: ready.scope },
            });
          });
          if (result.status !== "completed") {
            toolCtx.abort();
            throw new Error(`Briefing ${id} ${result.status}`);
          }
          const feedback = result.feedback;
          return {
            content: [{ type: "text", text: JSON.stringify({ status: "completed", briefingId: result.briefingId, feedback }) }],
            details: { status: "completed", briefingId: result.briefingId, feedback },
          };
        } finally {
          clearForced();
        }
      },

      renderCall(_args, theme) {
        const id = forcedResultId() ?? "briefing";
        return new Text(theme.fg("toolTitle", theme.bold("briefing_result ")) + theme.fg("muted", id), 0, 0);
      },

      renderResult(result, { expanded, isPartial }, theme) {
        const details = result.details as { status?: string; url?: string; scope?: string; feedback?: Feedback } | undefined;
        if (isPartial && details?.url) {
          let text = theme.fg("warning", "Briefing: ") + theme.fg("accent", details.url);
          if (expanded) text += `\n${theme.fg("dim", "Esc or /brief-cancel to cancel")}`;
          return new Text(text, 0, 0);
        }
        if (isPartial) return new Text(theme.fg("warning", "Recovering briefing..."), 0, 0);
        if (!details?.feedback) return new Text(result.content[0]?.type === "text" ? result.content[0].text : "", 0, 0);
        return new Text(theme.fg("success", "✓ Briefing recovered") + theme.fg("muted", ` - ${summary(details.feedback)}`), 0, 0);
      },
    });

    pi.registerTool({
      name: "brief_user",
      label: "Brief the user",
      description:
        "Present complex information in a paced browser briefing and return the user's notes, inline comments, decisions, and follow-up markers. Blocks until the user submits.",
      promptSnippet: "Present complex information or contextual decisions as a paced browser briefing",
      // Prompt guidance comes from the binary (`briefing guidance pi`) so it matches the CLI and MCP wrappers.
      promptGuidelines: piGuidance,
      executionMode: "sequential",
      parameters: schema,

      async execute(_toolCallId, params, signal, onUpdate, toolCtx) {
        const result = await run(["present", "--json"], JSON.stringify(params), toolCtx, signal, (ready) => {
          onUpdate?.({
            content: [{ type: "text", text: `Briefing: ${ready.url}` }],
            details: { status: "open", briefingId: ready.id, url: ready.url, scope: ready.scope },
          });
        });
        if (result.status !== "completed") {
          toolCtx.abort();
          throw new Error("Briefing cancelled by user");
        }
        const feedback = result.feedback;
        return {
          content: [{ type: "text", text: JSON.stringify({ status: "completed", briefingId: result.briefingId, feedback }) }],
          details: { status: "completed", briefingId: result.briefingId, feedback },
        };
      },

      renderCall(args, theme) {
        const input = args as { title?: unknown; chunks?: unknown };
        const title = typeof input.title === "string" ? input.title : "briefing";
        const count = Array.isArray(input.chunks) ? input.chunks.length : 0;
        return new Text(theme.fg("toolTitle", theme.bold("brief_user ")) + theme.fg("muted", `${title} (${count} chunks)`), 0, 0);
      },

      renderResult(result, { expanded, isPartial }, theme) {
        const details = result.details as { status?: string; url?: string; scope?: string; feedback?: Feedback } | undefined;
        if (isPartial && details?.url) {
          let text = theme.fg("warning", "Briefing: ") + theme.fg("accent", details.url);
          if (expanded) text += `\n${theme.fg("dim", "Esc or /brief-cancel to cancel")}`;
          return new Text(text, 0, 0);
        }
        if (isPartial) return new Text(theme.fg("warning", "Preparing briefing..."), 0, 0);
        if (!details?.feedback) return new Text(result.content[0]?.type === "text" ? result.content[0].text : "", 0, 0);
        return new Text(theme.fg("success", "✓ Briefing complete") + theme.fg("muted", ` - ${summary(details.feedback)}`), 0, 0);
      },
    });

    pi.setActiveTools(pi.getActiveTools().filter((name) => name !== DEMO_TOOL_NAME && name !== RESULT_TOOL_NAME));
    // A fork copies the branch, so reattaching there would wait on the same briefing twice.
    if (event.reason !== "new" && event.reason !== "fork") resumePending(ctx);
  });

  pi.on("before_agent_start", async (event) => {
    if (!forced) return;
    return { systemPrompt: `${event.systemPrompt}\n\n${forced.prompt}` };
  });

  pi.registerCommand("brief", {
    description: "Ask Pi to answer with a browser briefing",
    handler: async (args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      if (!args.trim()) return ctx.ui.notify("Usage: /brief <request>", "warning");
      if (!ctx.isIdle()) return ctx.ui.notify("Pi is busy; wait for the current turn", "warning");
      forceNextTurn({
        prompt:
          "The user explicitly requested a briefing for this turn. Do the necessary work, then call brief_user for the final presentation rather than emitting a long normal response.",
      });
      pi.sendUserMessage(args.trim());
    },
  });

  pi.registerCommand("brief-demo", {
    description: "Open the bundled briefing demo through an agent tool call",
    handler: async (_args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      if (!ctx.isIdle()) return ctx.ui.notify("Pi is busy; wait for the current turn", "warning");
      forceNextTurn({
        tool: DEMO_TOOL_NAME,
        prompt: `The user explicitly requested the bundled briefing demo. Call ${DEMO_TOOL_NAME} exactly once now. Do not call brief_user for this request. After the tool returns completed feedback, respond only to that feedback.`,
      });
      pi.sendUserMessage("Open the bundled briefing demo.");
    },
  });

  pi.registerCommand("brief-result", {
    description: "Recover a briefing by id: fetch its stored feedback, or reopen it with a fresh link",
    handler: async (args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      if (!ctx.isIdle()) return ctx.ui.notify("Pi is busy; wait for the current turn", "warning");
      const id = args.trim();
      if (!id) return ctx.ui.notify("Usage: /brief-result <briefingId>", "warning");
      recover(id, `The user explicitly requested recovery for briefing ${JSON.stringify(id)}.`, `Recover briefing ${id}.`);
    },
  });

  pi.registerCommand("brief-status", {
    description: "List known briefings (waiting, completed, cancelled)",
    handler: async (_args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      const text = await runCapture(["status"]).catch((error) => `error: ${describe(error)}`);
      ctx.ui.notify(text.trim() || "no briefings", "info");
    },
  });

  pi.registerCommand("brief-reopen", {
    description: "Show the open briefing's link again",
    handler: async (_args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      if (!active?.ready?.url) return ctx.ui.notify("No briefing is open", "warning");
      ctx.ui.notify(`Briefing: ${active.ready.url}`, "info");
    },
  });

  pi.registerCommand("brief-cancel", {
    description: "Cancel the open briefing",
    handler: async (_args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      if (!active) return ctx.ui.notify("No briefing is open", "warning");
      active.child.kill("SIGINT");
      ctx.ui.notify("Briefing cancelled", "info");
    },
  });

  pi.on("agent_settled", async () => {
    clearForced();
  });

  // Pi is going away (quit, /new, /resume, /fork, reload), not the user cancelling: stop waiting
  // but leave the briefing open. SIGHUP ends the CLI without the SIGINT/SIGTERM cancel, and the
  // session's pending entry lets a resume reattach.
  pi.on("session_shutdown", async () => {
    clearForced();
    if (!active) return;
    active.detached = true;
    active.child.kill("SIGHUP");
  });
}
