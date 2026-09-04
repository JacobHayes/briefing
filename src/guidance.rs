//! Model-facing guidance, kept in one place so the MCP instructions, the Pi extension's
//! prompt guidelines, and the `briefing guidelines` command cannot drift apart.

use std::time::Duration;

use crate::content::{MAX_CHUNKS, MAX_KEY_POINTS, MAX_OPTIONS, MIN_OPTIONS};
use crate::hub::HubConfig;

/// Rules that apply in every harness, one sentence or two each.
pub fn shared() -> Vec<String> {
    vec![
        "Use brief_user proactively whenever an answer crosses a complexity threshold: substantial research with \
         dependent findings, multi-part explanations, or decisions that need context. Keep short and simple answers \
         as normal chat."
            .into(),
        format!(
            "Finish the research and reasoning first, then call brief_user once with 3-8 semantic chunks (at most \
             {MAX_CHUNKS}) in dependency order: one main idea per chunk, 3-5 keyPoints each (at most {MAX_KEY_POINTS}), \
             focused details, stable context in tray, and {MIN_OPTIONS}-{MAX_OPTIONS} distinct decision options with \
             the recommended one first and marked."
        ),
        "Text fields accept Markdown, GFM tables, fenced code with a language tag, Mermaid fences, and Vega-Lite \
         fences; use them only when they clarify."
            .into(),
        "After the feedback arrives, respond only to it; do not repeat the presentation as a chat message.".into(),
        format!(
            "Briefings outlive the process that created them (unanswered ones for {}, results for {}).",
            human(HubConfig::ACTIVE_TTL),
            human(HubConfig::FINISHED_TTL)
        ),
    ]
}

/// The MCP server's `instructions` string.
pub fn mcp_instructions() -> String {
    let shared = shared();
    format!(
        "Briefing presents complex information in a paced browser interface and returns the user's notes, inline \
         comments, decisions, and follow-up markers.\n\n{}\n\nResults are returned as structuredContent. brief_user \
         returns immediately with the briefing link and a briefingId; put that exact link in your reply so the user \
         can open it (they may be on a different machine from the agent), then call await_briefing with the \
         briefingId; it blocks until they submit and returns their feedback. If await_briefing returns status \
         \"pending\", call it again. If your harness moves the call to the background, stop and wait for its \
         completion notification; do not poll. {}\n\n{} If a session was interrupted, or the user gives you a \
         briefingId, call await_briefing with it: it returns the stored feedback if they already submitted, or reopens \
         the briefing (status \"reopened\" with a fresh link to relay) if not.",
        shared[..3].join(" "),
        shared[3],
        shared[4],
    )
}

/// What `briefing guidelines` prints: the shared rules plus a note per harness shape.
pub fn json() -> serde_json::Value {
    serde_json::json!({
        "shared": shared(),
        "mcp": mcp_instructions(),
    })
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
        let text = mcp_instructions();
        assert!(text.contains("unanswered ones for 14 days, results for 6 hours"));
        assert!(text.contains("brief_user"));
        assert_eq!(json()["shared"].as_array().unwrap().len(), 5);
    }

    /// The human-readable skill doc must agree with the numbers the binary states.
    #[test]
    fn skill_doc_matches() {
        let skill = include_str!("../skills/briefing/SKILL.md");
        assert!(skill.contains("3-8 (max 10)"));
        assert!(skill.contains("unanswered for 14 days, results for 6 hours"), "SKILL.md retention text is stale");
    }
}
