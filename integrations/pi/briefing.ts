// Pi extension: thin adapter over the `briefing` CLI.
//
// Install: `pi install git:github.com/JacobHayes/briefing` (the repo's package.json
// declares this extension). Requires the `briefing` binary on PATH.
//
// Pi has no tool timeout, so real briefings use a single blocking tool: `brief_user` creates
// the briefing with `briefing present --json`, then runs `briefing await --json`, showing the
// link in Pi's UI while the user works and returning the feedback when they submit.
// Recovery/demo commands temporarily enable command-only tools so they exercise the same
// active-tool UI. Esc or /brief-cancel cancels the briefing (`briefing cancel`). Briefings live
// in a hub, not in Pi, so `/brief-result <id>` recovers one after a crash (stored feedback, or
// the same link with the draft intact) and `/brief-status` lists this session's.
//
// Every briefing the extension opens is recorded in the Pi session (`briefing-pending`, then
// `briefing-settled` once it completes, is cancelled or fails). Ending or killing Pi while one is
// open only stops the wait, and resuming that session reattaches to it through the same
// recovery path as `/brief-result`: the stored feedback if the user already submitted,
// otherwise the same link and a new wait.

import { spawn, type ChildProcess } from "node:child_process";
import { createInterface } from "node:readline";

import { VERSION as PI_VERSION, type ExtensionAPI, type ExtensionContext, type SessionEntry } from "@earendil-works/pi-coding-agent";
import { matchesKey, Text } from "@earendil-works/pi-tui";

const BINARY = process.env.BRIEFING_BIN || "briefing";
const DEMO_TOOL_NAME = "briefing_demo";
const RESULT_TOOL_NAME = "briefing_result";
const COMMAND_PROMPT_SECTION = "briefing_command";
// Tool `exposure` arrived in Pi 0.99.1. Pi installs packages without resolving peer ranges, and an
// older Pi would silently ignore `exposure: "model-only"`, so refuse to load there instead.
const MIN_PI_VERSION = "0.99.1";
const PENDING_ENTRY = "briefing-pending";
const SETTLED_ENTRY = "briefing-settled";

/** What `briefing present|demo --json` prints on stdout. */
type Created = { briefingId: string; status: "active"; url: string };

/** `briefing await --json`'s stderr event while the briefing is open. */
type ReadyEvent = { event: "ready"; briefingId: string; url: string };

type Feedback = {
  questions: Array<{ question: string; section?: string; selected: string[]; answer: string; status: "answered" | "unresolved" }>;
  annotations: Array<{ location: string; quote: string; comment: string; target?: Record<string, string> }>;
  notes: string[];
};

/** What `briefing await --json` prints on stdout. */
type CliResult = { briefingId: string } & (
  | { status: "pending" }
  | { status: "completed"; feedback: Feedback }
  | { status: "cancelled"; feedback: Feedback }
);

/** The running `await`. `detached` marks one stopped because Pi is going away, which leaves its
 * briefing open. */
type Active = { child: ChildProcess; id: string; ready?: ReadyEvent; detached?: boolean };

/** A briefing being created, before its id is known: what to do with it once it is. */
type Creating = { cancelled: boolean; shutdown: boolean };

/** Run the CLI and return its stdout; rejects with stderr on a non-zero exit. */
function runCapture(args: string[], options: { stdin?: string; env?: NodeJS.ProcessEnv } = {}): Promise<string> {
  const { stdin, env } = options;
  return new Promise<string>((resolve, reject) => {
    const child = spawn(BINARY, args, { stdio: [stdin === undefined ? "ignore" : "pipe", "pipe", "pipe"], env });
    if (stdin !== undefined) child.stdin!.end(stdin);
    let out = "";
    let err = "";
    child.stdout!.on("data", (d) => (out += d));
    child.stderr!.on("data", (d) => (err += d));
    child.on("error", reject);
    child.on("close", (code) => (code === 0 ? resolve(out) : reject(new Error(err || `briefing ${args[0]} exited ${code}`))));
  });
}

const describe = (error: unknown) => (error instanceof Error ? error.message : String(error));

/** The CLI's environment in this Pi session, so its briefings are tagged with Pi and the
 * session, unless the user set `BRIEFING_SESSION` themselves. */
function sessionEnv(ctx: ExtensionContext): NodeJS.ProcessEnv {
  if (process.env.BRIEFING_SESSION) return process.env;
  return { ...process.env, BRIEFING_SESSION: ctx.sessionManager.getSessionId(), BRIEFING_HARNESS: "pi" };
}

/** Cancel the briefing itself; an `await` on it then returns `cancelled`. */
function cancelBriefing(id: string): Promise<void> {
  return runCapture(["cancel", id]).then(
    () => {},
    () => {},
  );
}

function parseStringArray(text: string, label: string): string[] {
  const value = JSON.parse(text);
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string")) {
    throw new Error(`${label} returned an unexpected shape`);
  }
  return value;
}

/** Whether `version` is at least `MIN_PI_VERSION`, comparing major.minor.patch numerically. */
export function supportsPi(version: string): boolean {
  const parse = (v: string) => (v.match(/^(\d+)\.(\d+)\.(\d+)/) ?? []).slice(1).map(Number);
  const [have, need] = [parse(version), parse(MIN_PI_VERSION)];
  if (have.length !== 3) return false;
  const at = have.findIndex((part, i) => part !== need[i]);
  return at < 0 || have[at] > need[at];
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
  const answered = feedback.questions.filter((q) => q.status === "answered").length;
  const unresolved = feedback.questions.length - answered;
  return `${answered} answered questions, ${unresolved} unresolved, ${feedback.annotations.length} inline comments, ${feedback.notes.length} notes`;
}

export default function briefingExtension(pi: ExtensionAPI) {
  if (!supportsPi(PI_VERSION)) {
    throw new Error(`briefing needs Pi >=${MIN_PI_VERSION} for model-only tools; this is Pi ${PI_VERSION}`);
  }
  let active: Active | undefined;
  let creating: Creating | undefined;

  // `/brief`, `/brief-demo` and `/brief-result` all queue one instruction for the next turn: the
  // command records it, `before_agent_start` sets the named command prompt section, and
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

  /** Create a briefing (`present`/`demo` args), then wait for its feedback. */
  async function presentAndAwait(args: string[], stdin: string | undefined, ctx: ExtensionContext, signal?: AbortSignal, onReady?: (ready: ReadyEvent) => void): Promise<CliResult> {
    if (active || creating) throw new Error("A briefing is already open; wait for it or /brief-cancel");
    // Esc, aborts, /brief-cancel and shutdown during creation are remembered and applied once
    // the id is known.
    const state: Creating = { cancelled: false, shutdown: false };
    creating = state;
    ctx.ui.setWorkingMessage("Preparing briefing...");
    const disposeInterrupt = onInterrupt(ctx, signal, () => (state.cancelled = true));
    let created: Created;
    try {
      created = JSON.parse(await runCapture(args, { stdin, env: sessionEnv(ctx) })) as Created;
    } catch (error) {
      ctx.ui.setWorkingMessage();
      throw error;
    } finally {
      if (creating === state) creating = undefined;
      disposeInterrupt();
    }
    const id = created.briefingId;
    if (state.shutdown) {
      // Pi is going away: leave it open for a resumed session to reattach to.
      pi.appendEntry(PENDING_ENTRY, { id });
      throw new Error(`Pi is shutting down; recover briefing ${id} later with /brief-result ${id}`);
    }
    // Cancelled before the wait began: cancel it first, so `await` returns `cancelled` at once.
    const cancelledEarly = state.cancelled || signal?.aborted;
    if (cancelledEarly) await cancelBriefing(id);
    return awaitBriefing(id, ctx, cancelledEarly ? undefined : signal, onReady);
  }

  /** Call `interrupt` on Esc or once `signal` aborts (at once if it already has); returns the
   * teardown. */
  function onInterrupt(ctx: ExtensionContext, signal: AbortSignal | undefined, interrupt: () => void): () => void {
    const disposeInput = ctx.ui.onTerminalInput((data) => {
      if (matchesKey(data, "escape")) interrupt();
      return undefined;
    });
    signal?.addEventListener("abort", interrupt, { once: true });
    if (signal?.aborted) interrupt();
    return () => {
      signal?.removeEventListener("abort", interrupt);
      disposeInput();
    };
  }

  /** Wait for a briefing's feedback; `onReady` fires once the link is known. Esc and aborts
   * cancel the briefing, so `await` returns `cancelled` rather than being killed. */
  function awaitBriefing(id: string, ctx: ExtensionContext, signal?: AbortSignal, onReady?: (ready: ReadyEvent) => void): Promise<CliResult> {
    if (active) return Promise.reject(new Error("A briefing is already open; wait for it or /brief-cancel"));
    const child = spawn(BINARY, ["await", id, "--json"], { stdio: ["ignore", "pipe", "pipe"], env: sessionEnv(ctx) });
    const record: Active = { child, id };
    active = record;
    pi.appendEntry(PENDING_ENTRY, { id });
    const settle = (status: string) => pi.appendEntry(SETTLED_ENTRY, { id, status });

    ctx.ui.setWorkingMessage("Preparing briefing...");
    const disposeInterrupt = onInterrupt(ctx, signal, () => void cancelBriefing(id));

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
      // Keep Pi UI chrome minimal: the working row already stays visible while the
      // briefing blocks, and /brief-reopen can redisplay the link if needed.
      ctx.ui.setWorkingMessage(`Briefing: ${ready.url}`);
      onReady?.(ready);
    });

    return new Promise<CliResult>((resolve, reject) => {
      child.on("error", reject);
      child.on("close", (code) => {
        if (code !== 0) return reject(new Error(stderrLines.join("\n") || `briefing exited with ${code}`));
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
        disposeInterrupt();
        if (active === record) active = undefined;
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
      exposure: "model-only",
      label: "Briefing Demo",
      description: "Open the bundled briefing demo and return the user's feedback. Used only by /brief-demo.",
      promptSnippet: "Open the bundled briefing demo when /brief-demo is requested",
      executionMode: "sequential",
      parameters: { type: "object", properties: {}, additionalProperties: false } as any,

      async execute(_toolCallId, _params, signal, onUpdate, toolCtx) {
        try {
          const result = await presentAndAwait(["demo", "--json"], undefined, toolCtx, signal, (ready) => {
            onUpdate?.({
              content: [{ type: "text", text: `Briefing: ${ready.url}` }],
              details: { status: "open", briefingId: ready.briefingId, url: ready.url },
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
        const details = result.details as { status?: string; url?: string; feedback?: Feedback } | undefined;
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
      exposure: "model-only",
      label: "Briefing Result",
      description: "Recover a briefing by id and return its stored feedback or wait for it. Used only by /brief-result.",
      promptSnippet: "Recover a briefing result when /brief-result is requested",
      executionMode: "sequential",
      parameters: { type: "object", properties: {}, additionalProperties: false } as any,

      async execute(_toolCallId, _params, signal, onUpdate, toolCtx) {
        const id = forcedResultId();
        if (!id) throw new Error("Missing briefing id");
        try {
          const result = await awaitBriefing(id, toolCtx, signal, (ready) => {
            onUpdate?.({
              content: [{ type: "text", text: `Briefing: ${ready.url}` }],
              details: { status: "open", briefingId: ready.briefingId, url: ready.url },
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
        const details = result.details as { status?: string; url?: string; feedback?: Feedback } | undefined;
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
      exposure: "model-only",
      label: "Brief the user",
      description:
        "Present complex information in a paced browser briefing and return the user's notes, inline comments, and question answers. Blocks until the user submits.",
      promptSnippet: "Present complex information or contextual decisions as a paced browser briefing",
      // Prompt guidance comes from the binary (`briefing guidance pi`) so it matches the CLI and MCP wrappers.
      promptGuidelines: piGuidance,
      executionMode: "sequential",
      parameters: schema,

      async execute(_toolCallId, params, signal, onUpdate, toolCtx) {
        const result = await presentAndAwait(["present", "--json"], JSON.stringify(params), toolCtx, signal, (ready) => {
          onUpdate?.({
            content: [{ type: "text", text: `Briefing: ${ready.url}` }],
            details: { status: "open", briefingId: ready.briefingId, url: ready.url },
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
        const details = result.details as { status?: string; url?: string; feedback?: Feedback } | undefined;
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
    if (forced) event.systemPromptOptions.sections[COMMAND_PROMPT_SECTION] = forced.prompt;
    else delete event.systemPromptOptions.sections[COMMAND_PROMPT_SECTION];
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
    description: "Recover a briefing by id: fetch its stored feedback, or wait on its link again",
    handler: async (args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      if (!ctx.isIdle()) return ctx.ui.notify("Pi is busy; wait for the current turn", "warning");
      const id = args.trim();
      if (!id) return ctx.ui.notify("Usage: /brief-result <briefingId>", "warning");
      recover(id, `The user explicitly requested recovery for briefing ${JSON.stringify(id)}.`, `Recover briefing ${id}.`);
    },
  });

  pi.registerCommand("brief-status", {
    description: "List this session's briefings (waiting, completed, cancelled)",
    handler: async (_args, ctx) => {
      if (ctx.mode !== "tui") return ctx.ui.notify("Briefings require Pi's interactive TUI", "error");
      const text = await runCapture(["status"], { env: sessionEnv(ctx) }).catch((error) => `error: ${describe(error)}`);
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
      if (creating) creating.cancelled = true;
      else if (active) cancelBriefing(active.id);
      else return ctx.ui.notify("No briefing is open", "warning");
      ctx.ui.notify("Briefing cancelled", "info");
    },
  });

  pi.on("agent_settled", async () => {
    clearForced();
  });

  // Pi is going away (quit, /new, /resume, /fork, reload), not the user cancelling: stop waiting
  // but leave the briefing open in the hub. The session's pending entry lets a resume reattach.
  pi.on("session_shutdown", async () => {
    clearForced();
    if (creating) creating.shutdown = true;
    if (!active) return;
    active.detached = true;
    active.child.kill("SIGHUP");
  });
}
