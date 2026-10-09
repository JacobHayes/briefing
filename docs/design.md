# Design intent

What briefing is for, what it deliberately does not do, and the contract between the agent
and the page. This is the successor of the intent document that shipped with the original Pi
`guided` extension; the reading experience is the same, the plumbing changed.

## Purpose

A paced browser reading surface for when an answer is difficult to consume as one long chat
message. A briefing breaks complex information into semantic chunks, keeps context one click
away, and gathers useful feedback before the agent continues. It is for:

- substantial research with multiple dependent findings;
- explanations where later concepts depend on earlier context;
- material that benefits from an explicit Context panel as a stable memory aid;
- questions and decisions that need context before the user can answer;
- complex agent output where annotation and structured feedback beat a chat reply.

It is not the default renderer for every response. Short or simple answers stay in chat, and
richer rendering (tables, diagrams, charts, code) is a way to make a chunk clearer, not a
reason to open a briefing more often.

## Non-goals

- Do not replace the harness's chat UI or build a general web client.
- No built-in authentication; access control belongs to the network or proxy.
- Do not auto-open a briefing for every answer.
- Do not gamify reading. The design target is low visual and interaction load, nothing more.
- Do not infer comprehension from navigation or time spent. Only user-authored signal is
  returned.
- Do not keep long-term state. Records exist so a briefing survives its creating process and
  a device switch; they expire on fixed TTLs, and there is one result per briefing with no
  history.
- Do not build live back-and-forth with the model inside a chunk. Notes and questions return
  when the user submits; the agent answers in the conversation or presents a follow-up.

## Interaction model

Activation is proactive: when an answer crosses the complexity threshold the agent calls
`brief_user` without waiting to be asked, after finishing the research and reasoning. The
call validates the content, registers it, and hands back a link. What the user sees:

- **One semantic chunk per screen**, presented as an article: title, one-line purpose, lead
  point, key points, and optional inline details (links cite sources in the text). A single
  progress bar with a `Step X of Y` label sits in a slim sticky header.
- **Context on demand.** The goal, key context, running summary, and open questions live in a
  `Context` panel opened from the header, not permanently on screen.
- **Quiet by default.** Model-authored content that should be read is shown inline, and the
  only controls on a chunk are its questions.
- **Three ways to respond, each with a clear target:** an inline comment (a passage), a note
  (the whole briefing), or an answer (a question). There is no per-section response box and no
  follow-up flag: a comment or a note says what should be followed up.
- **Always-on inline commenting.** Selecting any passage in the reading column or the Context
  panel, by mouse, touch, or keyboard, reveals a `Comment` action; there is no mode to enable.
  Saved comments highlight their passage in place; hovering or focusing a highlight shows a
  read-only preview, clicking it pins the note with `Edit` and `Delete` (a two-step in-page
  confirm, never a browser dialog). Notes sit in the right margin when there is room and
  never cover presentation text. The `Comment` action and composer stay attached to the
  selected text as the page scrolls, and a composer too tall for the visible space (high zoom,
  a short window) sits below the selection and extends the page rather than covering the
  text. Mermaid nodes and edges and Vega-Lite charts can be commented on directly; those
  comments carry structured target metadata, and a Mermaid diagram can be expanded to fill
  most of the screen.
- **A sidebar with Outline and Notes tabs**, opened from the header on any screen. The Outline
  lists every chunk (with how many of its questions are still open) plus the review screen,
  and jumps to any of them; chunks can also link back to an earlier one with
  `[text](#section-N)`. Notes hold thoughts that belong to no one passage, including anything
  about the briefing as a whole; they return to the agent next to the other feedback and carry
  the same weight. Switching tabs keeps each one's scroll position and unsent text. The
  sidebar docks in the left margin on wide viewports and is a full-screen panel on narrow ones
  (sized to the visible viewport, so a touch keyboard never covers it).
- **Questions** sit at the bottom of the chunk they depend on (a chunk may carry several;
  questions about the whole briefing are asked on the review screen). One with options is a
  choice, recommended option first with its tradeoffs; a selection can be cleared, and
  `multiSelect` allows several. Every question has a visible box for the user's own answer.
  Nothing is required: an unanswered question goes back to the agent as `unresolved`. The first
  `Next` (or `Submit`) instead scrolls to an unanswered question whose heading has not yet been
  on screen, so none is skipped unseen.
- **Navigation** is Back, Next, and the Outline; a final review
  screen lists everything the user wrote (with an `Add a note` shortcut) before `Submit`. No
  timers, no automatic advancement.
- **Drafts persist.** Everything typed, including a half-written note or comment, is saved
  server-side (debounced, revisioned) and cached in the browser, so a refresh, a crash of the
  agent's process, or opening the link on another device continues where the user left off.
  Edits made on two devices merge (comments and notes by id, other fields by latest edit)
  rather than one copy replacing the other. The header says when the draft has not reached
  the server, and Cancel reports failure instead of claiming success. Annotations re-anchor from semantic section
  identity, text offsets, and quote context rather than DOM ranges, so highlights survive
  navigation and refresh. Selection boundaries come from the selected text rather than the raw
  DOM range, so a selection that spills past the end of a paragraph still anchors, while one
  whose text spans two regions still does not.

Keyboard: Left/Right move between screens when focus is outside a form control; `c` after a
keyboard selection opens the comment composer; `n` opens the Notes panel; Cmd/Ctrl+Enter saves
a comment; Escape closes the composer or a pinned note; highlights are focusable and
Enter/Space pins them.

The page uses plain wording ("Submit", "Submitted") rather than naming the agent, because
the same page serves every harness.

## Harness surfaces

| Harness | Shape | Commands |
|---|---|---|
| MCP (Claude Code, Codex, others) | `brief_user` returns the link at once; `await_briefing` blocks until submit; `cancel_briefing` | none |
| Pi extension | blocking `brief_user` for real briefings; `/brief-demo` and `/brief-result` temporarily enable command-only tools so they use the same active-tool UI mechanics | `/brief <request>`, `/brief-demo`, `/brief-reopen`, `/brief-cancel`, `/brief-result <id>`, `/brief-status` |
| CLI | `briefing present spec.json`, `briefing demo`, `briefing await <id>`, `briefing status` | |

Why the MCP shape is two calls, and how the wait survives client timeouts, is in the
README's "Long waits" section; the per-client budgets are `PROFILES` in `src/mcp.rs`.

## Configuration

The CLI's built-in bind default remains `auto` for every harness. Every invocation loads the
per-machine `config.toml`, then overlays environment variables, then explicit CLI arguments.
clap owns the argument layer and most environment parsing declaratively; the file is a fallback
below it in `run`. Only per-machine settings (`bind`, `hub`, `open`) live in the
file; per-client settings such as `hold` stay argument/environment only, since one binary serves
several MCP clients and each client's launcher sets its own. Configuration is strict so
misspelled or invalid file settings fail even when a later layer would override them, rather
than silently reverting to behavior the user did not select. Invalid environment values may also
fail in clap even when a CLI flag would override them.

`bind` shares one parser across file/env/CLI (`BindMode::from_str` in `src/bind.rs`): `auto`,
`local`, `tailscale`, or a literal IPv4/IPv6 address without a port or zone identifier. Explicit
IPs never fall back and report scope `explicit`, without implying network trust.

`open` is per-machine rather than per-command, so the layers resolve it once. Opening is a
client concern: `Backend::create` in `src/backend.rs` opens the URL on the creating process's
machine, whichever hub serves the briefing. The server side (`Site`) never
opens a browser, so `serve` is headless by construction, and a `serve` run that was given an
explicit `--open true` says so rather than ignoring it silently.

## Content contract

The model should:

- use semantic units rather than arbitrary word-count chunks, ordered by dependency, one main
  claim per chunk; 3-8 chunks by default, 10 at most;
- aim for 3-5 `keyPoints` per chunk (8 at most) and put optional depth that should still be
  read in `details`;
- use the `tray` (the Context panel) for stable context so it is not repeated on every chunk,
  and `remember` only for anchors needed later;
- ask questions where it needs the user's input, in the `questions` of the chunk they depend
  on, keeping top-level `questions` for ones that span the whole briefing; a choice offers 2-4
  meaningfully distinct options, the recommended one first and marked, with concrete
  tradeoffs and neutral wording (`multiSelect` when they are independent), and an open question
  has none; treat an `unresolved` answer as still open, never as approval;
- use rich Markdown only when it clarifies: GFM tables for comparisons and tradeoff matrices,
  fenced code with a language tag for technical examples, Mermaid for flows, architecture,
  state, and sequences, Vega-Lite for magnitude, trend, or segmentation. Prose is the default;
- respond only to the returned feedback afterwards, never repeating the presentation in chat.

## Limits

Input: whole presentation at most 1 MiB, fenced blocks at most 128 KB each; 1-10 chunks; per
chunk up to 8 `keyPoints`, 4 `remember`; tray up to 6 `keyContext` and 5
`openQuestions`; 0-6 top-level questions plus up to 4 per chunk, each with no options or 2-4 and
up to 4 `tradeoffs`; required text fields non-empty
after trimming.

Output: up to 500 annotations, each with a 2 000-character quote, 4 000-character comment,
and 300-character location; up to 100 free-standing notes of 20 000 characters each; other
user text 20 000 characters; request body 8 MiB.

Parsing is strict everywhere: presentations, page submissions, API requests and responses, and
stored records all reject unknown fields, so a stale client or page fails loudly instead of
losing data. A page submission is also checked against the briefing it answers (only the
questions it asked, only the options it offered, one choice unless `multiSelect`, every
question accounted for) and rejected rather than repaired. The one opaque value is the page's
draft, which only the page reads.

## Security and lifecycle

- Default to loopback/Tailscale. Explicit IPs are opt-in; wildcards expose all interfaces.
- Every briefing has one cryptographically random id (~131 bits). It is both the agent-side
  handle and the capability in the URL (`/briefing/<id>`); a separate URL token would protect
  nothing while the dashboard and agent API, which list both, are unauthenticated. Show the URL
  to the user so it can be opened from another authorized device. The id does not protect the
  hub dashboard or agent API.
- Require an allowed bound/public `Host` header on every request and an allowed `Origin` on
  browser writes. These are DNS-rebinding/cross-site defenses, not authentication.
  Strict CSP with a per-page nonce; renderer libraries are served from the binary, no
  CDN. Remote data and images referenced by content are allowed (charts, illustrations).
- Sanitize any HTML in content: no scripts, event handlers, dangerous URLs, or styles that
  could break the page.
- Briefings always live in a hub, and every CLI and MCP process is a client of one: the
  configured `--hub`, or this machine's own (`src/local_hub.rs`). One hub owns a state dir,
  enforced by an OS lock on `.hub.lock` taken before it loads or sweeps a record and held
  until it exits. The owner advertises itself in `.hub.json` (origin, bind, port, version, a
  per-process instance id, and a control secret); the file outlives the hub, so a client
  trusts it only when `/healthz` at that origin echoes the instance id. A client that finds no
  live hub starts `briefing serve --on-demand --idle-exit 60s` in the background, on its
  explicit settings or else the bind and port the last hub used; that hub exits a minute
  after nothing is open. A client newer than an on-demand hub replaces it immediately: a
  `POST /control/shutdown` with the secret, a wait for the lock to be released, then a fresh
  start. Waiting clients reconnect across the switch, and a client still running the older
  release keeps working against the newer hub, which answers it in its protocol (see the
  README's hub section). Discovery and replacement (`/healthz`, `/control/shutdown`) sit
  outside protocol negotiation, so they work across any two versions. A hub reloads every
  record at startup, upgrading older ones in place (`src/migrate.rs`), so a restart keeps
  briefings and, on the same port, their links; a browser alone cannot start one, so after a
  reboot a link answers once any client has run.
- Persistence is part of success: a create, draft save, or outcome is written to the store
  before memory changes and before waiters wake, in order, and a failed write is an error
  rather than a log line. `serve` refuses to run without a usable state dir. `present` therefore
  returns as soon as the briefing exists, and `await` is the only command that waits; an
  interrupted `await` stops waiting without cancelling.
- Every way of creating a briefing (CLI, MCP over stdio or HTTP, the hub's agent API) goes
  through the one site's create path, so validation and the link behave the same everywhere.
  Opening the browser happens a layer up, in the client's `Backend`, so briefings open in the
  creating client's browser unless disabled, and the hub never opens one itself. A browser
  opener that fails only logs a warning; the briefing stays live and the caller still shows
  the link.
- A wait ends in exactly one of `pending`, `completed`, or `cancelled`, carried as a tagged
  `status` with the feedback alongside; the CLI's `--json` output, the hub API, and the MCP
  `await_briefing` result all use that shape. Records are written to the user's state directory with
  owner-only permissions and swept 7 days after finishing (28 days if never answered).
- Restrict all hub routes through network controls or an authenticating proxy; block untrusted
  direct access. Briefing does not authenticate proxy identity headers.
- `--public-origin` sets generated URLs and the allowed proxy Host/Origin, not binding or TLS.
- Fail visibly when the browser cannot be opened and the link cannot be shown.
