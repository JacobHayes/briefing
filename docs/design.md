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
- decisions that need context before the user can choose;
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
  point, key points, optional inline details, and sources behind a disclosure. A single
  progress bar with a `Step X of Y` label sits in a slim sticky header.
- **Context on demand.** The goal, key context, running summary, and open questions live in a
  `Context` panel opened from the header, not permanently on screen.
- **Quiet by default.** Model-authored content that should be read is shown inline. Per-section
  response controls stay collapsed behind `Respond`, except when the chunk carries a
  checkpoint question, in which case the response field opens by default. Each section has at
  most one free-text response plus a `Flag this section for follow-up` marker.
- **Always-on inline commenting.** Selecting any passage in the reading column or the Context
  panel, by mouse, touch, or keyboard, reveals a `Comment` action; there is no mode to enable.
  Saved comments highlight their passage in place; hovering or focusing a highlight shows a
  read-only preview, clicking it pins the note with `Edit` and `Delete` (a two-step in-page
  confirm, never a browser dialog). Notes sit in the right margin when there is room and
  never cover presentation text. Mermaid nodes and edges and Vega-Lite charts can be
  commented on directly; those comments carry structured target metadata.
- **A free-standing Notes panel**, opened from the header on any screen, holds thoughts that
  belong to no section (inline comments and per-section responses cover those). It docks in
  the left margin on wide viewports and is a bottom sheet on narrow ones. Notes return to the
  agent next to the other feedback and carry the same weight.
- **Decisions** are cards with a recommended option first, tradeoffs, and a collapsed
  guidance field, either inline at the bottom of the chunk they depend on or standalone after
  the chunks for choices that span the whole briefing. Required decisions block `Next` until
  answered.
- **Navigation** is Back, Next, and an always-available `View all` escape hatch; a final review
  screen lists everything the user wrote before `Submit`. No timers, no automatic advancement.
- **Drafts persist.** Everything typed is saved server-side (debounced, revisioned) and cached
  in the browser, so a refresh, a crash of the agent's process, or opening the link on another
  device continues where the user left off. Annotations re-anchor from semantic section
  identity, text offsets, and quote context rather than DOM ranges, so highlights survive
  navigation and refresh. Selection boundaries come from the selected text rather than the raw
  DOM range, so a selection that spills past the end of a paragraph still anchors, while one
  whose text spans two regions still does not.

Keyboard: Left/Right move between screens when focus is outside a form control; `c` after a
keyboard selection opens the comment composer; `n` opens the Notes panel; Cmd/Ctrl+Enter saves
a comment; Escape closes the composer or a pinned note; highlights are focusable and
Enter/Space pins them.

The page uses plain wording ("Respond", "Submitted") rather than naming the agent, because
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
below it in `run`. Only per-machine settings (`bind`, `hub`, `on_create`, `open`) live in the
file; per-client settings such as `hold` stay argument/environment only, since one binary serves
several MCP clients and each client's launcher sets its own. Configuration is strict so
misspelled or invalid file settings fail even when a later layer would override them, rather
than silently reverting to behavior the user did not select. Invalid environment values may also
fail in clap even when a CLI flag would override them.

`bind` shares one parser across file/env/CLI (`BindMode::from_str` in `src/bind.rs`): `auto`,
`local`, `tailscale`, or a literal IPv4/IPv6 address without a port or zone identifier. Explicit
IPs never fall back and report scope `explicit`, without implying network trust.

`open` is per-machine rather than per-command, so the layers resolve it once. What differs
between an embedded server and the hub is a `Role` in `src/main.rs`, the single place that says
a hub serves the agent API and never opens a browser; a `serve` run that was given an explicit
`--open true` says so rather than ignoring it silently.

## Content contract

The model should:

- use semantic units rather than arbitrary word-count chunks, ordered by dependency, one main
  claim per chunk; 3-8 chunks by default, 10 at most;
- aim for 3-5 `keyPoints` per chunk (8 at most) and put optional depth that should still be
  read in `details`;
- use the `tray` (the Context panel) for stable context so it is not repeated on every chunk,
  and `remember` only for anchors needed later;
- ask an explicit `checkpoint` question when it needs an answer;
- offer 2-4 meaningfully distinct decision options, the recommended one first and marked,
  with concrete tradeoffs and neutral wording; put a decision on the chunk it depends on and
  keep top-level `decisions` for choices that span the whole briefing;
- use rich Markdown only when it clarifies: GFM tables for comparisons and tradeoff matrices,
  fenced code with a language tag for technical examples, Mermaid for flows, architecture,
  state, and sequences, Vega-Lite for magnitude, trend, or segmentation. Prose is the default;
- respond only to the returned feedback afterwards, never repeating the presentation in chat.

## Limits

Input: whole presentation at most 1 MiB, fenced blocks at most 128 KB each; 1-10 chunks; per
chunk up to 8 `keyPoints`, 4 `remember`, 6 `sources`; tray up to 6 `keyContext` and 5
`openQuestions`; 0-6 top-level decisions plus at most one per chunk, each with 2-4 options and
up to 4 `tradeoffs`; sources must be absolute http(s) URLs; required text fields non-empty
after trimming.

Output: up to 500 annotations, each with a 2 000-character quote, 4 000-character comment,
and 300-character location; up to 100 free-standing notes of 20 000 characters each; other
user text 20 000 characters; request body 8 MiB.

## Security and lifecycle

- Default to loopback/Tailscale. Explicit IPs are opt-in; wildcards expose all interfaces.
- Every briefing URL carries a cryptographically random capability token; the agent side uses
  a separate id. Show the URL to the user so it can be opened from another authorized device.
  Capability tokens do not protect the hub dashboard or agent API.
- Require an allowed bound/public `Host` header on every request and an allowed `Origin` on
  browser writes. These are DNS-rebinding/cross-site defenses, not authentication.
  Strict CSP with a per-page nonce; renderer libraries are served from the binary, no
  CDN. Remote data and images referenced by content are allowed (charts, illustrations).
- Sanitize any HTML in content: no scripts, event handlers, dangerous URLs, or styles that
  could break the page.
- The embedded server starts lazily on the first briefing, not at process start, and serves
  for the life of the process. Every way of creating a briefing (CLI, MCP over stdio or HTTP,
  the hub's agent API) goes through the one site's create path, so validation, the recorded
  link, and `--on-create` behave the same everywhere. Embedded briefings open in the local
  browser unless disabled; the long-running hub never opens a browser itself. A briefing whose
  browser opener fails is cancelled rather than left dangling.
- A wait ends in exactly one of `pending`, `completed`, or `cancelled`, carried as a tagged
  `status` with the feedback alongside; the CLI's `--json` output, the hub API, and the MCP
  `await_briefing` result all use that shape (MCP adds `reopened` for a recovered briefing). Records are written to the user's state directory with
  owner-only permissions and swept 6 hours after finishing (14 days if never answered).
- Restrict all hub routes through network controls or an authenticating proxy; block untrusted
  direct access. Briefing does not authenticate proxy identity headers.
- `--public-origin` sets generated URLs and the allowed proxy Host/Origin, not binding or TLS.
- Fail visibly when the browser cannot be opened and the link cannot be shown.
