# Codex CLI

## Install

```sh
cargo install --git https://github.com/JacobHayes/briefing --locked
mkdir -p ~/.codex/skills && ln -s "$(pwd)/skills/briefing" ~/.codex/skills/briefing
```

Add to `~/.codex/config.toml`:

```toml
[mcp_servers.briefing]
command = "briefing"
args = ["mcp"]
# Codex's per-call timeout is wall-clock (default 300s) and is NOT extended by progress
# notifications, but it pauses while an elicitation is pending. The server recognises Codex
# from the MCP handshake and holds the call open with an elicitation; this cap is a backstop.
tool_timeout_sec = 14400
```

## How the hold works

When Codex calls `brief_user`, the server (recognising Codex from `clientInfo`) sends a form elicitation whose message contains
the briefing URL. Codex pauses its tool timeout while that prompt is open. Briefing in the
browser, press **Submit** there, then accept the prompt (or just accept once you're done; the
server also cancels the prompt itself as soon as the browser submission lands). Declining the
prompt cancels the briefing.

If your approval policy disallows MCP elicitations (`approval_policy.granular.mcp_elicitations = false`),
the elicitation is auto-declined and the briefing is cancelled. Either allow elicitations or run
with `--hold progress` and a large `tool_timeout_sec`.

## Remote / hub

Codex supports streamable HTTP MCP servers:

```toml
[mcp_servers.briefing]
url = "http://100.x.y.z:7789/mcp"   # hub started with: briefing serve --mcp
tool_timeout_sec = 14400
```

Run the hub with `briefing serve --mcp` (see the README). Its `/` page lists briefings
awaiting feedback.

## Recovery

If Codex loses the tool call (timeout, restart), ask it to call `await_briefing` with the
briefing id shown on the page: it returns the stored feedback if you already submitted, or a
fresh link with your draft intact if not.
