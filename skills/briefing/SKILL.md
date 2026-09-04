---
name: briefing
description: Present complex research, multi-part explanations, or contextual decisions in a paced browser briefing (brief_user tool) and continue from the user's notes, inline comments, and decisions. Use when an answer is too long or too dependent on earlier context to read well as one chat message, or when the user asks for a briefing.
---

# Briefing

`brief_user` opens a paced browser page that shows one semantic chunk per screen, keeps
stable context one click away, collects decisions, and returns only user-authored signal:
notes, checkpoint answers, inline comments on selected passages, flagged sections, and decisions.

`brief_user` validates the content and opens the briefing. What happens next depends on the
harness:

- **Pi extension:** `brief_user` shows the link in Pi's UI and blocks until the user submits,
  returning `feedback` directly.
- **MCP server (Claude Code, Codex, ...):** `brief_user` returns **immediately** with
  `status: "open"`, `url`, and `briefingId`. Put that exact link in your reply (the user may
  be on another machine and a browser is not always opened for them), then call
  `await_briefing` with the `briefingId`; it blocks until they submit. If it returns
  `status: "pending"`, call it again. `cancel_briefing` abandons an open briefing.

`feedback` is `{chunks[{title, status, checkpoint, note}], decisions[{question, selected,
note}], annotations[{location, quote, comment, target?}], overallNote}`; `status: "revisit"`
marks a section flagged for follow-up. MCP results are structured JSON with an
`instructions` field.

Briefings outlive the process that created them (unanswered for two weeks, results for
6 h). If the harness backgrounds
`await_briefing`, wait for its completion notification rather than polling. If a session was
interrupted, or the user hands you a briefing id, call `await_briefing` with it: it returns the
stored feedback if they already submitted, or `status: "reopened"` with a fresh link to relay
(their draft is preserved). In Pi, `/brief-result <id>` does the same.

## When to use it

Use it proactively, without waiting to be asked, when an answer crosses a complexity threshold:

- substantial research with multiple dependent findings;
- explanations where later concepts depend on earlier ones;
- briefings that benefit from a persistent Context panel;
- decisions the user cannot make without context first;
- complex agent output where annotation and structured feedback beat a chat reply.

Keep short or simple answers as normal chat. Rich rendering support is not a reason to open
a briefing more often.

## Content contract

Finish the research and reasoning first, then call the tool once.

- **Chunks**: 3-8 (max 10) semantic units in dependency order, one main claim each.
  `mainPoint` is concise; aim for 3-5 `keyPoints` (max 8); `details` holds focused
  supporting explanation that should be read by default; `remember` holds only anchors needed
  later (max 4); `checkpoint` asks an explicit question when you need an answer; `sources` are
  absolute http(s) URLs (max 6).
- **Tray** (the Context panel): stable `keyContext` (max 6), a `runningSummary`, and
  `openQuestions` (max 5). Put context here instead of repeating it on every chunk.
- **Decisions**: 0-6, each with 2-4 meaningfully distinct options. Put the recommended option
  first and set `recommended: true` on it only. State concrete `tradeoffs` (max 4) without
  loaded wording. `required` defaults to true.
- **Rich text**: every prose field accepts Markdown: emphasis, links (bare domains allowed),
  lists, GFM tables, inline code, fenced code with a language tag, ```mermaid fences, and
  ```vega-lite fences (JSON spec). Use them only when they clarify: tables for comparisons and
  tradeoff matrices, Mermaid for flows/architecture/state, Vega-Lite for magnitude or trend,
  code fences for technical examples. Prose is the default.
- Whole presentation under 1 MiB; fenced blocks under 128 KB each. The user can leave up to
  500 inline comments.

## After the result

Respond only to what came back: answer checkpoint questions, act on decisions, address inline
comments (each carries a location, the quoted passage, and the comment), and follow up on
flagged sections. Do not repeat the presentation as a chat message. If the user cancelled,
ask how they would like to proceed instead of re-opening the briefing.

## Minimal example

```json
{
  "title": "Choosing a queue backend",
  "goal": "Pick a queue for the ingest pipeline",
  "chunks": [
    {
      "title": "What the pipeline needs",
      "mainPoint": "Ordering per tenant matters more than raw throughput.",
      "keyPoints": ["~2k msg/s peak", "At-least-once is fine", "Ops team already runs Postgres"],
      "checkpoint": "Is per-tenant ordering a hard requirement?"
    }
  ],
  "tray": { "keyContext": ["Deadline: end of quarter"], "runningSummary": "Ordering > throughput." },
  "decisions": [
    {
      "question": "Which backend?",
      "options": [
        { "label": "Postgres SKIP LOCKED", "recommended": true, "tradeoffs": ["No new infra", "Caps near 10k msg/s"] },
        { "label": "Kafka", "tradeoffs": ["Ordering per partition", "New cluster to run"] }
      ]
    }
  ]
}
```
