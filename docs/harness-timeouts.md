# How MCP clients treat long "wait for a human" tool calls

Research snapshot (2026-09-03) that informs `--hold` and `--max-wait-secs`. Items marked
*unverified* had no primary source. Sources are linked inline.

## Protocol regimes

- **2025-11-25 and earlier** (what shipping clients speak): one `tools/call` request; the only
  in-band liveness signal is `notifications/progress`; elicitation is a server-initiated
  `elicitation/create` (form or url); Tasks were experimental core. Spec: implementations
  "MAY choose to reset the timeout clock when receiving a progress notification... SHOULD always
  enforce a maximum timeout" ([lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle#timeouts)).
- **2026-07-28** (rolling out): stateless core, no server-initiated requests. Elicitation becomes
  MRTR: the server returns `resultType: "input_required"` plus an opaque `requestState`; the
  client re-issues `tools/call` with `inputResponses`, and may be prompted repeatedly
  ([MRTR](https://modelcontextprotocol.io/specification/draft/basic/patterns/mrtr),
  [elicitation](https://modelcontextprotocol.io/specification/draft/client/elicitation)).
  Tasks moved to the `io.modelcontextprotocol/tasks` extension
  ([SEP-2663](https://modelcontextprotocol.io/seps/2663-tasks-extension)), whose stated use
  cases include "approval gates, review steps, or any operation that pauses for user confirmation".

## What existing "ask the user" servers do

| Server | Mechanism | Wait limit | Client workaround |
|---|---|---|---|
| [interactive-feedback-mcp](https://github.com/noopstudios/interactive-feedback-mcp) and forks | Plain blocking call | none | `"timeout": 600` in Cursor config |
| [mcp-feedback-enhanced](https://github.com/Minidoracat/mcp-feedback-enhanced) | Blocking; clamps `timeout` arg to 60-86400 s | 600 s default | `"timeout": 600`, `autoApprove` |
| [user-feedback-mcp](https://github.com/mrexodia/user-feedback-mcp), [user-prompt-mcp](https://github.com/nazar256/user-prompt-mcp), [interactive-mcp](https://github.com/ttommyth/interactive-mcp), [Human-In-the-Loop-MCP-Server](https://github.com/GongRzhe/Human-In-the-Loop-MCP-Server) | Blocking | 30 s to 20 min | client `timeout` |
| [telegram-mcp-server](https://github.com/batianVolyc/telegram-mcp-server) | Blocking for hours | none | `tool_timeout_sec = 3600` for Codex |
| [Temporal HITL tutorial](https://learn.temporal.io/tutorials/ai/building-mcp-tools-with-temporal/adding-hitl-to-mcp-tools/), [mcp-agent async_tool](https://docs.mcp-agent.com/cloud/mcp-agent-cloud/long-running-tools), [AWS handleId](https://dev.to/aws/fix-mcp-timeouts-async-handleid-pattern-8ek) | Return an id immediately; separate status tool | days | none needed |
| [AAIF MRTR demo](https://aaif.io/blog/non-blocking-human-in-the-loop-agents-re-engineering-agentic-runloops-and-state-machines-with-mr) | `input_required` + signed continuation token | token TTL | none needed |
| [fastmcp (TS)](https://github.com/punkpeye/fastmcp) | `streamKeepalive`, `reportProgress`, `ping` | configurable | addresses transport idle, not client timeouts |

Every popular feedback server simply blocks and pushes the problem to the user's client
config. The robust designs are return-immediately + poll, or MRTR continuation tokens.

## Client timeout semantics

| Client | Default | Config | Progress resets? | Elicitation | Tasks |
|---|---|---|---|---|---|
| Claude Code | wall-clock `MCP_TOOL_TIMEOUT` (~28 h) plus idle 5 min (HTTP) / 30 min (stdio); calls auto-background after 2 min (v2.1.212) | `MCP_TOOL_TIMEOUT`, per-server `timeout` ms, `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`, `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` | idle timer yes, wall-clock no | form + url (2.1.76+); a call waiting on an elicitation is not backgrounded; remote-HTTP form elicitation bug [#85442](https://github.com/anthropics/claude-code/issues/85442) | not documented |
| Codex CLI | 300 s wall-clock ([PR #28234](https://github.com/openai/codex/pull/28234); docs still say 60) | `tool_timeout_sec` | no; timer pauses during an outstanding elicitation ([PR #17566](https://github.com/openai/codex/pull/17566)) | form (+ url behind a feature flag); declined by policy in unattended modes | none |
| Cursor | ~60 s, not configurable | none | no ([staff](https://forum.cursor.com/t/acp-mcp-client-should-reset-json-rpc-tool-call-timeout-on-notifications-progress-or-honour-resettimeoutonprogress/160548)) | form (url *unverified*) | no |
| VS Code Copilot | none ([#14130](https://github.com/microsoft/vscode-copilot-release/issues/14130)) | none | n/a | form + url | 2025-11-25 tasks shape |
| Gemini CLI | 600 s | `timeout` ms | no | none ([#22249](https://github.com/google-gemini/gemini-cli/issues/22249)) | no |
| Cline | 60 s, max 3600 | `timeout` seconds | no | none | no |
| Zed | 60 s | `context_server_timeout`, per-server `timeout` seconds | no | none | no |
| Continue | SDK 60 s | `connectionTimeout` ms | no | *unverified* | no |
| Goose | 300 s | extension `timeout` seconds | *unverified* | form + url; elicitation itself times out at 5 min | no |
| OpenCode | SDK 60 s (v2 docs claim 12 h, *unverified*) | per-server `timeout` ms | no | none | no |
| Pi (via pi-mcp-adapter) | SDK 60 s | `requestTimeoutMs` | no | form | no |
| Windsurf, Aider | *unverified* | | | | |

Server-cancelling an outstanding elicitation (`notifications/cancelled` for our own
`elicitation/create`) is spec-legal, but no client documents what happens to the dialog.
Under 2026-07-28 the question disappears: there is no outstanding request.

## Implications for briefing

The design matches the most portable pattern: a bounded blocking wait with progress
heartbeats, a `briefingId` resume handle (`await_briefing`), and elicitation as an opt-in hold
rather than the wait itself. What the data adds:

- `--max-wait-secs` matters more than `--hold` on the 60-second clients (Cursor, Cline, Zed,
  Continue, OpenCode, Pi's MCP adapter): none reset on progress, several cannot raise the
  limit. Budgets in `ClientProfile`: ~50 s there, ~280 s Codex/Goose, ~570 s Gemini, 24 h
  Claude Code and VS Code.
- Claude Code (verified 2026-09-04): no effective timeout for our purposes. The ~28 h
  wall-clock cap is far away and the 30-minute stdio idle timer is reset by the 10 s
  heartbeats. It moves any call over 2 minutes into a background task and wakes the model with
  a completion notice, so the tool text tells the model to wait for that rather than poll.
  Progress `message` text is not rendered (issue #31893), which is why `brief_user` returns
  the link immediately instead of reporting it mid-call.
- Because records are mirrored to disk, a lost tool call is never fatal: `await_briefing`
  with the same id from any later process returns the stored feedback or re-serves the page.
- Tasks is the sanctioned long-run pattern but only VS Code implements it (old shape), so it is
  not worth targeting yet. MRTR `input_required` + `requestState` is the natural next step once
  clients speak 2026-07-28; the existing `briefingId` can become that state.
