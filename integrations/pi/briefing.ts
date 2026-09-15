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

import { spawn, type ChildProcess } from "node:child_process";
import { createInterface } from "node:readline";

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { matchesKey, Text } from "@earendil-works/pi-tui";

const BINARY = process.env.BRIEFING_BIN || "briefing";
const DEMO_TOOL_NAME = "briefing_demo";
const RESULT_TOOL_NAME = "briefing_result";

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

type Active = { child: ChildProcess; ready?: ReadyEvent };

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

function summary(feedback: Feedback): string {
  const decisions = feedback.decisions.filter((d) => d.selected || d.note).length;
  const sections = feedback.chunks.filter((c) => c.note || c.checkpoint || c.status === "revisit").length;
  return `${decisions} decisions, ${sections} section responses, ${feedback.annotations.length} inline comments, ${feedback.notes.length} notes`;
}

export default function briefingExtension(pi: ExtensionAPI) {
  let active: Active | undefined;
  let forceBriefingNextTurn = false;

  // A command-only tool (`/brief-demo`, `/brief-result`) is enabled for exactly one turn: the
  // command records what to run, `before_agent_start` injects the prompt that steers the model
  // to it, and `agent_settled` tears the tool back down. `cliArgs` also carries the recovery
  // id, so the tool needs no model-supplied parameters.
  type Forced = { tool: string; cliArgs: string[]; prompt: string };
  let forced: Forced | undefined;

  function forceCommandTool(next: Forced) {
    forced = next;
    pi.setActiveTools([...new Set([...pi.getActiveTools(), next.tool])]);
  }

  function clearForcedTool() {
    if (!forced) return;
    const { tool } = forced;
    forced = undefined;
    pi.setActiveTools(pi.getActiveTools().filter((name) => name !== tool));
  }

  /** Run the CLI to completion; `onReady` fires once the link is known. */
  function run(args: string[], stdin: string | undefined, ctx: ExtensionContext, signal?: AbortSignal, onReady?: (ready: ReadyEvent) => void): Promise<CliResult> {
    if (active) return Promise.reject(new Error("A briefing is already open; wait for it or /brief-cancel"));
    const child = spawn(BINARY, args, { stdio: [stdin === undefined ? "ignore" : "pipe", "pipe", "pipe"] });
    if (stdin !== undefined) child.stdin!.end(stdin);
    active = { child };

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
      active!.ready = ready;
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
    }).finally(() => {
      signal?.removeEventListener("abort", onAbort);
      disposeInterrupt();
      if (active?.child === child) active = undefined;
      ctx.ui.setWorkingMessage();
      ctx.ui.setStatus("briefing", undefined);
      ctx.ui.setWidget("briefing", undefined);
    });
  }

  pi.on("session_start", async (_event, ctx) => {
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
          restoreCommandTools();
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
      parameters: {
        type: "object",
        properties: { id: { type: "string", description: "Briefing id to recover" } },
        required: ["id"],
        additionalProperties: false,
      } as any,

      async execute(_toolCallId, params, signal, onUpdate, toolCtx) {
        const input = params as { id?: unknown };
        const id = forcedResultId ?? (typeof input.id === "string" ? input.id.trim() : "");
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
          forcedResultId = undefined;
          restoreCommandTools();
        }
      },

      renderCall(args, theme) {
        const input = args as { id?: unknown };
        const id = typeof input.id === "string" && input.id ? input.id : forcedResultId ?? "briefing";
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
  });

  pi.on("before_agent_start", async (event) => {
    if (forceDemoNextTurn) {
      forceDemoNextTurn = false;
      return {
        systemPrompt: `${event.systemPrompt}\n\nThe user explicitly requested the bundled briefing demo. Call ${DEMO_TOOL_NAME} exactly once now. Do not call brief_user for this request. After the tool returns completed feedback, respond only to that feedback.`,
      };
    }

    if (forceResultNextTurn) {
      forceResultNextTurn = false;
      return {
        systemPrompt: `${event.systemPrompt}\n\nThe user explicitly requested recovery for briefing ${forcedResultId}. Call ${RESULT_TOOL_NAME} exactly once now with id ${JSON.stringify(forcedResultId)}. Do not call brief_user for this request. After the tool returns completed feedback, respond only to that feedback.`,
      };
    }

    if (!forceBriefingNextTurn) return;
    forceBriefingNextTurn = false;
    return {
      systemPrompt: `${event.systemPrompt}\n\nThe user explicitly requested a briefing for this turn. Do the necessary work, then call brief_user for the final presentation rather than emitting a long normal response.`,
    };
  });

  pi.registerCommand("brief", {
    description: "Ask Pi to answer with a browser briefing",
    handler: async (args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      if (!args.trim()) return ctx.ui.notify("Usage: /brief <request>", "warning");
      if (!ctx.isIdle()) return ctx.ui.notify("Pi is busy; wait for the current turn", "warning");
      forceBriefingNextTurn = true;
      pi.sendUserMessage(args.trim());
    },
  });

  pi.registerCommand("brief-demo", {
    description: "Open the bundled briefing demo through an agent tool call",
    handler: async (_args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      if (!ctx.isIdle()) return ctx.ui.notify("Pi is busy; wait for the current turn", "warning");
      enableCommandTool(DEMO_TOOL_NAME);
      forceDemoNextTurn = true;
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
      forcedResultId = id;
      enableCommandTool(RESULT_TOOL_NAME);
      forceResultNextTurn = true;
      pi.sendUserMessage(`Recover briefing ${id}.`);
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
    forcedResultId = undefined;
    restoreCommandTools();
  });

  pi.on("session_shutdown", async () => {
    forceBriefingNextTurn = false;
    forceDemoNextTurn = false;
    forceResultNextTurn = false;
    forcedResultId = undefined;
    restoreCommandTools();
    active?.child.kill("SIGINT");
  });
}
