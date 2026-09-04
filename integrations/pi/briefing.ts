// Pi extension: thin adapter over the `briefing` CLI.
//
// Install: `pi install git:github.com/JacobHayes/briefing` (the repo's package.json
// declares this extension and the skill). Requires the `briefing` binary on PATH.
//
// Pi has no tool timeout, so this is a single blocking tool: `brief_user` spawns
// `briefing present --json`, shows the link in Pi's UI while the user works, and returns the
// feedback when they submit. Esc or /brief-cancel cancels. Briefings are mirrored to disk by
// the CLI, so `/brief-result <id>` recovers one after a crash (stored feedback, or a fresh
// link with the draft intact) and `/brief-status` lists them.

import { spawn, type ChildProcess } from "node:child_process";
import { createInterface } from "node:readline";

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { matchesKey, Text } from "@earendil-works/pi-tui";

const BINARY = process.env.BRIEFING_BIN || "briefing";

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
  cancelled: boolean;
  chunks: Array<{ title: string; status: string; checkpoint: string; note: string }>;
  decisions: Array<{ question: string; selected: string; note: string }>;
  annotations: Array<{ location: string; quote: string; comment: string; target?: Record<string, string> }>;
  overallNote: string;
};

type CliResult = { status: "completed" | "cancelled" | "pending"; briefingId: string; result?: Feedback };

type Active = { child: ChildProcess; url?: string };

async function loadSchema(): Promise<any> {
  const text = await new Promise<string>((resolve, reject) => {
    const child = spawn(BINARY, ["schema"], { stdio: ["ignore", "pipe", "pipe"] });
    let out = "";
    let err = "";
    child.stdout.on("data", (d) => (out += d));
    child.stderr.on("data", (d) => (err += d));
    child.on("error", reject);
    child.on("close", (code) => (code === 0 ? resolve(out) : reject(new Error(err || `briefing schema exited ${code}`))));
  });
  return JSON.parse(text);
}

function summary(feedback: Feedback): string {
  const decisions = feedback.decisions.filter((d) => d.selected || d.note).length;
  const sections = feedback.chunks.filter((c) => c.note || c.checkpoint || c.status === "revisit").length;
  return `${decisions} decisions, ${sections} section responses, ${feedback.annotations.length} inline comments`;
}

export default function briefingExtension(pi: ExtensionAPI) {
  let active: Active | undefined;
  let forceBriefingNextTurn = false;

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
      active!.url = ready.url;
      const message = `Briefing (${ready.scope}): ${ready.url}`;
      ctx.ui.setWorkingMessage(message);
      ctx.ui.setStatus("briefing", `briefing: ${ready.scope}`);
      const widget = [message, `Listening on ${ready.label}`];
      if (!ready.openedBrowser) widget.push("Browser not opened automatically; open the link manually");
      if (ready.diagnostics) widget.push(ready.diagnostics);
      ctx.ui.setWidget("briefing", widget, { placement: "belowEditor" });
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
    try {
      schema = await loadSchema();
    } catch (error) {
      ctx.ui.notify(`briefing binary unavailable: ${error instanceof Error ? error.message : String(error)}`, "warning");
      return;
    }

    pi.registerTool({
      name: "brief_user",
      label: "Brief the user",
      description:
        "Present complex information in a paced browser briefing and return the user's notes, inline comments, decisions, and follow-up markers. Blocks until the user submits.",
      promptSnippet: "Present complex information or contextual decisions as a paced browser briefing",
      promptGuidelines: [
        "Proactively use brief_user whenever an answer crosses a complexity threshold (substantial research, multi-part explanations, decisions that need context). Use normal concise responses for simple answers.",
        "Finish the research first, then call brief_user once with 3-8 semantic chunks in dependency order, 3-5 keyPoints each, stable context in tray, and 2-4 distinct decision options with the recommended one first and marked.",
        "Every prose field accepts Markdown: GFM tables, fenced code with a language tag, ```mermaid fences for flows/architecture/state, and ```vega-lite fences for charts; use them only when they clarify.",
        "brief_user shows the link in Pi's UI and blocks until the user submits; respond only to the returned feedback and do not repeat the presentation.",
        "Briefings survive restarts for a few hours; if the user gives you a briefing id from an interrupted session, tell them to run /brief-result <id> to recover it.",
      ],
      executionMode: "sequential",
      parameters: schema,

      async execute(_toolCallId, params, signal, onUpdate, toolCtx) {
        const result = await run(["present", "--json"], JSON.stringify(params), toolCtx, signal, (ready) => {
          onUpdate?.({
            content: [{ type: "text", text: `Briefing open at ${ready.url}` }],
            details: { status: "open", briefingId: ready.id, url: ready.url, scope: ready.scope },
          });
        });
        if (result.status !== "completed" || !result.result || result.result.cancelled) {
          toolCtx.abort();
          throw new Error("Briefing cancelled by user");
        }
        const feedback = result.result;
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
          let text = theme.fg("warning", "Briefing open: ") + theme.fg("accent", details.url) + theme.fg("muted", ` (${details.scope})`);
          if (expanded) text += `\n${theme.fg("dim", "Esc or /brief-cancel to cancel")}`;
          return new Text(text, 0, 0);
        }
        if (isPartial) return new Text(theme.fg("warning", "Preparing briefing..."), 0, 0);
        if (!details?.feedback) return new Text(result.content[0]?.type === "text" ? result.content[0].text : "", 0, 0);
        return new Text(theme.fg("success", "✓ Briefing complete") + theme.fg("muted", ` - ${summary(details.feedback)}`), 0, 0);
      },
    });
  });

  pi.on("before_agent_start", async (event) => {
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
    description: "Open the bundled briefing demo; feedback is sent to Pi as a message",
    handler: async (_args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      try {
        const result = await run(["demo", "--json"], undefined, ctx);
        if (result.status === "completed" && result.result) {
          pi.sendUserMessage(`I just reviewed the briefing demo. Here is my structured feedback from it:\n\n${JSON.stringify(result.result, null, 2)}`);
        } else {
          ctx.ui.notify(`Briefing demo ${result.status}`, "info");
        }
      } catch (error) {
        ctx.ui.notify(error instanceof Error ? error.message : String(error), "error");
      }
    },
  });

  pi.registerCommand("brief-result", {
    description: "Recover a briefing by id: fetch its stored feedback, or reopen it with a fresh link",
    handler: async (args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      const id = args.trim();
      if (!id) return ctx.ui.notify("Usage: /brief-result <briefingId>", "warning");
      try {
        const result = await run(["await", id, "--json"], undefined, ctx);
        if (result.status === "completed" && result.result && !result.result.cancelled) {
          pi.sendUserMessage(`Here is my feedback from briefing ${id} (recovered after the earlier session was interrupted); respond to it:\n\n${JSON.stringify(result.result, null, 2)}`);
        } else {
          ctx.ui.notify(`Briefing ${id} ${result.status}`, "info");
        }
      } catch (error) {
        ctx.ui.notify(error instanceof Error ? error.message : String(error), "error");
      }
    },
  });

  pi.registerCommand("brief-status", {
    description: "List known briefings (waiting, completed, cancelled)",
    handler: async (_args, ctx) => {
      const text = await new Promise<string>((resolve, reject) => {
        const child = spawn(BINARY, ["status"], { stdio: ["ignore", "pipe", "pipe"] });
        let out = "";
        let err = "";
        child.stdout.on("data", (d) => (out += d));
        child.stderr.on("data", (d) => (err += d));
        child.on("error", reject);
        child.on("close", (code) => (code === 0 ? resolve(out) : reject(new Error(err || `briefing status exited ${code}`))));
      }).catch((error) => `error: ${error instanceof Error ? error.message : String(error)}`);
      ctx.ui.notify(text.trim() || "no briefings", "info");
    },
  });

  pi.registerCommand("brief-reopen", {
    description: "Show the open briefing's link again",
    handler: async (_args, ctx) => {
      if (!active?.url) return ctx.ui.notify("No briefing is open", "warning");
      ctx.ui.notify(`Briefing: ${active.url}`, "info");
    },
  });

  pi.registerCommand("brief-cancel", {
    description: "Cancel the open briefing",
    handler: async (_args, ctx) => {
      if (!active) return ctx.ui.notify("No briefing is open", "warning");
      active.child.kill("SIGINT");
      ctx.ui.notify("Briefing cancelled", "info");
    },
  });

  pi.on("session_shutdown", async () => {
    forceBriefingNextTurn = false;
    active?.child.kill("SIGINT");
  });
}
