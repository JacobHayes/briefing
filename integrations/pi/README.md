# Pi integration intent

This is a thin, interactive-TUI adapter over the `briefing` CLI, using Pi's
structured prompt and tool-exposure APIs (introduced in Pi 0.99.1; checked against Pi 1.0.4).
Install with `pi install git:github.com/JacobHayes/briefing`; the `briefing` binary
must be on `PATH`, or selected by `BRIEFING_BIN`.

## Interaction and exposure

- `brief_user` is the normal blocking tool: it opens a browser briefing, shows the
  link in Pi's working row, and returns only submitted feedback.
- `brief_user`, `briefing_demo`, and `briefing_result` have `model-only` exposure.
  They can be declared to the model but are never callable through
  `ctx.executeTool()`, including codemode scripts. Opening or recovering a user
  interaction must be a direct model tool call, not nested programmatic work.
- `briefing_demo` and `briefing_result` are registered but inactive normally.
  `/brief-demo` and `/brief-result <id>` temporarily activate the corresponding
  tool, with no model-supplied parameters. Recovery keeps its id in command state.
- `/brief <request>` asks the model to do the work and use `brief_user` for its
  final presentation; it does not activate another tool.

## Command prompt lifecycle

Commands queue one instruction. `before_agent_start` writes it to
`systemPromptOptions.sections.briefing_command`, preserving the base prompt and
other extensions' sections rather than replacing the entire system prompt.
Forcing a second command replaces the first instruction and temporary tool.

Demo and recovery tools clear forcing and temporary activation in `finally`;
`agent_settled` and `session_shutdown` also clear them. At the following ordinary
turn, the handler removes the section. Pi appends a named section-removal patch
(`briefing_command: null`) to the transcript, so old command guidance remains
historical rather than active. The current run's already-recorded section is not
rewritten mid-run. A fresh extension runtime also removes a previously recorded
section on its next ordinary turn.

## Cancellation and recovery

Esc, `/brief-cancel`, and the operation abort signal interrupt the CLI with
`SIGINT`, cancelling the briefing and aborting the agent when the CLI reports
cancellation. `/brief-reopen` redisplays the current link; `/brief-status` lists
known briefings.

Session shutdown sends `SIGHUP` instead: it stops waiting without cancelling the
browser briefing. CLI ready events record `briefing-pending` entries; completion,
cancellation, or non-detached failure records `briefing-settled`. Resuming a
session reattaches to its latest pending briefing through the same recovery tool
as `/brief-result`: stored feedback if submitted, or a new link with the draft
intact. New and forked sessions do not automatically reattach.

## Boundaries and verification

Keep CLI content validation, persistence, browser presentation, and feedback
semantics in the Rust binary. This adapter does not add MCP settings, host
configuration, nested orchestration, or support for non-TUI interactions.

From the repo root, `mise run pi:check` runs the typecheck and the smoke suite
(`npm run check` here does the same once `npm ci` has installed the dependencies).

The smoke suite (`smoke-test.mjs`) loads the extension against a minimal stand-in
for Pi's host, with `fake-briefing.mjs` in place of the CLI: no browser, server,
credentials or model requests. It covers tool exposure, the command section and
its removal, command replacement, teardown on completion, failure, cancellation,
abort and shutdown, and resume versus new or forked sessions. How Pi itself turns
exposure and sections into callable tools and transcript patches is Pi's to test;
real model compliance and terminal appearance are outside this suite.
