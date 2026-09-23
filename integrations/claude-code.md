# Claude Code

## Install

```sh
mise use -g github:JacobHayes/briefing@latest   # or: cargo install --git https://github.com/JacobHayes/briefing --locked
claude mcp add --scope user briefing -- briefing mcp
```

The MCP server carries its own model guidance; the bundled `skills/briefing` skill is only for
raw CLI use without this MCP integration.

Claude Code's idle timeout for MCP calls (30 min on stdio) is reset by progress notifications,
and the server sends one every 10 seconds while a briefing is open, so `await_briefing` can
block for as long as you take (the wall-clock cap is about 28 h; the server returns `pending`
after 24 h and the model calls again). Claude Code moves any MCP call over 2 minutes into a
background task and notifies the model when it completes; the tool text tells the model not
to poll in the meantime. Set `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0` if you would rather the
call stay in the foreground.

If a session dies or is restarted, ask the new session to call `await_briefing` with the
briefing id (shown on the page and in the earlier tool output): it returns the stored feedback
if you already submitted, or a fresh link with your draft intact if not.

## Headless box, browser elsewhere

The embedded server binds to the box's Tailscale address when Tailscale is running, and the
model shows you the link, so opening it from a laptop or phone on the tailnet just works.

## Remote sessions (Claude Code web, another machine)

Run a hub on a box you can reach from both the agent and your browser, then point the client at it:

```sh
# on the hub box (Tailscale address is picked automatically)
briefing serve --mcp

# on the agent side, either stdio with a remote backend...
claude mcp add --scope user briefing -e BRIEFING_HUB=http://100.x.y.z:7789 -- briefing mcp
# ...or the hub's MCP endpoint directly
claude mcp add --scope user --transport http briefing http://100.x.y.z:7789/mcp
```

The hub's `/` page lists briefings awaiting feedback with their links.

## Tools

| Tool | Purpose |
|---|---|
| `brief_user` | Validate, open the briefing, return `url` + `briefingId` immediately (the model shows you the link). |
| `await_briefing` | Block until you submit and return `feedback` (or `pending` after the client's budget; the model calls it again). With an id from an earlier session it returns the stored feedback, or `reopened` plus a fresh link. |
| `cancel_briefing` | Cancel an open briefing. |
