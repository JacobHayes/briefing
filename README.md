# briefing

**Paced browser briefings for coding agents.** When Claude Code, Codex, Pi, or any agent
that speaks MCP has something too long or too layered for a chat reply, it opens a briefing
in your browser instead: one idea per screen, context one click away, inline comments on
anything you select, optional questions right under the context they depend on (pick an
option or answer in your own words), a free-standing Notes panel, and a review screen before
you send it back.
Only what you wrote returns to the agent.

![Walkthrough: read a chunk, select a sentence and comment on it, open the Context panel, pick a decision, review, submit](docs/screenshots/tour.gif)

```mermaid
flowchart LR
    A[Agent finishes<br/>its research] --> B[Opens a briefing<br/>in your browser]
    B --> C[You read it one idea at a time,<br/>comment, and answer]
    C --> D[Your notes and answers<br/>go back to the agent]
```

## Why

- **Long agent answers don't get read.** A wall of text in a terminal is skimmed, then
  argued with from memory. A briefing paces the same content so each idea lands before the
  next one.
- **Feedback should be precise.** Select any sentence, table cell, diagram node, or chart and
  comment on exactly that. The agent gets the quoted passage with your note, not a paraphrase.
- **Questions need context first.** A chunk's questions sit at the bottom of the chunk that
  justifies them (questions about the whole briefing go on the review screen), with the
  recommended option first and its tradeoffs spelled out. Answers are optional: skip one and
  it goes back as unresolved, never as a silent yes.
- **It survives everything.** Drafts save as you type. If the agent's process dies, or you
  switch from laptop to phone, the briefing picks up where you left off, and the agent can
  fetch your answers later.

## A tour

| | |
|---|---|
| ![Inline comment pinned in the margin next to the highlighted passage](docs/screenshots/03-inline-comment.png) | ![The Context panel: goal, things to keep in mind, running summary, open questions](docs/screenshots/04-context.png) |
| Select text, press **Comment**, and the note lives next to the passage. | The **Context** panel keeps the goal and stable facts one click away. |
| ![A decision card with a recommended option and tradeoffs](docs/screenshots/05-decision.png) | ![The review screen listing every answer, decision, and comment before submit](docs/screenshots/06-review.png) |
| Decisions come with a recommendation and honest tradeoffs. | The review screen shows exactly what goes back, and nothing else. |
| ![A Mermaid diagram inside a chunk with its own Comment button](docs/screenshots/02-rich-content.png) | ![A Vega-Lite bar chart inside a chunk](docs/screenshots/02b-chart.png) |
| Markdown, GFM tables, code, and Mermaid diagrams render inline; nodes and edges are commentable, and a diagram expands to fill the screen. | Vega-Lite charts too, all served from the binary with no CDN. |

Try it yourself: `briefing demo`.

## Quick start

**1. Install** (macOS arm64, Linux arm64/amd64 prebuilt; one static binary):

```sh
mise use -g github:JacobHayes/briefing@latest
# or: cargo install --git https://github.com/JacobHayes/briefing --locked
```

**2. Connect your agent:**

| Harness | Setup | Notes |
|---|---|---|
| Claude Code | `claude mcp add --scope user briefing -- briefing mcp` | [integrations/claude-code.md](integrations/claude-code.md) |
| Codex | `[mcp_servers.briefing]` with `command = "briefing"`, `args = ["mcp"]`, `tool_timeout_sec = 14400` | [integrations/codex.md](integrations/codex.md) |
| Pi | `pi install git:github.com/JacobHayes/briefing` | extension, requires Pi >=0.99.1, [integrations/pi](integrations/pi/README.md) |
| Anything else | `briefing present presentation.json` | JSON in, feedback out |

Optionally link `skills/briefing` into a harness's skills directory for raw CLI use if it does
not use the MCP server or Pi extension; those integrations already carry their own guidance.

**3. Ask for one.** Say "brief me on the options for X" or just let the agent decide: it is
told to open a briefing whenever an answer crosses a complexity threshold and to stay in
chat for anything short.

## How it works

Briefings always live in a hub: the one you point clients at with `--hub`, or else this
machine's own, which the first client starts in the background (see
[Recovery and hand-off](#recovery-and-hand-off)). `brief_user` validates the content, creates
the briefing there, and returns the link at once so the agent can show it to you (you may be
on a different machine from the agent). `await_briefing` then blocks until you press
**Submit** and returns your feedback as MCP `structuredContent`:

```jsonc
{
  "status": "completed",
  "briefingId": "7rJ-tS8jIOb8SPX5",
  "feedback": {
    "questions":   [{ "question": "...", "section": "...", "selected": ["..."], "answer": "...", "status": "answered" }],
    "annotations": [{ "location": "...", "quote": "...", "comment": "...", "target": { "..." : "..." } }],
    "notes":       ["..."]
  },
  "instructions": "Respond only to this feedback ..."
}
```

Each result carries an `instructions` field telling the model what to do next; the text
block is a one-line summary because Claude Code hands the model only the structured part.
All three tools (`brief_user`, `await_briefing`, `cancel_briefing`) declare an
`outputSchema`. Caps: 500 inline comments, 4 000-character comments, 100 free-standing notes,
20 000-character notes.

The page itself is a single calm reading column: `Step X of Y`, Back and Next, an **Outline**
to jump to any section, and no timers or auto-advance. The full interaction model, the
content contract the agent follows, and the non-goals are in
[docs/design.md](docs/design.md).

### Long waits

A briefing can take an hour; most MCP clients time out a tool call in a minute. The server
reads `clientInfo.name` and capabilities from the MCP handshake and picks a hold strategy per
client, with no extra tool parameters: `notifications/progress` heartbeats every 10 s for
clients whose timer resets on progress or that have no timeout (Claude Code, VS Code), a form
elicitation for Codex (whose timer pauses while one is open; the server cancels it when the
browser submits, declining it cancels the briefing), and a short budget (50 s) for the
60-second clients (Cursor, Cline, Zed, Continue, OpenCode, Pi's MCP adapter, unknown).

When the budget runs out the tool returns `status: "pending"` with the `briefingId` and the
model calls `await_briefing` again; you never notice. Claude Code moves calls longer than
two minutes into a background task and notifies the model on completion; the tool text tells
the model to wait for that rather than poll. `--hold` and `--max-wait-secs` override the
plan. The per-client table, with sources, is `PROFILES` in
[src/mcp.rs](src/mcp.rs).

Pi's own extension has no client-side tool timeout to work around, so it exposes a single
blocking `brief_user` that shows the link in Pi's UI and returns the feedback directly. The
`/brief-demo` and `/brief-result` commands route through temporary command-only tools so they
exercise the same active-tool UI mechanics. All three Pi interaction tools are model-only:
codemode scripts and other nested tool calls cannot open them. Command guidance uses a named
structured prompt section that is removed on the following ordinary turn. Quitting or killing
Pi while a briefing is open leaves the briefing open (only Esc or `/brief-cancel` cancels it),
and resuming that session reattaches to it through the `/brief-result` path: the stored
feedback if the user has submitted, otherwise the same link and a new wait. Individual
long-poll HTTP requests to a hub may still time out; `briefing await` treats those as pending
and repolls internally, so callers never see the timeout.

## Recovery and hand-off

A briefing lives in a hub, never in the agent or the command that created it. Without
`--hub`, that is this machine's own hub: a `briefing serve` that the first client starts in
the background, that exits after a minute with nothing open, and that the next client starts
again. When a client is newer than that hub it replaces it on the spot; open pages and
waiting agents ride through the sub-second switch. The hub writes every briefing to
`$XDG_STATE_HOME/briefing/briefings/<id>.json` (default `~/.local/state/...`) - the
presentation, your in-progress draft, and the submitted result - before acknowledging a
change, and reloads them when it starts. It comes back on the port and bind it last used
(7789 and `auto` by default; `--port`/`--bind` choose), so links survive restarts. A browser
alone cannot start the hub, though: after a reboot or an idle exit, a link answers again once
any `briefing` command or MCP call has run.

- **Agent disconnected after you submitted:** `await_briefing` (or `briefing await <id>`)
  from any later process returns the stored result. Results are kept for 7 days.
- **Agent died before you submitted:** `await_briefing` with the id keeps waiting on the
  same link, starting this machine's hub again if needed; your draft is intact. The id is
  shown on the page's Submitted screen and error banner, in `brief_user` output, and by
  `briefing status`.
- **Interrupted `briefing await`:** it only stops waiting; the briefing stays open until you
  submit, cancel it on the page, or run `briefing cancel <id>`.
- **Switching devices mid-briefing:** drafts are saved server-side (debounced, revisioned;
  the page adopts a newer draft on focus) and cached in localStorage, so opening the same
  link elsewhere continues where you left off.

Unanswered briefings expire after 28 days. One result per briefing, no history.

## CLI

```sh
briefing demo                      # open the bundled demo
briefing present spec.json         # create a briefing and print its link; --json for JSON
briefing await <briefingId>        # wait for its feedback and print it; --json for JSON
briefing cancel <briefingId>       # cancel an open briefing
briefing schema                    # JSON Schema for presentation input
briefing guidance cli              # agent-facing CLI workflow guidance
briefing guidance pi               # Pi extension guidance as JSON
briefing guidance mcp              # MCP instructions text
briefing guidance skill            # print the CLI-focused Agent Skill markdown
briefing mcp                       # MCP over stdio
briefing serve --mcp               # long-lived hub (see below)
briefing status                    # list known briefings (waiting / completed / cancelled)
```

`present` returns as soon as the briefing exists, printing its link and the next step; with
`--json`, one line: `{ "briefingId", "status": "active", "url", "scope", "openedBrowser",
"instructions" }`. `await` prints the link on stderr while the briefing is open (a JSON
`ready` event with `--json`) and the result on stdout. With `--json` the result is one line,
the same `status` shape the MCP tool and the hub API return:

```jsonc
{ "briefingId": "7rJ-tS8jIOb8SPX5", "status": "completed", "feedback": { "questions": [], "annotations": [], "notes": ["..."] } }
{ "briefingId": "7rJ-tS8jIOb8SPX5", "status": "cancelled", "feedback": { "..." : "..." } }
{ "briefingId": "7rJ-tS8jIOb8SPX5", "status": "pending" }
```

Exit codes: 0 whenever the briefing reached an outcome (read `status`: completed, cancelled, or
pending after `--wait-seconds`), 130 interrupted, 1 on errors.

| Flag / env | Meaning |
|---|---|
| `--bind auto\|local\|tailscale\|IP` (`BRIEFING_BIND`) | Where the server listens: `auto` prefers Tailscale, otherwise loopback; `local` uses `127.0.0.1`; `tailscale` and literal IPv4/IPv6 addresses fail instead of falling back |
| `--open true\|false` (`BRIEFING_OPEN`) | Whether this client opens new briefings in the local browser, including hub-created briefings. `serve` ignores it because the hub process stays headless |
| `--hub URL` (`BRIEFING_HUB`) | Use this hub instead of this machine's own |
| `--port N` (`BRIEFING_PORT`) | Port this machine's hub listens on (default 7789) |
| `BRIEFING_STATE_DIR` | Where this machine's hub keeps records (default `$XDG_STATE_HOME/briefing/briefings`). One hub owns it at a time (`.hub.lock`) and advertises itself in `.hub.json` |
| `BRIEFING_CONFIG` | Override the settings file path |
| `BRIEFING_BROWSER`, `BRIEFING_LOG` | Override the browser opener; tracing filter |

### Per-machine configuration

Briefing always reads `$XDG_CONFIG_HOME/briefing/config.toml` (default
`~/.config/briefing/config.toml`) first, then overlays environment variables, then explicit
command-line arguments. For example,

```toml
bind = "local"                    # auto | local | tailscale | literal IPv4/IPv6 address
hub = "https://briefings.example" # use a remote hub instead of this machine's own
open = false                      # do not open the system browser from this client
```

For each key, the value comes from the settings file, then `BRIEFING_<KEY>`, then the matching
argument, in increasing priority (`bind` also has a built-in `auto` default). For example
`--open true`/`--open false` override `BRIEFING_OPEN` and the file setting for commands that
create briefings, whichever hub serves them. `serve` is a headless hub and
never opens a browser, and says so when given an explicit `--open true`. Unknown fields or an
invalid settings file fail visibly even when overridden; invalid environment values may also
fail before CLI overrides apply.
Set `BRIEFING_CONFIG` to use another file; an explicitly selected file must exist. Settings that are
per client rather than per machine (such as `--hold`) stay argument/environment only, so each MCP
client's launcher can set its own.

## Hub mode (optional)

This machine's own hub only helps when your browser can reach the machine the agent runs on.
For a session running elsewhere (Claude Code web, Codex cloud, a headless box), run one
long-lived `briefing serve` somewhere reachable, and point every harness on every machine at it
with `--hub`:

```sh
briefing serve --mcp
```

- Defaults to Tailscale when available, otherwise loopback. Override with `--bind`.
- Serves briefing pages, a dashboard at `/` listing briefings awaiting feedback (with links,
  progress, and a cancel action) and recent results, the agent API (`/agent/briefings`), and
  with `--mcp` a streamable-HTTP MCP endpoint at `/mcp`.
- `--finished-ttl 7d` / `--active-ttl 28d` tune retention; this machine's on-demand hub uses
  the same defaults. Hubs sweep expired records in the background once a minute.
- `--public-origin https://briefings.example` when fronted by a reverse proxy (TLS lives there).
- The hub never tries to open a browser itself; clients using the hub can still open the
  returned URL locally with their own `--open`/`BRIEFING_OPEN`/config setting.
- `GET /agent/briefings/{id}/wait?timeout_secs=N` answers with the same
  `{ "briefingId", "status": "completed" | "cancelled" | "pending", "feedback"? }` shape as
  the CLI's `--json` output.
- Clients either point the stdio server at it (`briefing mcp --hub URL`) or connect to
  `/mcp` directly. `briefing --hub URL present|await|cancel|status` work against a hub.
- Clients, pages, and the hub name their wire protocol in a `Briefing-Protocol` header on the
  API routes (`/agent/*` and the page's `/api/*`). A hub serves its own protocol and the one
  before it (translating for the older one and answering in it), and answers anything else
  with a 426 naming both; a client refuses a response in any protocol but its own. So after
  upgrading the hub, agent sessions started on the previous release keep working until they
  restart; two releases behind, they get a clear error instead of misread data. Pages, the
  dashboard, `/mcp`, and `/healthz` take no header.

### Behind a reverse proxy

```sh
briefing serve --bind 127.0.0.1 --port 7789 --mcp \
  --public-origin https://briefings.example
```

`--public-origin` sets generated URLs and the allowed proxy Host/Origin; TLS stays at the
proxy. Preserve the public Host header or use the bound IP.

`bind` accepts literal IPv4/IPv6 addresses, without ports, on all config surfaces.
Explicit IPs never fall back. Wildcards (`0.0.0.0`, `::`) listen on all interfaces and need
`--public-origin` for usable links.

## Security model

- Defaults to loopback/Tailscale; other bind addresses are opt-in.
- Every briefing has one random, unguessable id, used by the agent side and as the URL capability.
- `Host` and browser-write `Origin` checks prevent DNS rebinding and cross-site requests.
- Strict CSP with a per-page nonce; renderer libraries are served from the binary.
- Presentation and feedback sizes are capped. Records are written to the user's state
  directory with owner-only permissions and deleted 7 days after finishing (28 days if never
  answered) by the hub while it runs.
- A hub is one trusted audience: anyone who can reach it can list, read, create, cancel, and
  submit every briefing on it, so share a hub only with devices you trust as much as the agent.
- No built-in authentication. Restrict all routes through network controls or an authenticating
  proxy; block untrusted direct access. Proxy identity headers are not authentication. The one
  authenticated route, `/control/shutdown`, takes a secret only readable from the state dir.

## Development

```sh
mise install         # rust (+ clippy, rustfmt, release targets), zig, cargo-zigbuild, node
mise run check       # full project gate: format, generated skill, clippy -D warnings, tests, Pi extension
mise run pi:check    # focused Pi extension typecheck/smoke
mise run fix         # format, clippy --fix, regenerate skill
mise use -g github:JacobHayes/briefing@latest   # or: cargo install --path . --locked
mise run assets:update
cargo zigbuild --release --target aarch64-apple-darwin   # any release target, from Linux
```

The build embeds the browser renderer libraries (marked, DOMPurify, highlight.js, Mermaid,
Vega, Vega-Lite, vega-embed). `build.rs` installs the versions pinned in
`assets/package-lock.json` with `npm ci` (or `bun install`) on first build, so Node or Bun must
be available; offline builds can point `BRIEFING_VENDOR_DIR` at a directory holding the
seven files.

CI (`.rwx/ci.yml`) runs the non-mutating `mise run check` (which includes the page's browser
regression tests in `tests/browser`, Playwright + Chromium) and cross-builds every push; releases (`.rwx/release.yml`) are immutable calver tags `vYYYY.MM.DD.N`, published
by every push to `main` once its CI passes. The release build sets `BRIEFING_VERSION` to the tag, which
`build.rs` bakes into `briefing --version`; other
builds report the Cargo version with a `-dev` suffix. The workflow files
carry the details. Fresh releases can be hidden by mise's `minimum_release_age` for a while;
`MISE_MINIMUM_RELEASE_AGE=0` overrides. TLS is rustls + ring with bundled webpki roots, so no
platform SDKs are needed to cross-compile.

Tests cover validation, the hub state machine, the on-disk store, drafts, host/origin checks,
the full HTTP flow, a hub restart that keeps its briefings and links, and the MCP server driven
over stdio (progress hold, pending/await/cancel, the Codex elicitation hold, and a briefing
outliving its server and the on-demand hub).
