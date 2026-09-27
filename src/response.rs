//! What the browser returns: the user's notes, question answers, and inline comments.

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
pub enum QuestionStatus {
    /// The user picked at least one option or wrote an answer.
    Answered,
    /// The user left it unanswered. Still open: never read it as approval.
    Unresolved,
}

/// Every question the briefing asked, answered or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionResponse {
    pub question: String,
    /// The chunk it was asked in; absent for a question about the whole briefing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// Labels of the chosen options (several only for a multi-select question).
    pub selected: Vec<String>,
    /// The user's own words: an answer, a correction, or a question back.
    pub answer: String,
    pub status: QuestionStatus,
}

/// Structured target for comments on diagrams/charts (Mermaid node or edge, Vega chart).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Annotation {
    pub location: String,
    pub quote: String,
    pub comment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AnnotationTarget>,
}

/// Everything the user sent back from the briefing page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BriefingResponse {
    pub questions: Vec<QuestionResponse>,
    pub annotations: Vec<Annotation>,
    /// Free-standing notes written in the Notes panel; not tied to any section.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// How a wait on a briefing ended. This is the one shape every wire carries under
/// `status`: the hub agent API, the CLI's `--json` stdout, and MCP `await_briefing`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "lowercase")]
/// Serialized only; readers go through [`BriefingOutcome`], which checks it strictly.
pub enum Outcome {
    /// The user has not submitted yet; wait again.
    Pending,
    /// The user submitted their feedback.
    Completed { feedback: BriefingResponse },
    /// The user (or the agent) cancelled; `feedback` holds whatever was captured, usually nothing.
    Cancelled { feedback: BriefingResponse },
}

/// A wait result addressed to its briefing: `{"briefingId": ..., "status": ..., ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BriefingOutcome {
    pub briefing_id: String,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// The strict wire form of [`BriefingOutcome`]: `deny_unknown_fields` cannot see through
/// `flatten`, so reading goes through this and checks the status/feedback pairing itself.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OutcomeWire {
    briefing_id: String,
    status: String,
    #[serde(default)]
    feedback: Option<BriefingResponse>,
}

impl<'de> Deserialize<'de> for BriefingOutcome {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        OutcomeWire::deserialize(deserializer)?.try_into().map_err(serde::de::Error::custom)
    }
}

impl TryFrom<OutcomeWire> for BriefingOutcome {
    type Error = String;

    fn try_from(wire: OutcomeWire) -> Result<Self, String> {
        let outcome = match (wire.status.as_str(), wire.feedback) {
            ("pending", None) => Outcome::Pending,
            ("completed", Some(feedback)) => Outcome::Completed { feedback },
            ("cancelled", Some(feedback)) => Outcome::Cancelled { feedback },
            ("pending", Some(_)) => return Err("a pending outcome carries no feedback".into()),
            ("completed" | "cancelled", None) => return Err(format!("a {} outcome needs feedback", wire.status)),
            (other, _) => return Err(format!("unknown outcome status {other:?}")),
        };
        Ok(BriefingOutcome { briefing_id: wire.briefing_id, outcome })
    }
}

// ---- What the page submits ----

/// A page submission: parsed strictly, then checked against the briefing it answers.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Submission {
    #[serde(default)]
    questions: Vec<SubmittedQuestion>,
    #[serde(default)]
    annotations: Vec<Annotation>,
    #[serde(default)]
    notes: Vec<String>,
}

/// One answer as the page sends it; the hub derives its status.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubmittedQuestion {
    question: String,
    #[serde(default)]
    section: Option<String>,
    #[serde(default)]
    selected: Vec<String>,
    #[serde(default)]
    answer: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid submission: {0}")]
pub struct SubmissionError(pub String);

/// A complete submission for `presentation` that answers nothing: every question it asks,
/// unresolved, and no comments or notes. The smallest thing a page can submit.
pub fn blank_submission(presentation: &crate::content::Briefing) -> Value {
    let chunk_questions = presentation.chunks.iter().flat_map(|chunk| {
        chunk.questions.iter().map(move |q| serde_json::json!({ "question": q.question, "section": chunk.title }))
    });
    let briefing_questions = presentation.questions.iter().map(|q| serde_json::json!({ "question": q.question }));
    serde_json::json!({ "questions": chunk_questions.chain(briefing_questions).collect::<Vec<_>>(), "annotations": [], "notes": [] })
}

fn bounded(text: &str, max: usize, what: &str) -> Result<String, SubmissionError> {
    let text = text.trim();
    if text.chars().count() > max {
        return Err(SubmissionError(format!("{what} is longer than {max} characters")));
    }
    Ok(text.to_string())
}

fn required(text: &str, max: usize, what: &str) -> Result<String, SubmissionError> {
    let text = bounded(text, max, what)?;
    if text.is_empty() {
        return Err(SubmissionError(format!("{what} is empty")));
    }
    Ok(text)
}

/// Parse what the page posted for `presentation`. Anything the page would never send is
/// rejected rather than repaired: unknown fields, oversized text, empty comments or notes, a
/// question the briefing did not ask, an option it did not offer, several choices on a
/// single-choice question. A completed submission answers every question (answered or not);
/// a cancellation may carry none.
pub fn parse_submission(
    value: &Value,
    presentation: &crate::content::Briefing,
    cancelled: bool,
) -> Result<BriefingResponse, SubmissionError> {
    let submission: Submission =
        serde_json::from_value(value.clone()).map_err(|error| SubmissionError(error.to_string()))?;
    if submission.annotations.len() > MAX_ANNOTATIONS {
        return Err(SubmissionError(format!("more than {MAX_ANNOTATIONS} comments")));
    }
    if submission.notes.len() > MAX_RESULT_ITEMS {
        return Err(SubmissionError(format!("more than {MAX_RESULT_ITEMS} notes")));
    }

    let asked: Vec<(Option<&str>, &crate::content::Question)> = presentation
        .chunks
        .iter()
        .flat_map(|chunk| chunk.questions.iter().map(move |q| (Some(chunk.title.as_str()), q)))
        .chain(presentation.questions.iter().map(|q| (None, q)))
        .collect();
    let mut answers: Vec<Option<QuestionResponse>> = vec![None; asked.len()];
    for submitted in &submission.questions {
        let section = submitted.section.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let index = asked
            .iter()
            .position(|(asked_section, q)| *asked_section == section && q.question.trim() == submitted.question.trim())
            .ok_or_else(|| SubmissionError(format!("the briefing did not ask {:?}", submitted.question)))?;
        if answers[index].is_some() {
            return Err(SubmissionError(format!("{:?} is answered twice", submitted.question)));
        }
        let question = asked[index].1;
        let mut selected = Vec::new();
        for label in &submitted.selected {
            let label = label.trim();
            if !question.options.iter().any(|option| option.label.trim() == label) {
                return Err(SubmissionError(format!("{:?} has no option {label:?}", question.question)));
            }
            if selected.iter().any(|chosen: &String| chosen == label) {
                return Err(SubmissionError(format!("{label:?} is selected twice")));
            }
            selected.push(label.to_string());
        }
        if selected.len() > 1 && question.multi_select != Some(true) {
            return Err(SubmissionError(format!("{:?} takes one option", question.question)));
        }
        let answer = bounded(&submitted.answer, MAX_USER_TEXT, "an answer")?;
        let status = if selected.is_empty() && answer.is_empty() {
            QuestionStatus::Unresolved
        } else {
            QuestionStatus::Answered
        };
        answers[index] = Some(QuestionResponse {
            question: question.question.clone(),
            section: asked[index].0.map(str::to_string),
            selected,
            answer,
            status,
        });
    }
    let questions = if cancelled {
        answers.into_iter().flatten().collect()
    } else {
        answers
            .into_iter()
            .zip(&asked)
            .map(|(answer, (_, question))| {
                answer.ok_or_else(|| SubmissionError(format!("{:?} is missing from the submission", question.question)))
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    let annotations = submission
        .annotations
        .iter()
        .map(|a| {
            let target = match &a.target {
                None => None,
                Some(t) => {
                    let field = |v: &Option<String>| -> Result<Option<String>, SubmissionError> {
                        v.as_deref()
                            .map(|v| bounded(v, MAX_ANNOTATION_TARGET_FIELD, "a comment target field"))
                            .transpose()
                            .map(|v| v.filter(|v| !v.is_empty()))
                    };
                    Some(AnnotationTarget {
                        section: field(&t.section)?,
                        content_type: field(&t.content_type)?,
                        target_type: field(&t.target_type)?,
                        target_id: field(&t.target_id)?,
                        target_label: field(&t.target_label)?,
                        source_excerpt: field(&t.source_excerpt)?,
                    })
                }
            };
            Ok(Annotation {
                location: required(&a.location, MAX_ANNOTATION_LOCATION, "a comment's location")?,
                quote: required(&a.quote, MAX_ANNOTATION_QUOTE, "a comment's quote")?,
                comment: required(&a.comment, MAX_ANNOTATION_COMMENT, "a comment")?,
                target,
            })
        })
        .collect::<Result<Vec<_>, SubmissionError>>()?;
    let notes = submission.notes.iter().map(|n| required(n, MAX_USER_TEXT, "a note")).collect::<Result<Vec<_>, _>>()?;
    Ok(BriefingResponse { questions, annotations, notes })
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
    pub answered: usize,
    pub unresolved: usize,
    pub comments: usize,
    pub notes: usize,
}

impl std::fmt::Display for FeedbackCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} answered questions, {} unresolved, {} comments, {} notes",
            self.answered, self.unresolved, self.comments, self.notes
        )
    }
}

impl BriefingResponse {
    pub fn counts(&self) -> FeedbackCounts {
        FeedbackCounts {
            answered: self.questions.iter().filter(|q| q.status == QuestionStatus::Answered).count(),
            unresolved: self.questions.iter().filter(|q| q.status == QuestionStatus::Unresolved).count(),
            comments: self.annotations.len(),
            notes: self.notes.len(),
        }
    }

    /// The per-item lines shown to the model (no header).
    pub fn detail_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for question in &self.questions {
            let place = question.section.as_deref().unwrap_or("whole briefing");
            let mut entry = format!("Question ({place}): {}", question.question);
            if !question.selected.is_empty() {
                entry.push_str(&format!("\nSelected: {}", question.selected.join(", ")));
            }
            if !question.answer.is_empty() {
                entry.push_str(&format!("\nAnswer: {}", question.answer));
            }
            if question.status == QuestionStatus::Unresolved {
                entry.push_str("\nUnresolved: left unanswered; still open, not approval.");
            }
            lines.push(entry);
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
        for note in &self.notes {
            lines.push(format!("Note: {note}"));
        }
        lines
    }
}

impl Outcome {
    /// An empty, cancelled result.
    pub fn cancelled() -> Self {
        Outcome::Cancelled { feedback: BriefingResponse::default() }
    }

    /// The text handed back to the model as the tool result.
    pub fn format_text(&self) -> String {
        match self {
            Outcome::Pending => "The briefing is still open; the user has not submitted.".to_string(),
            Outcome::Cancelled { .. } => "The user cancelled the briefing without submitting feedback.".to_string(),
            Outcome::Completed { feedback } => {
                let mut lines = vec!["User completed the briefing.".to_string()];
                lines.extend(feedback.detail_lines());
                if lines.len() == 1 {
                    lines.push("The user returned no notes, answers, or comments.".to_string());
                }
                lines.join("\n")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn briefing() -> crate::content::Briefing {
        serde_json::from_value(json!({
            "title": "T", "goal": "G",
            "chunks": [{ "title": "First", "mainPoint": "M", "questions": [
                { "question": "Pick?", "options": [{ "label": "A" }, { "label": "B" }] },
                { "question": "Any?", "multiSelect": true, "options": [{ "label": "X" }, { "label": "Y" }] }
            ] }],
            "questions": [{ "question": "Open?" }]
        }))
        .unwrap()
    }

    fn submission(questions: Value) -> Value {
        json!({
            "questions": questions,
            "annotations": [{ "location": "First", "quote": " q ", "comment": "c", "target": { "contentType": "mermaid", "targetId": "n1" } }],
            "notes": ["  a thought "]
        })
    }

    fn all_answered() -> Value {
        json!([
            { "question": "Pick?", "section": "First", "selected": ["A"], "answer": "" },
            { "question": "Any?", "section": "First", "selected": ["X", "Y"], "answer": "" },
            { "question": "Open?", "selected": [], "answer": "  " }
        ])
    }

    #[test]
    fn parses_a_submission_against_its_briefing() {
        let result = parse_submission(&submission(all_answered()), &briefing(), false).unwrap();
        assert_eq!(result.questions[0].selected, vec!["A"]);
        assert_eq!(result.questions[0].status, QuestionStatus::Answered);
        assert_eq!(result.questions[1].selected, vec!["X", "Y"]);
        // Status comes from what was answered; whitespace is not an answer.
        assert_eq!(result.questions[2].status, QuestionStatus::Unresolved);
        assert_eq!(result.questions[2].section, None);
        assert_eq!(result.annotations[0].quote, "q");
        assert_eq!(result.notes, vec!["a thought"]);
        assert_eq!(result.counts(), FeedbackCounts { answered: 2, unresolved: 1, comments: 1, notes: 1 });

        let text = Outcome::Completed { feedback: result }.format_text();
        assert!(text.contains("Target: content=mermaid, id=n1"));
        assert!(text.contains("Question (First): Pick?\nSelected: A"));
        assert!(text.contains("Question (whole briefing): Open?\nUnresolved"));
    }

    #[test]
    fn rejects_what_the_page_never_sends() {
        let reject = |value: Value, cancelled: bool| parse_submission(&value, &briefing(), cancelled).unwrap_err().0;
        let mut questions = all_answered();

        assert!(reject(json!({ "questions": all_answered(), "overallNote": "x" }), false).contains("unknown field"));
        questions[0]["extra"] = json!(1);
        assert!(reject(submission(questions), false).contains("unknown field"));

        let with = |index: usize, key: &str, value: Value| {
            let mut questions = all_answered();
            questions[index][key] = value;
            submission(questions)
        };
        assert!(reject(with(0, "question", json!("Not asked?")), false).contains("did not ask"));
        assert!(reject(with(2, "section", json!("First")), false).contains("did not ask"));
        assert!(reject(with(0, "selected", json!(["Z"])), false).contains("no option"));
        assert!(reject(with(0, "selected", json!(["A", "B"])), false).contains("takes one option"));
        assert!(reject(with(1, "selected", json!(["X", "X"])), false).contains("selected twice"));

        let mut twice = all_answered();
        let first = twice[0].clone();
        twice.as_array_mut().unwrap().push(first);
        assert!(reject(submission(twice), false).contains("answered twice"));
        let mut missing = all_answered();
        missing.as_array_mut().unwrap().pop();
        assert!(reject(submission(missing), false).contains("missing"));

        let mut blank = submission(all_answered());
        blank["annotations"][0]["comment"] = json!("  ");
        assert!(reject(blank, false).contains("empty"));
        let mut huge = submission(all_answered());
        huge["notes"] = json!(["x".repeat(MAX_USER_TEXT + 1)]);
        assert!(reject(huge, false).contains("longer than"));
    }

    #[test]
    fn a_cancellation_may_carry_nothing() {
        assert_eq!(parse_submission(&json!({}), &briefing(), true).unwrap(), BriefingResponse::default());
        assert!(parse_submission(&json!({}), &briefing(), false).is_err());
        assert!(Outcome::cancelled().format_text().contains("cancelled"));
        let empty = BriefingResponse::default();
        assert_eq!(empty.counts(), FeedbackCounts { answered: 0, unresolved: 0, comments: 0, notes: 0 });
        assert!(Outcome::Completed { feedback: empty }.format_text().contains("returned no notes"));
    }

    #[test]
    fn outcome_wire_shape() {
        let pending = BriefingOutcome { briefing_id: "b1".into(), outcome: Outcome::Pending };
        assert_eq!(serde_json::to_value(&pending).unwrap(), json!({"briefingId": "b1", "status": "pending"}));
        let done = BriefingOutcome { briefing_id: "b1".into(), outcome: Outcome::cancelled() };
        let value = serde_json::to_value(&done).unwrap();
        assert_eq!(value["status"], "cancelled");
        assert_eq!(value["feedback"]["annotations"], json!([]));
        assert!(value.get("cancelled").is_none());
        // Round trips, strictly: unknown keys, and a status that disagrees with its feedback, fail.
        assert_eq!(serde_json::from_value::<BriefingOutcome>(value.clone()).unwrap(), done);
        let mut extra = value.clone();
        extra["bogus"] = json!(1);
        assert!(serde_json::from_value::<BriefingOutcome>(extra).is_err());
        assert!(serde_json::from_value::<BriefingOutcome>(json!({"briefingId": "b1", "status": "completed"})).is_err());
        assert!(
            serde_json::from_value::<BriefingOutcome>(json!({"briefingId": "b1", "status": "pending", "feedback": {"questions": [], "annotations": [], "notes": []}}))
                .is_err()
        );
    }
}
