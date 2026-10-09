//! Versioned migrations for stored briefing records.
//!
//! Every record file carries `schemaVersion`; a file without one is version 1 (written before
//! versioning existed). Loading a record runs each migration from its version up to
//! [`SCHEMA_VERSION`] on the raw JSON, so the typed structs only ever see the current shape.
//! Each migration is a small, pure function of the previous version's JSON; add one per shape
//! change and bump [`SCHEMA_VERSION`].

use serde_json::{Map, Value, json};

/// The shape `StoredRecord` serializes today.
pub const SCHEMA_VERSION: u64 = 3;

/// Version assumed for a record file with no `schemaVersion`.
pub const LEGACY_SCHEMA_VERSION: u64 = 1;

/// `MIGRATIONS[n]` turns a version `n + 1` record into version `n + 2`.
const MIGRATIONS: [fn(&mut Value); (SCHEMA_VERSION - LEGACY_SCHEMA_VERSION) as usize] = [v1_questions, v2_one_id];

#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    /// Already current; nothing changed.
    Current,
    /// Upgraded from the given version to [`SCHEMA_VERSION`].
    Upgraded { from: u64 },
}

/// Bring a stored record up to [`SCHEMA_VERSION`] in place. Fails for a record written by a
/// newer binary: it cannot be read safely, and must not be rewritten in an older shape.
pub fn migrate(record: &mut Value) -> Result<Migration, String> {
    let version = record.get("schemaVersion").and_then(Value::as_u64).unwrap_or(LEGACY_SCHEMA_VERSION);
    if version > SCHEMA_VERSION {
        return Err(format!("written by a newer briefing (schema {version}, this build reads up to {SCHEMA_VERSION})"));
    }
    if version == SCHEMA_VERSION {
        return Ok(Migration::Current);
    }
    let Some(object) = record.as_object_mut() else {
        return Err("record is not a JSON object".into());
    };
    let mut value = Value::Object(std::mem::take(object));
    for step in &MIGRATIONS[(version - LEGACY_SCHEMA_VERSION) as usize..] {
        step(&mut value);
    }
    value["schemaVersion"] = json!(SCHEMA_VERSION);
    *record = value;
    Ok(Migration::Upgraded { from: version })
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().trim().to_string()
}

fn as_object(value: &mut Value) -> Option<&mut Map<String, Value>> {
    value.as_object_mut()
}

/// v2 -> v3: the briefing's `id` became its link's capability too, so the separate `token`
/// (and the `url` built from it) went. Links to such a briefing change; its id does not.
fn v2_one_id(record: &mut Value) {
    if let Some(object) = as_object(record) {
        object.remove("token");
        object.remove("url");
    }
}

/// v1 -> v2: checkpoints and decisions became optional questions, sources were dropped, and
/// feedback kept only questions, comments, and notes (section notes, the revisit flag, and the
/// overall response folded into notes).
fn v1_questions(record: &mut Value) {
    let presentation = record.get("presentation").cloned().unwrap_or(Value::Null);
    let chunk_titles: Vec<String> = presentation["chunks"]
        .as_array()
        .map(|chunks| chunks.iter().map(|c| text(&c["title"])).collect())
        .unwrap_or_default();

    // The result first: turning a checkpoint answer into a question needs the v1 presentation.
    // A result already in the v2 shape (converted by hand before versioning) is left alone.
    if let Some(result) = record.get_mut("result").filter(|r| r.is_object() && r.get("questions").is_none()) {
        *result = v1_result(result, &presentation);
    }
    if let Some(presentation) = record.get_mut("presentation") {
        v1_presentation(presentation);
    }
    if let Some(state) = record.get_mut("draft").and_then(|draft| draft.get_mut("state")) {
        v1_draft_state(state, &chunk_titles);
    }
}

/// A v1 decision (`{question, context?, required?, options}`) as a v2 question.
fn decision_to_question(decision: &Value) -> Value {
    let mut question = json!({ "question": decision["question"] });
    if let Some(context) = decision.get("context").filter(|c| c.is_string()) {
        question["context"] = context.clone();
    }
    if let Some(options) = decision.get("options").filter(|o| o.as_array().is_some_and(|o| !o.is_empty())) {
        question["options"] = options.clone();
    }
    question
}

pub(crate) fn v1_presentation(presentation: &mut Value) {
    if let Some(chunks) = presentation.get_mut("chunks").and_then(Value::as_array_mut) {
        for chunk in chunks.iter_mut().filter_map(as_object) {
            chunk.remove("sources");
            // Same order the v1 page used: the decision card, then the checkpoint prompt.
            let mut questions = Vec::new();
            if let Some(decision) = chunk.remove("decision").filter(Value::is_object) {
                questions.push(decision_to_question(&decision));
            }
            if let Some(checkpoint) = chunk.remove("checkpoint").filter(|c| !text(c).is_empty()) {
                questions.push(json!({ "question": checkpoint }));
            }
            if !questions.is_empty() {
                chunk.insert("questions".into(), Value::Array(questions));
            }
        }
    }
    if let Some(object) = presentation.as_object_mut()
        && let Some(decisions) = object.remove("decisions")
    {
        let questions: Vec<Value> = decisions.as_array().into_iter().flatten().map(decision_to_question).collect();
        if !questions.is_empty() {
            object.insert("questions".into(), Value::Array(questions));
        }
    }
}

/// v1 draft answers, keyed like the v2 page keys them: `c<chunk>-<n>` in each chunk's question
/// order (decision, then checkpoint) and `b<n>` for the briefing-wide ones. Section notes and
/// the overall response are left for the page, which folds them into notes.
fn v1_draft_state(state: &mut Value, chunk_titles: &[String]) {
    let Some(state) = state.as_object_mut() else { return };
    let decisions = state.remove("decisions").unwrap_or(Value::Null);
    let answer = |old: &Value| {
        let selected = text(&old["selected"]);
        json!({ "selected": if selected.is_empty() { json!([]) } else { json!([selected]) }, "answer": old["note"].as_str().unwrap_or_default() })
    };
    let mut questions = Map::new();
    for index in 0..chunk_titles.len() {
        let mut n = 0;
        if let Some(old) = decisions.get(format!("chunk:{index}")) {
            questions.insert(format!("c{index}-{n}"), answer(old));
            n += 1;
        }
        if let Some(checkpoint) = state.get_mut("chunks").and_then(|chunks| chunks.get_mut(index.to_string()))
            && let Some(old) = checkpoint.as_object_mut().and_then(|c| c.remove("checkpoint"))
            && !text(&old).is_empty()
        {
            questions.insert(format!("c{index}-{n}"), json!({ "selected": [], "answer": old }));
        }
    }
    for (key, old) in decisions.as_object().into_iter().flatten() {
        if let Ok(n) = key.parse::<usize>() {
            questions.insert(format!("b{n}"), answer(old));
        }
    }
    if !questions.is_empty() {
        state.insert("questions".into(), Value::Object(questions));
    }
}

/// A v1 submitted result in the v2 feedback shape.
pub(crate) fn v1_result(result: &Value, presentation: &Value) -> Value {
    let mut questions = Vec::new();
    let mut notes: Vec<Value> = result["notes"].as_array().cloned().unwrap_or_default();
    let checkpoint_for = |title: &str| {
        presentation["chunks"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| text(&c["title"]) == title)
            .map(|c| text(&c["checkpoint"]))
    };
    for chunk in result["chunks"].as_array().into_iter().flatten() {
        let title = text(&chunk["title"]);
        let checkpoint = text(&chunk["checkpoint"]);
        if !checkpoint.is_empty() {
            let question = checkpoint_for(&title).filter(|q| !q.is_empty()).unwrap_or_else(|| "Checkpoint".into());
            questions.push(json!({ "question": question, "section": title, "selected": [], "answer": checkpoint, "status": "answered" }));
        }
        let note = text(&chunk["note"]);
        if !note.is_empty() {
            notes.push(json!(format!("{title}: {note}")));
        }
        if chunk["status"].as_str() == Some("revisit") {
            notes.push(json!(format!("Flagged for follow-up: {title}")));
        }
    }
    for decision in result["decisions"].as_array().into_iter().flatten() {
        let selected = text(&decision["selected"]);
        let answer = text(&decision["note"]);
        let status = if selected.is_empty() && answer.is_empty() { "unresolved" } else { "answered" };
        questions.push(json!({
            "question": decision["question"],
            "selected": if selected.is_empty() { json!([]) } else { json!([selected]) },
            "answer": answer,
            "status": status,
        }));
    }
    let overall = text(&result["overallNote"]);
    if !overall.is_empty() {
        notes.push(json!(overall));
    }
    json!({
        "questions": questions,
        "annotations": result["annotations"].as_array().cloned().unwrap_or_default(),
        "notes": notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v1 record: an inline decision and a checkpoint, a top-level decision, sources, a draft
    /// with answers to both, and a submitted result in the old feedback shape.
    fn v1_record() -> Value {
        json!({
            "id": "abc", "token": "tok", "status": "completed", "createdAt": 1, "finishedAt": 2,
            "presentation": {
                "title": "T", "goal": "G",
                "chunks": [
                    { "title": "First", "mainPoint": "M",
                      "sources": [{ "label": "x", "url": "https://example.com" }],
                      "checkpoint": "What changed?",
                      "decision": { "question": "Pick one?", "required": true, "context": "because",
                                    "options": [{ "label": "A", "recommended": true }, { "label": "B" }] } },
                    { "title": "Second", "mainPoint": "N" }
                ],
                "decisions": [{ "question": "Ship?", "options": [{ "label": "Yes" }, { "label": "No" }] }]
            },
            "draft": { "current": 1, "state": {
                "chunks": { "0": { "note": "a note", "checkpoint": "an answer", "status": "revisit" } },
                "decisions": { "chunk:0": { "selected": "A", "note": "why" }, "0": { "selected": "", "note": "" } },
                "annotations": [], "notes": [], "overallNote": "overall"
            } },
            "result": {
                "chunks": [{ "title": "First", "status": "revisit", "checkpoint": "an answer", "note": "a note" }],
                "decisions": [{ "question": "Pick one?", "selected": "A", "note": "why" }, { "question": "Ship?", "selected": "", "note": "" }],
                "annotations": [{ "location": "First", "quote": "q", "comment": "c" }],
                "notes": ["free"], "overallNote": "overall"
            }
        })
    }

    #[test]
    fn v1_records_become_current_and_readable() {
        let mut record = v1_record();
        assert_eq!(migrate(&mut record), Ok(Migration::Upgraded { from: 1 }));
        assert_eq!(record["schemaVersion"], SCHEMA_VERSION);

        let chunk = &record["presentation"]["chunks"][0];
        assert!(chunk.get("sources").is_none() && chunk.get("decision").is_none() && chunk.get("checkpoint").is_none());
        assert_eq!(chunk["questions"][0]["question"], "Pick one?");
        assert_eq!(chunk["questions"][0]["context"], "because");
        assert!(chunk["questions"][0].get("required").is_none());
        assert_eq!(chunk["questions"][1], json!({ "question": "What changed?" }));
        assert_eq!(record["presentation"]["questions"][0]["question"], "Ship?");

        let answers = &record["draft"]["state"]["questions"];
        assert_eq!(answers["c0-0"], json!({ "selected": ["A"], "answer": "why" }));
        assert_eq!(answers["c0-1"], json!({ "selected": [], "answer": "an answer" }));
        assert_eq!(answers["b0"], json!({ "selected": [], "answer": "" }));
        assert!(record["draft"]["state"].get("decisions").is_none());

        let result = &record["result"];
        assert_eq!(result["questions"][0]["question"], "What changed?");
        assert_eq!(result["questions"][0]["answer"], "an answer");
        assert_eq!(result["questions"][1]["selected"], json!(["A"]));
        assert_eq!(result["questions"][2]["status"], "unresolved");
        assert_eq!(result["notes"], json!(["free", "First: a note", "Flagged for follow-up: First", "overall"]));

        // The typed record reads it, and a second pass changes nothing.
        let typed: crate::store::StoredRecord = serde_json::from_value(record.clone()).unwrap();
        assert_eq!(typed.presentation.chunks[0].questions.len(), 2);
        assert_eq!(typed.result.unwrap().questions.len(), 3);
        assert_eq!(migrate(&mut record), Ok(Migration::Current));
    }

    #[test]
    fn unversioned_records_already_in_the_new_shape_survive() {
        let mut record = json!({
            "id": "x", "token": "t", "status": "completed", "createdAt": 1,
            "presentation": { "title": "T", "goal": "G", "chunks": [{ "title": "C", "mainPoint": "M", "questions": [{ "question": "Q?" }] }] },
            "draft": { "state": { "questions": { "c0-0": { "selected": [], "answer": "kept" } }, "annotations": [], "notes": [] } },
            "result": { "questions": [{ "question": "Q?", "section": "C", "selected": [], "answer": "kept", "status": "answered" }], "annotations": [], "notes": [] }
        });
        let mut before = record.clone();
        assert_eq!(migrate(&mut record), Ok(Migration::Upgraded { from: 1 }));
        record.as_object_mut().unwrap().remove("schemaVersion");
        before.as_object_mut().unwrap().remove("token");
        assert_eq!(record, before);
    }

    #[test]
    fn v2_records_lose_their_token() {
        let mut record =
            json!({ "schemaVersion": 2, "id": "x", "token": "t", "url": "http://h/briefing/t", "status": "active" });
        assert_eq!(migrate(&mut record), Ok(Migration::Upgraded { from: 2 }));
        assert_eq!(record, json!({ "schemaVersion": SCHEMA_VERSION, "id": "x", "status": "active" }));
    }

    #[test]
    fn newer_records_are_refused() {
        let mut record = json!({ "schemaVersion": SCHEMA_VERSION + 1 });
        assert!(migrate(&mut record).unwrap_err().contains("newer"));
    }
}
