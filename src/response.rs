//! What the browser returns: the user's notes, decisions, and inline comments.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_RESULT_ITEMS: usize = 100;
pub const MAX_USER_TEXT: usize = 20_000;
pub const MAX_ANNOTATIONS: usize = 500;
pub const MAX_ANNOTATION_QUOTE: usize = 2_000;
pub const MAX_ANNOTATION_COMMENT: usize = 4_000;
pub const MAX_ANNOTATION_LOCATION: usize = 300;
pub const MAX_ANNOTATION_TARGET_FIELD: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChunkStatus {
    /// Flagged for follow-up.
    Revisit,
    Unmarked,
}

/// One section: the user's checkpoint answer, free note, and whether they flagged it for follow-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChunkResponse {
    pub title: String,
    pub status: ChunkStatus,
    pub checkpoint: String,
    pub note: String,
}

impl ChunkResponse {
    /// The user wrote something or flagged the section.
    pub fn is_substantive(&self) -> bool {
        self.status == ChunkStatus::Revisit || !self.note.is_empty() || !self.checkpoint.is_empty()
    }
}

/// One decision: the selected option label (empty if none) and any written guidance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DecisionResponse {
    pub question: String,
    pub selected: String,
    pub note: String,
}

impl DecisionResponse {
    pub fn is_substantive(&self) -> bool {
        !self.selected.is_empty() || !self.note.is_empty()
    }
}

/// Structured target for comments on diagrams/charts (Mermaid node or edge, Vega chart).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AnnotationTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_excerpt: Option<String>,
}

/// An inline comment: where it was made, the exact quoted passage, and the comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Annotation {
    pub location: String,
    pub quote: String,
    pub comment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AnnotationTarget>,
}

/// Everything the user sent back from the briefing page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BriefingResponse {
    pub cancelled: bool,
    pub chunks: Vec<ChunkResponse>,
    pub decisions: Vec<DecisionResponse>,
    pub annotations: Vec<Annotation>,
    pub overall_note: String,
}

fn trimmed(value: Option<&Value>, max: usize) -> String {
    let Some(Value::String(text)) = value else {
        return String::new();
    };
    let cut: String = text.chars().take(max).collect();
    cut.trim().to_string()
}

fn non_empty(text: String) -> Option<String> {
    if text.is_empty() { None } else { Some(text) }
}

fn parse_target(value: Option<&Value>) -> Option<AnnotationTarget> {
    let row = value?.as_object()?;
    let field = |name: &str| non_empty(trimmed(row.get(name), MAX_ANNOTATION_TARGET_FIELD));
    let target = AnnotationTarget {
        section: field("section"),
        content_type: field("contentType"),
        target_type: field("targetType"),
        target_id: field("targetId"),
        target_label: field("targetLabel"),
        source_excerpt: field("sourceExcerpt"),
    };
    (target != AnnotationTarget::default()).then_some(target)
}

fn parse_annotations(value: Option<&Value>) -> Vec<Annotation> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .take(MAX_ANNOTATIONS)
        .filter_map(|item| {
            let row = item.as_object()?;
            let quote = trimmed(row.get("quote"), MAX_ANNOTATION_QUOTE);
            let comment = trimmed(row.get("comment"), MAX_ANNOTATION_COMMENT);
            if quote.is_empty() || comment.is_empty() {
                return None;
            }
            let location = non_empty(trimmed(row.get("location"), MAX_ANNOTATION_LOCATION))
                .unwrap_or_else(|| "Unspecified section".to_string());
            Some(Annotation { location, quote, comment, target: parse_target(row.get("target")) })
        })
        .collect()
}

/// Clamp and normalize whatever the browser posted into a `BriefingResponse`.
pub fn parse_browser_result(value: &Value, cancelled: bool) -> BriefingResponse {
    let empty = serde_json::Map::new();
    let input = value.as_object().unwrap_or(&empty);
    let rows = |name: &str| -> Vec<&Value> {
        match input.get(name) {
            Some(Value::Array(items)) => items.iter().take(MAX_RESULT_ITEMS).collect(),
            _ => Vec::new(),
        }
    };

    let chunks = rows("chunks")
        .into_iter()
        .filter_map(|item| {
            let row = item.as_object()?;
            let title = non_empty(trimmed(row.get("title"), 500))?;
            let status = match row.get("status").and_then(Value::as_str) {
                Some("revisit") => ChunkStatus::Revisit,
                _ => ChunkStatus::Unmarked,
            };
            Some(ChunkResponse {
                title,
                status,
                checkpoint: trimmed(row.get("checkpoint"), MAX_USER_TEXT),
                note: trimmed(row.get("note"), MAX_USER_TEXT),
            })
        })
        .collect();

    let decisions = rows("decisions")
        .into_iter()
        .filter_map(|item| {
            let row = item.as_object()?;
            let question = non_empty(trimmed(row.get("question"), 1_000))?;
            Some(DecisionResponse {
                question,
                selected: trimmed(row.get("selected"), 1_000),
                note: trimmed(row.get("note"), MAX_USER_TEXT),
            })
        })
        .collect();

    BriefingResponse {
        cancelled,
        chunks,
        decisions,
        annotations: parse_annotations(input.get("annotations")),
        overall_note: trimmed(input.get("overallNote"), MAX_USER_TEXT),
    }
}

fn format_target(target: &AnnotationTarget) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(v) = &target.content_type {
        parts.push(format!("content={v}"));
    }
    if let Some(v) = &target.target_type {
        parts.push(format!("target={v}"));
    }
    if let Some(v) = &target.target_id {
        parts.push(format!("id={v}"));
    }
    if let Some(v) = &target.target_label {
        parts.push(format!("label={v}"));
    }
    if parts.is_empty() && target.source_excerpt.is_none() {
        return None;
    }
    let mut out = parts.join(", ");
    if let Some(excerpt) = &target.source_excerpt {
        if !out.is_empty() {
            out.push_str("; ");
        }
        out.push_str(&format!("source={excerpt}"));
    }
    Some(out)
}

/// How much the user sent back, for one-line summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedbackCounts {
    pub decisions: usize,
    pub sections: usize,
    pub comments: usize,
}

impl std::fmt::Display for FeedbackCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} decisions, {} section responses, {} comments", self.decisions, self.sections, self.comments)
    }
}

impl BriefingResponse {
    /// An empty, cancelled result.
    pub fn cancelled() -> Self {
        Self { cancelled: true, ..Self::default() }
    }

    pub fn counts(&self) -> FeedbackCounts {
        FeedbackCounts {
            decisions: self.decisions.iter().filter(|d| d.is_substantive()).count(),
            sections: self.chunks.iter().filter(|c| c.is_substantive()).count(),
            comments: self.annotations.len(),
        }
    }

    /// The per-item lines shown to the model (no header).
    pub fn detail_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let revisit: Vec<&str> =
            self.chunks.iter().filter(|c| c.status == ChunkStatus::Revisit).map(|c| c.title.as_str()).collect();
        if !revisit.is_empty() {
            lines.push(format!("Sections flagged for follow-up: {}", revisit.join(", ")));
        }
        for chunk in &self.chunks {
            if !chunk.checkpoint.is_empty() {
                lines.push(format!("Checkpoint - {}: {}", chunk.title, chunk.checkpoint));
            }
            if !chunk.note.is_empty() {
                lines.push(format!("Note - {}: {}", chunk.title, chunk.note));
            }
        }
        for decision in &self.decisions {
            let selected =
                if decision.selected.is_empty() { "No predefined option selected" } else { decision.selected.as_str() };
            lines.push(format!("Decision - {}: {}", decision.question, selected));
            if !decision.note.is_empty() {
                lines.push(format!("Decision guidance: {}", decision.note));
            }
        }
        for annotation in &self.annotations {
            let quote: Vec<String> = annotation.quote.lines().map(|l| format!("> {l}")).collect();
            let target = annotation.target.as_ref().and_then(format_target);
            let mut entry = format!("Comment - {}:", annotation.location);
            if let Some(target) = target {
                entry.push_str(&format!("\nTarget: {target}"));
            }
            entry.push('\n');
            entry.push_str(&quote.join("\n"));
            entry.push_str(&format!("\nComment: {}", annotation.comment));
            lines.push(entry);
        }
        if !self.overall_note.is_empty() {
            lines.push(format!("Overall response: {}", self.overall_note));
        }
        lines
    }

    /// The text handed back to the model as the tool result.
    pub fn format_text(&self) -> String {
        if self.cancelled {
            return "The user cancelled the briefing without submitting feedback.".to_string();
        }
        let mut lines = vec!["User completed the briefing.".to_string()];
        lines.extend(self.detail_lines());
        if lines.len() == 1 {
            lines.push("The user returned no notes, checkpoints, decisions, or comments.".to_string());
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_and_clamps_browser_payload() {
        let result = parse_browser_result(
            &json!({
                "chunks": [
                    {"title": "First", "status": "revisit", "checkpoint": "  answer ", "note": ""},
                    {"title": "", "status": "understood"},
                    {"title": "Second", "status": "bogus"}
                ],
                "decisions": [{"question": "Q", "selected": "A", "note": "why"}],
                "annotations": [
                    {"location": "First", "quote": "q", "comment": "c", "target": {"contentType": "mermaid", "targetId": "n1", "bogus": "x"}},
                    {"location": "x", "quote": "", "comment": "no quote"}
                ],
                "overallNote": "done"
            }),
            false,
        );
        assert_eq!(result.chunks.len(), 2);
        assert_eq!(result.chunks[0].status, ChunkStatus::Revisit);
        assert_eq!(result.chunks[0].checkpoint, "answer");
        assert_eq!(result.chunks[1].status, ChunkStatus::Unmarked);
        assert_eq!(result.decisions[0].selected, "A");
        assert_eq!(result.annotations.len(), 1);
        let target = result.annotations[0].target.as_ref().unwrap();
        assert_eq!(target.content_type.as_deref(), Some("mermaid"));
        assert_eq!(result.counts().to_string(), "1 decisions, 1 section responses, 1 comments");

        let text = result.format_text();
        assert!(text.contains("Sections flagged for follow-up: First"));
        assert!(text.contains("Target: content=mermaid, id=n1"));
        assert!(text.contains("> q\nComment: c"));
        assert!(text.contains("Overall response: done"));
    }

    #[test]
    fn empty_and_cancelled_results() {
        let empty = parse_browser_result(&json!({}), false);
        assert_eq!(empty.counts(), FeedbackCounts { decisions: 0, sections: 0, comments: 0 });
        assert!(empty.format_text().contains("returned no notes"));
        assert_eq!(parse_browser_result(&json!(null), true), BriefingResponse::cancelled());
        assert!(BriefingResponse::cancelled().format_text().contains("cancelled"));
    }
}
