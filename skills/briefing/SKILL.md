---
name: briefing
description: Present complex research, multi-part explanations, or contextual decisions as paced browser briefings through the `briefing` CLI. Use when an answer is too long or dependent on earlier context to read well as one chat message, or when the user asks for a briefing.
---

# Briefing

`briefing` opens a paced browser briefing for complex agent output. The user reads one idea
at a time, can comment on exact passages, choose decision options, and submit feedback for
you to act on.

Use this skill when you need to create a briefing through the `briefing` CLI. It fits
substantial research, layered explanations, or decisions that need context first; keep short
or simple answers in chat.

## Discover usage

Before creating a briefing, ask the installed binary for current instructions instead of
relying on this file for flags or JSON shape:

```bash
briefing guidance cli
briefing schema
briefing present --help
```

For recovery, inspect:

```bash
briefing status --help
briefing await --help
```

## Minimal flow

- Finish your research first.
- Build a presentation JSON file that matches `briefing schema`.
- Run `briefing present <file> --json` or pipe JSON to `briefing present --json`.
- When feedback returns, respond only to that feedback: answer checkpoints, address inline
  comments, follow up on revisit flags, and act on notes and decisions.
