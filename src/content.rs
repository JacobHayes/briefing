//! Briefing input contract: the JSON the model authors for `brief_user`.
//!
//! Field names are camelCase on the wire so the schema matches the original Pi
//! extension and the browser page.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_CHUNKS: usize = 10;
pub const MAX_KEY_POINTS: usize = 8;
pub const MAX_REMEMBER: usize = 4;
pub const MAX_SOURCES: usize = 6;
pub const MAX_KEY_CONTEXT: usize = 6;
pub const MAX_OPEN_QUESTIONS: usize = 5;
pub const MAX_DECISIONS: usize = 6;
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 4;
pub const MAX_TRADEOFFS: usize = 4;
pub const MAX_PRESENTATION_BYTES: usize = 1024 * 1024;
pub const MAX_FENCED_SOURCE_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Research,
    Explanation,
    Decision,
    Briefing,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    /// Short source label.
    pub label: String,
    /// Absolute http(s) source URL.
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Chunk {
    /// Short descriptive title for this semantic chunk.
    pub title: String,
    /// One sentence explaining why this chunk matters to the user's goal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// The single main claim or idea. Keep it concise and self-contained.
    pub main_point: String,
    /// Aim for 3-5 supporting points; use up to 8 only when keeping one semantic chunk coherent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 8))]
    pub key_points: Option<Vec<String>>,
    /// Optional supporting explanation. Use for focused, clear, substantive context that belongs on this section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// 1-4 memory anchors needed later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4))]
    pub remember: Option<Vec<String>>,
    /// Optional question or response prompt for the user; when present, the response area opens by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// Sources directly supporting this chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 6))]
    pub sources: Option<Vec<Source>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DecisionOption {
    /// Short option label.
    pub label: String,
    /// What this option means and when it fits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Concrete implications or tradeoffs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4))]
    pub tradeoffs: Option<Vec<String>>,
    /// Mark true only when recommending this option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    /// The concrete decision the user needs to make.
    pub question: String,
    /// Only the context needed to make this decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Require a selection or written guidance before continuing. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// 2-4 meaningfully distinct options.
    #[schemars(length(min = 2, max = 4))]
    pub options: Vec<DecisionOption>,
}

/// Optional Context panel content: stable context, running summary, and open questions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tray {
    /// Stable context the user should not have to remember.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 6))]
    pub key_context: Option<Vec<String>>,
    /// Compact synthesis that remains visible throughout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_summary: Option<String>,
    /// Unresolved questions to keep visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 5))]
    pub open_questions: Option<Vec<String>>,
}

/// A paced browser briefing: semantic chunks in dependency order, optional context panel, and decisions. Every prose field accepts Markdown (GFM tables, fenced code with a language tag, ```mermaid fences for flows/architecture/state, ```vega-lite fences for charts); use them only when they clarify.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Briefing {
    /// Short title for the briefing.
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// The user's current goal or the outcome this presentation supports.
    pub goal: String,
    /// 1-10 semantic chunks in the order the user should encounter them.
    #[schemars(length(min = 1, max = 10))]
    pub chunks: Vec<Chunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tray: Option<Tray>,
    /// 0-6 decisions shown after the explanatory chunks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 6))]
    pub decisions: Vec<Decision>,
    /// Short heading for the final briefing screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_prompt: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ValidationError(pub String);

fn require_text(value: Option<&str>, label: &str) -> Result<(), ValidationError> {
    match value {
        Some(text) if !text.trim().is_empty() => Ok(()),
        _ => Err(ValidationError(format!("{label} cannot be empty"))),
    }
}

fn is_vega_lite_fence(language: &str) -> bool {
    matches!(language, "vega-lite" | "vegalite" | "vl")
}

fn text_fields(input: &Briefing) -> Vec<(String, &str)> {
    let mut fields: Vec<(String, &str)> = vec![("goal".into(), input.goal.as_str())];
    for (index, chunk) in input.chunks.iter().enumerate() {
        let prefix = format!("chunk {}", index + 1);
        if let Some(purpose) = &chunk.purpose {
            fields.push((format!("{prefix} purpose"), purpose));
        }
        fields.push((format!("{prefix} mainPoint"), &chunk.main_point));
        if let Some(details) = &chunk.details {
            fields.push((format!("{prefix} details"), details));
        }
        if let Some(checkpoint) = &chunk.checkpoint {
            fields.push((format!("{prefix} checkpoint"), checkpoint));
        }
        for (i, point) in chunk.key_points.iter().flatten().enumerate() {
            fields.push((format!("{prefix} keyPoint {}", i + 1), point));
        }
        for (i, item) in chunk.remember.iter().flatten().enumerate() {
            fields.push((format!("{prefix} remember {}", i + 1), item));
        }
    }
    if let Some(tray) = &input.tray {
        for (i, item) in tray.key_context.iter().flatten().enumerate() {
            fields.push((format!("context keyContext {}", i + 1), item));
        }
        if let Some(summary) = &tray.running_summary {
            fields.push(("context runningSummary".into(), summary));
        }
        for (i, item) in tray.open_questions.iter().flatten().enumerate() {
            fields.push((format!("context openQuestion {}", i + 1), item));
        }
    }
    for (index, decision) in input.decisions.iter().enumerate() {
        let prefix = format!("decision {}", index + 1);
        if let Some(context) = &decision.context {
            fields.push((format!("{prefix} context"), context));
        }
        for (oi, option) in decision.options.iter().enumerate() {
            if let Some(description) = &option.description {
                fields.push((format!("{prefix} option {} description", oi + 1), description));
            }
            for (ti, tradeoff) in option.tradeoffs.iter().flatten().enumerate() {
                fields.push((format!("{prefix} option {} tradeoff {}", oi + 1, ti + 1), tradeoff));
            }
        }
    }
    if let Some(prompt) = &input.completion_prompt {
        fields.push(("completionPrompt".into(), prompt));
    }
    fields
}

/// Fenced code blocks found in a Markdown string: `(language, source)`.
pub fn fenced_code_blocks(markdown: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut current: Option<(String, String)> = None;
    for event in Parser::new_ext(markdown, Options::all()) {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match &kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_ascii_lowercase(),
                    CodeBlockKind::Indented => String::new(),
                };
                current = Some((language, String::new()));
            }
            Event::Text(text) => {
                if let Some((_, source)) = current.as_mut() {
                    source.push_str(&text);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(block) = current.take() {
                    blocks.push(block);
                }
            }
            _ => {}
        }
    }
    blocks
}

fn validate_rich_content(input: &Briefing) -> Result<(), ValidationError> {
    for (label, value) in text_fields(input) {
        for (index, (language, source)) in fenced_code_blocks(value).iter().enumerate() {
            let block = index + 1;
            if source.len() > MAX_FENCED_SOURCE_BYTES {
                return Err(ValidationError(format!(
                    "{label} code block {block} exceeds {}KB",
                    MAX_FENCED_SOURCE_BYTES / 1024
                )));
            }
            if is_vega_lite_fence(language) {
                let spec: serde_json::Value = serde_json::from_str(source).map_err(|error| {
                    ValidationError(format!("{label} Vega-Lite block {block} is not valid JSON: {error}"))
                })?;
                if !spec.is_object() {
                    return Err(ValidationError(format!("{label} Vega-Lite block {block} must be a JSON object")));
                }
            }
        }
    }
    Ok(())
}

/// Validate a presentation and return a normalized copy (trimmed title/goal).
pub fn validate(input: &Briefing) -> Result<Briefing, ValidationError> {
    let serialized = serde_json::to_vec(input).map_err(|error| ValidationError(error.to_string()))?;
    if serialized.len() > MAX_PRESENTATION_BYTES {
        return Err(ValidationError(format!("brief_user input exceeds {}KB", MAX_PRESENTATION_BYTES / 1024)));
    }
    require_text(Some(&input.title), "title")?;
    require_text(Some(&input.goal), "goal")?;
    if input.chunks.is_empty() || input.chunks.len() > MAX_CHUNKS {
        return Err(ValidationError(format!("brief_user requires 1-{MAX_CHUNKS} chunks")));
    }

    for (index, chunk) in input.chunks.iter().enumerate() {
        let n = index + 1;
        require_text(Some(&chunk.title), &format!("chunk {n} title"))?;
        require_text(Some(&chunk.main_point), &format!("chunk {n} mainPoint"))?;
        let too_many = |len: usize, max: usize, what: &str| {
            if len > max { Err(ValidationError(format!("chunk {n} has more than {max} {what}"))) } else { Ok(()) }
        };
        too_many(chunk.key_points.as_ref().map_or(0, Vec::len), MAX_KEY_POINTS, "keyPoints")?;
        too_many(chunk.remember.as_ref().map_or(0, Vec::len), MAX_REMEMBER, "remember anchors")?;
        too_many(chunk.sources.as_ref().map_or(0, Vec::len), MAX_SOURCES, "sources")?;
        for source in chunk.sources.iter().flatten() {
            require_text(Some(&source.label), &format!("chunk {n} source label"))?;
            let url = url::Url::parse(&source.url)
                .map_err(|_| ValidationError(format!("chunk {n} source URL is invalid: {}", source.url)))?;
            if url.scheme() != "http" && url.scheme() != "https" {
                return Err(ValidationError(format!("chunk {n} source URL must use http or https")));
            }
        }
    }

    if let Some(tray) = &input.tray {
        if tray.key_context.as_ref().map_or(0, Vec::len) > MAX_KEY_CONTEXT {
            return Err(ValidationError(format!("tray has more than {MAX_KEY_CONTEXT} keyContext items")));
        }
        if tray.open_questions.as_ref().map_or(0, Vec::len) > MAX_OPEN_QUESTIONS {
            return Err(ValidationError(format!("tray has more than {MAX_OPEN_QUESTIONS} openQuestions")));
        }
    }

    if input.decisions.len() > MAX_DECISIONS {
        return Err(ValidationError(format!("brief_user supports at most {MAX_DECISIONS} decisions")));
    }
    for (index, decision) in input.decisions.iter().enumerate() {
        let n = index + 1;
        require_text(Some(&decision.question), &format!("decision {n} question"))?;
        if decision.options.len() < MIN_OPTIONS || decision.options.len() > MAX_OPTIONS {
            return Err(ValidationError(format!("decision {n} requires {MIN_OPTIONS}-{MAX_OPTIONS} options")));
        }
        let mut labels = std::collections::HashSet::new();
        for option in &decision.options {
            require_text(Some(&option.label), &format!("decision {n} option label"))?;
            if !labels.insert(option.label.trim().to_lowercase()) {
                return Err(ValidationError(format!("decision {n} has duplicate option: {}", option.label)));
            }
            if option.tradeoffs.as_ref().map_or(0, Vec::len) > MAX_TRADEOFFS {
                return Err(ValidationError(format!(
                    "decision {n} option {} has more than {MAX_TRADEOFFS} tradeoffs",
                    option.label
                )));
            }
        }
        if decision.options.iter().filter(|o| o.recommended == Some(true)).count() > 1 {
            return Err(ValidationError(format!("decision {n} has more than one recommended option")));
        }
    }

    validate_rich_content(input)?;

    let mut normalized = input.clone();
    normalized.title = input.title.trim().to_string();
    normalized.goal = input.goal.trim().to_string();
    Ok(normalized)
}

/// JSON Schema for the `brief_user` tool input.
pub fn json_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Briefing)).expect("schema serializes")
}

/// The bundled demo presentation.
pub fn demo() -> Briefing {
    serde_json::from_str(include_str!("../assets/demo.json")).expect("demo presentation is valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> Briefing {
        Briefing {
            title: "T".into(),
            mode: None,
            goal: "G".into(),
            chunks: vec![Chunk {
                title: "C".into(),
                purpose: None,
                main_point: "M".into(),
                key_points: None,
                details: None,
                remember: None,
                checkpoint: None,
                sources: None,
            }],
            tray: None,
            decisions: vec![],
            completion_prompt: None,
        }
    }

    #[test]
    fn accepts_minimal_and_demo() {
        validate(&minimal()).unwrap();
        validate(&demo()).unwrap();
    }

    #[test]
    fn rejects_blank_title_and_bad_source() {
        let mut p = minimal();
        p.title = "   ".into();
        assert!(validate(&p).unwrap_err().0.contains("title"));

        let mut p = minimal();
        p.chunks[0].sources = Some(vec![Source { label: "x".into(), url: "ftp://example.com".into() }]);
        assert!(validate(&p).unwrap_err().0.contains("http"));
    }

    #[test]
    fn rejects_duplicate_or_double_recommended_options() {
        let opt = |label: &str, rec: bool| DecisionOption {
            label: label.into(),
            description: None,
            tradeoffs: None,
            recommended: Some(rec),
        };
        let mut p = minimal();
        p.decisions = vec![Decision {
            question: "Q".into(),
            context: None,
            required: None,
            options: vec![opt("A", false), opt("a", false)],
        }];
        assert!(validate(&p).unwrap_err().0.contains("duplicate"));
        p.decisions = vec![Decision {
            question: "Q".into(),
            context: None,
            required: None,
            options: vec![opt("A", true), opt("B", true)],
        }];
        assert!(validate(&p).unwrap_err().0.contains("recommended"));
    }

    #[test]
    fn validates_vega_lite_fences() {
        let mut p = minimal();
        p.chunks[0].details = Some("```vega-lite\n{not json\n```".into());
        assert!(validate(&p).unwrap_err().0.contains("Vega-Lite"));
        p.chunks[0].details = Some("```vl\n{\"mark\":\"bar\"}\n```\n\n```ts\nconst x = 1;\n```".into());
        validate(&p).unwrap();
        assert_eq!(fenced_code_blocks(p.chunks[0].details.as_ref().unwrap()).len(), 2);
    }

    #[test]
    fn schema_uses_camel_case_and_limits() {
        let schema = json_schema();
        let props = &schema["properties"];
        assert!(props.get("completionPrompt").is_some());
        assert_eq!(schema["properties"]["chunks"]["maxItems"], 10);
        assert_eq!(schema["required"].as_array().unwrap().len(), 3);
    }
}
