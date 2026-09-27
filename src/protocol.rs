//! The version of the wire protocol between briefing clients and a hub, and compatibility
//! with the previous version.
//!
//! Every agent-API and page-API request carries a `Briefing-Protocol` header and every
//! response from a hub carries the hub's. A request without one comes from a client released
//! before versioning, which speaks protocol 1. A hub serves its own protocol and the one
//! before it, translating requests and responses for the older one; anything else gets a 426
//! naming both versions. A client refuses a hub whose protocol differs from its own.
//!
//! Protocol 2 replaced checkpoints and decisions with questions and reduced feedback to
//! questions, comments, and notes (see [`crate::migrate`] for the same change to stored files).

use serde_json::{Value, json};

use crate::migrate;

pub const PROTOCOL: u32 = 2;

/// The oldest protocol a hub still translates for.
pub const OLDEST_SUPPORTED: u32 = PROTOCOL - 1;

/// Version assumed when a request has no protocol header.
pub const UNVERSIONED: u32 = 1;

pub const HEADER: &str = "briefing-protocol";

/// The protocol a request asked for: its header, or protocol 1 without one.
pub fn requested(header: Option<&str>) -> Result<u32, String> {
    let Some(value) = header else { return Ok(UNVERSIONED) };
    let version: u32 = value.trim().parse().map_err(|_| format!("invalid {HEADER} header: {value:?}"))?;
    if version > PROTOCOL {
        return Err(format!(
            "this client speaks briefing protocol {version}, but this hub only speaks {OLDEST_SUPPORTED}-{PROTOCOL}; upgrade the hub"
        ));
    }
    if version < OLDEST_SUPPORTED {
        return Err(format!(
            "this client speaks briefing protocol {version}, but this hub needs {OLDEST_SUPPORTED}-{PROTOCOL}; upgrade briefing and restart the agent session"
        ));
    }
    Ok(version)
}

/// What a client does with the hub's protocol header: anything but its own is an error.
pub fn check_hub(header: Option<&str>, hub: &str) -> Result<(), String> {
    match header.map(str::trim) {
        Some(value) if value == PROTOCOL.to_string() => Ok(()),
        Some(value) => Err(format!(
            "the hub at {hub} speaks briefing protocol {value}, this client speaks {PROTOCOL}; upgrade whichever is older (restart agent sessions after upgrading the client)"
        )),
        None => Err(format!(
            "the hub at {hub} predates briefing protocol {PROTOCOL} (it sent no {HEADER} header); upgrade the hub"
        )),
    }
}

// ---- Protocol 1 compatibility ----

/// A protocol 1 presentation (`checkpoint`, `decision`, top-level `decisions`) in the current shape.
pub fn presentation_from_v1(presentation: &mut Value) {
    migrate::v1_presentation(presentation);
}

/// A protocol 1 page submission (`chunks`, `decisions`, `overallNote`) in the current shape,
/// for `presentation` as the hub holds it now. A checkpoint answer goes to its chunk's open
/// question (the one without options that the checkpoint became), and a decision's answer to
/// the question of the same text, in its chunk when it was one of the chunk's.
pub fn submission_from_v1(submission: &Value, presentation: &Value) -> Value {
    let chunks = presentation["chunks"].as_array().cloned().unwrap_or_default();
    let open_question = |chunk: &Value| {
        chunk["questions"]
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .find(|q| q["options"].as_array().is_none_or(|o| o.is_empty()))
            .map(|q| q["question"].clone())
    };
    let v1_view = json!({
        "chunks": chunks.iter().map(|c| json!({ "title": c["title"], "checkpoint": open_question(c).unwrap_or(Value::Null) })).collect::<Vec<_>>()
    });
    let converted = migrate::v1_result(submission, &v1_view);
    let section_of = |question: &Value| {
        chunks
            .iter()
            .find(|c| c["questions"].as_array().into_iter().flatten().any(|q| q["question"] == *question))
            .map(|c| c["title"].clone())
    };
    let questions: Vec<Value> = converted["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| {
            let mut q = q.clone();
            if let Some(object) = q.as_object_mut() {
                // A submission carries no status; the hub derives it.
                object.remove("status");
                if !object.contains_key("section")
                    && let Some(section) = section_of(&object["question"])
                {
                    object.insert("section".into(), section);
                }
            }
            q
        })
        .collect();
    json!({ "questions": questions, "annotations": converted["annotations"], "notes": converted["notes"] })
}

/// Current feedback (`questions`, `annotations`, `notes`) in the protocol 1 shape: every question
/// becomes a decision whose selection is its first chosen option and whose guidance is the answer.
pub fn feedback_to_v1(feedback: &Value) -> Value {
    let decisions: Vec<Value> = feedback["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| {
            let selected = q["selected"]
                .as_array()
                .map(|s| s.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            json!({ "question": q["question"], "selected": selected, "note": q["answer"] })
        })
        .collect();
    json!({
        "chunks": [],
        "decisions": decisions,
        "annotations": feedback["annotations"],
        "notes": feedback["notes"],
        "overallNote": "",
    })
}

/// A wait result (`{briefingId, status, feedback?}`) for a protocol 1 client.
pub fn outcome_to_v1(outcome: &mut Value) {
    if let Some(feedback) = outcome.get_mut("feedback") {
        *feedback = feedback_to_v1(feedback);
    }
}

/// A briefing summary for a protocol 1 client, whose draft summary requires `sectionNotes` and
/// `decisions` (section notes no longer exist; answered questions stand in for decisions).
pub fn info_to_v1(info: &mut Value) {
    if let Some(draft) = info.get_mut("draft").and_then(Value::as_object_mut) {
        let answered = draft.remove("answered").unwrap_or(json!(0));
        draft.insert("sectionNotes".into(), json!(0));
        draft.insert("decisions".into(), answered);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation() {
        assert_eq!(requested(None), Ok(1));
        assert_eq!(requested(Some("2")), Ok(2));
        assert!(requested(Some("3")).unwrap_err().contains("upgrade the hub"));
        assert!(requested(Some("0")).unwrap_err().contains("restart the agent session"));
        assert!(requested(Some("two")).is_err());
        assert!(check_hub(Some("2"), "h").is_ok());
        assert!(check_hub(Some("3"), "h").unwrap_err().contains("speaks briefing protocol 3"));
        assert!(check_hub(None, "h").unwrap_err().contains("predates"));
    }

    #[test]
    fn v1_translations() {
        let feedback = json!({
            "questions": [{ "question": "Q?", "section": "S", "selected": ["A", "B"], "answer": "why", "status": "answered" }],
            "annotations": [], "notes": ["n"]
        });
        let v1 = feedback_to_v1(&feedback);
        assert_eq!(v1["decisions"][0], json!({ "question": "Q?", "selected": "A, B", "note": "why" }));
        assert_eq!(v1["overallNote"], "");

        let mut info = json!({ "id": "x", "draft": { "screen": 1, "answered": 2 } });
        info_to_v1(&mut info);
        assert_eq!(info["draft"], json!({ "screen": 1, "sectionNotes": 0, "decisions": 2 }));

        // The briefing as the hub holds it: the checkpoint and decision became questions.
        let presentation = json!({ "chunks": [{ "title": "C", "questions": [
            { "question": "Pick?", "options": [{ "label": "A" }, { "label": "B" }] },
            { "question": "Why?" }
        ] }] });
        let submission = json!({
            "chunks": [{ "title": "C", "status": "unmarked", "checkpoint": "because", "note": "" }],
            "decisions": [{ "question": "Pick?", "selected": "A", "note": "" }],
            "annotations": [], "notes": [], "overallNote": "all good"
        });
        assert_eq!(
            submission_from_v1(&submission, &presentation),
            json!({
                "questions": [
                    { "question": "Why?", "section": "C", "selected": [], "answer": "because" },
                    { "question": "Pick?", "section": "C", "selected": ["A"], "answer": "" }
                ],
                "annotations": [], "notes": ["all good"]
            })
        );
    }
}
