//! Model-facing guidance, kept in one place so each integration can wrap the same briefing
//! behavior in the terms its tool surface uses.

use std::time::Duration;

use crate::content::{MAX_CHUNKS, MAX_KEY_POINTS, MAX_OPTIONS, MIN_OPTIONS};
use crate::hub::HubConfig;

const SKILL_DESCRIPTION: &str = "Present complex research, multi-part explanations, or contextual decisions as paced browser briefings through the `briefing` CLI. Use when an answer is too long or dependent on earlier context to read well as one chat message, or when the user asks for a briefing.";

#[derive(Clone, Copy)]
struct Surface {
    use_action: &'static str,
    create_action: &'static str,
}

const TOOL_SURFACE: Surface = Surface { use_action: "brief_user", create_action: "call brief_user once" };
const CLI_SURFACE: Surface = Surface { use_action: "the `briefing` CLI", create_action: "create one briefing" };

fn when_to_brief(surface: Surface) -> String {
    format!(
        "Use {} proactively whenever an answer crosses a complexity threshold: substantial research with dependent \
         findings, multi-part explanations, or decisions that need context. Keep short and simple answers as normal \
         chat.",
        surface.use_action
    )
}

fn content_guidance(surface: Surface) -> String {
    format!(
        "Finish the research and reasoning first, then {} with 3-8 semantic chunks (at most {MAX_CHUNKS}) in dependency \
         order: one main idea per chunk, 3-5 keyPoints each (at most {MAX_KEY_POINTS}), focused details, stable context \
         in tray, and {MIN_OPTIONS}-{MAX_OPTIONS} distinct decision options with the recommended one first and marked. \
         Put a decision on the chunk it depends on (the chunk's `decision` field) so its options sit right under their \
         context; use top-level decisions only for choices that span the whole briefing.",
        surface.create_action
    )
}

fn authoring_guidance() -> String {
    "Put durable context in the tray instead of repeating it in chunks; use remember only for anchors needed later; keep \
     decision tradeoffs concrete and neutral; do not open a briefing merely because rich rendering could be used."
        .into()
}

fn rich_text_guidance() -> String {
    "Text fields accept Markdown, GFM tables, fenced code with a language tag, and ```mermaid and ```vega-lite fences, \
     which render as diagrams and charts the user can comment on. Use a Mermaid diagram whenever structure is \
     easier to see than read (flows, architecture, sequences, state machines, dependencies), and a Vega-Lite chart for \
     magnitudes or trends; skip them when they would only decorate."
        .into()
}

fn result_guidance() -> String {
    "After the feedback arrives, respond only to it; do not repeat the presentation as a chat message. Treat \
     free-standing notes as first-class feedback, answer checkpoint responses, address inline comments using their \
     location and quote, and follow up on chunks marked status `revisit`."
        .into()
}

fn retention_guidance() -> String {
    format!(
        "Briefings outlive the process that created them (unanswered ones for {}, results for {}).",
        human(HubConfig::ACTIVE_TTL),
        human(HubConfig::FINISHED_TTL)
    )
}

fn full_guidance(surface: Surface) -> Vec<String> {
    vec![
        when_to_brief(surface),
        content_guidance(surface),
        authoring_guidance(),
        rich_text_guidance(),
        result_guidance(),
        retention_guidance(),
    ]
}

/// Prompt guidelines for the Pi extension's `promptGuidelines` field.
pub fn pi_guidance() -> Vec<String> {
    let mut guidance = full_guidance(TOOL_SURFACE);
    guidance.push("brief_user shows the link in Pi's UI and blocks until the user submits.".into());
    guidance.push(
        "If the user gives you a briefing id from an interrupted Pi session, tell them to run /brief-result <id> to recover it."
            .into(),
    );
    guidance
}

/// MCP server instructions text.
pub fn mcp_guidance() -> String {
    let common = full_guidance(TOOL_SURFACE).join(" ");
    format!(
        "Briefing presents complex information in a paced browser interface and returns the user's notes, inline \
         comments, decisions, and follow-up markers; free-standing notes from the Notes panel count as much as any \
         other feedback.\n\n{common}\n\nResults are returned as structuredContent. brief_user returns immediately with \
         the briefing link and a briefingId; put that exact link in your reply so the user can open it (they may be on \
         a different machine from the agent), then call await_briefing with the briefingId; it blocks until they submit \
         and returns their feedback. If await_briefing returns status \"pending\", call it again. If your harness moves \
         the call to the background, stop and wait for its completion notification; do not poll.\n\nIf a session was \
         interrupted, or the user gives you a briefingId, call await_briefing with it: it returns the stored feedback if \
         they already submitted, or reopens the briefing (status \"reopened\" with a fresh link to relay) if not."
    )
}

/// Agent-facing guidance for using the raw CLI without an MCP/Pi tool wrapper.
pub fn cli_guidance() -> String {
    let rules = full_guidance(CLI_SURFACE).into_iter().map(|rule| format!("- {rule}")).collect::<Vec<_>>().join("\n");
    format!(
        "Briefing CLI guidance\n\nDiscover the installed CLI before use:\n- `briefing --help` for available \
         commands and global flags.\n- `briefing present --help` for the current presentation flow.\n- `briefing schema` \
         for the exact presentation JSON schema.\n- `briefing status --help` and `briefing await --help` for recovery.\n\n\
         Typical flow:\n1. Finish the research first.\n2. Write a presentation JSON file that matches `briefing schema`.\n3. \
         Run `briefing present <file> --json` (or pipe JSON on stdin). It prints a ready event with the link on stderr \
         and blocks until the user submits, cancels, or the wait budget expires.\n4. If the result is completed, act only \
         on the returned feedback. If it is pending or the process was interrupted, use the briefing id with `briefing \
         await <id> --json`; `briefing status` lists recoverable briefings.\n\nBriefing rules:\n{rules}\n"
    )
}

/// Markdown for the CLI-focused Agent Skill.
pub fn skill_guidance() -> String {
    format!(
        r#"---
name: briefing
description: {SKILL_DESCRIPTION}
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
"#
    )
}

/// `6 hours`, `14 days`, `90 minutes`.
fn human(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (n, unit) = if secs.is_multiple_of(86_400) {
        (secs / 86_400, "day")
    } else if secs.is_multiple_of(3600) {
        (secs / 3600, "hour")
    } else {
        (secs / 60, "minute")
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_is_spelled_from_the_config() {
        assert_eq!(human(Duration::from_secs(6 * 3600)), "6 hours");
        assert_eq!(human(Duration::from_secs(14 * 86_400)), "14 days");
        assert_eq!(human(Duration::from_secs(60)), "1 minute");
        assert!(mcp_guidance().contains("unanswered ones for 14 days, results for 6 hours"));
    }

    #[test]
    fn skill_doc_matches_generated_guidance() {
        assert_eq!(include_str!("../skills/briefing/SKILL.md"), skill_guidance());
    }
}
