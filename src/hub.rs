//! Registry of presentations awaiting briefing.
//!
//! Every presentation has one unguessable `id`, used by the agent side (CLI / MCP / hub API)
//! and as the capability in the browser URL (`/briefing/<id>`). A hub is the only process that
//! serves its records: they live in memory and, when a [`Store`] is configured, are mirrored to
//! disk and reloaded at startup, so a restarted hub picks up where the last one left off.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use crate::content::Briefing;
use crate::response::{BriefingResponse, Outcome, parse_submission};
use crate::store::{Store, StoredRecord, now_secs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BriefingStatus {
    Active,
    Completed,
    Cancelled,
}

impl BriefingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BriefingStatus::Active => "active",
            BriefingStatus::Completed => "completed",
            BriefingStatus::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for BriefingStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Outcome {
    /// The record status behind this outcome.
    pub fn status(&self) -> BriefingStatus {
        match self {
            Outcome::Pending => BriefingStatus::Active,
            Outcome::Completed { .. } => BriefingStatus::Completed,
            Outcome::Cancelled { .. } => BriefingStatus::Cancelled,
        }
    }

    fn of(status: BriefingStatus, result: Option<BriefingResponse>) -> Self {
        let feedback = result.unwrap_or_default();
        match status {
            BriefingStatus::Active => Outcome::Pending,
            BriefingStatus::Completed => Outcome::Completed { feedback },
            BriefingStatus::Cancelled => Outcome::Cancelled { feedback },
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HubError {
    #[error("briefing not found")]
    NotFound,
    #[error("briefing already {0}")]
    AlreadyFinished(BriefingStatus),
    /// The page submitted something this briefing cannot accept.
    #[error("{0}")]
    Invalid(String),
    /// The change could not be written to the store, so it did not happen.
    #[error("could not save briefing: {0}")]
    Storage(String),
}

/// Outcome of a draft save.
#[derive(Debug, Clone, PartialEq)]
pub enum DraftSave {
    Saved {
        revision: u64,
    },
    /// The caller's base revision is behind; here is the current draft.
    Stale {
        revision: u64,
        draft: Value,
    },
}

/// Where the user is in a briefing, derived from the saved draft.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftSummary {
    /// 1-based chunk screen the user is on, counted as the page counts its steps: the review
    /// screen is not one of them, and reading it reports `review` instead.
    pub screen: u64,
    pub screens: u64,
    #[serde(default)]
    pub review: bool,
    pub annotations: u64,
    /// Questions with a selected option or a written answer.
    pub answered: u64,
    /// Unix milliseconds, as reported by the browser.
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BriefingInfo {
    #[serde(rename = "briefingId")]
    pub id: String,
    pub title: String,
    pub status: BriefingStatus,
    /// Unix seconds.
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(flatten)]
    pub origin: Origin,
    /// The link, filled in by the [`crate::backend::Site`] serving it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<DraftSummary>,
}

/// Who created a briefing: a display label (`claude-code@laptop`) and, when known, the agent
/// harness and its session id, which `briefing status` filters on and the pages show.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Origin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl Origin {
    pub fn source(source: impl Into<String>) -> Self {
        Self { source: Some(source.into()), ..Self::default() }
    }
}

/// The agent session a process runs under, from the environment its harness gives the commands
/// and stdio MCP servers it starts. Only meaningful in such a process; a hub's environment is
/// its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HarnessSession {
    pub harness: Option<String>,
    pub id: Option<String>,
    /// Set by the user (`BRIEFING_SESSION`), so it wins over ids a harness reports per call.
    pub explicit: bool,
}

impl HarnessSession {
    /// `BRIEFING_SESSION` (named by `BRIEFING_HARNESS`), else Claude Code's or Codex's own id.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.trim().is_empty());
        if let Some(id) = var("BRIEFING_SESSION") {
            return Self { harness: var("BRIEFING_HARNESS"), id: Some(id), explicit: true };
        }
        [("CLAUDE_CODE_SESSION_ID", "claude-code"), ("CODEX_THREAD_ID", "codex")]
            .iter()
            .find_map(|(name, harness)| {
                var(name).map(|id| Self { harness: Some(harness.to_string()), id: Some(id), explicit: false })
            })
            .unwrap_or_default()
    }
}

fn info(stored: &StoredRecord) -> BriefingInfo {
    BriefingInfo {
        id: stored.id.clone(),
        title: stored.presentation.title.clone(),
        status: stored.status,
        created_at: stored.created_at,
        finished_at: stored.finished_at,
        origin: stored.origin.clone(),
        url: None,
        draft: stored.draft.as_ref().map(|draft| draft_summary(&stored.presentation, draft)),
    }
}

pub fn draft_summary(presentation: &Briefing, draft: &Value) -> DraftSummary {
    let state = &draft["state"];
    let count_map = |value: &Value, keep: &dyn Fn(&Value) -> bool| {
        value.as_object().map(|m| m.values().filter(|v| keep(v)).count()).unwrap_or(0) as u64
    };
    let non_empty = |v: &Value, key: &str| v[key].as_str().is_some_and(|s| !s.trim().is_empty());
    let screens = presentation.chunks.len() as u64;
    let current = draft["current"].as_u64().unwrap_or(0);
    DraftSummary {
        screen: current.min(screens.saturating_sub(1)) + 1,
        screens,
        review: current >= screens,
        annotations: state["annotations"].as_array().map(|a| a.len()).unwrap_or(0) as u64,
        answered: count_map(&state["questions"], &|q| {
            q["selected"].as_array().is_some_and(|s| !s.is_empty()) || non_empty(q, "answer")
        }),
        updated_at: draft["updatedAt"].as_u64().unwrap_or(0),
    }
}

/// An in-memory record: the stored form plus a channel that wakes waiters on finish.
struct Record {
    stored: StoredRecord,
    status: watch::Sender<BriefingStatus>,
}

impl Record {
    fn new(stored: StoredRecord) -> Record {
        let (status, _) = watch::channel(stored.status);
        Record { stored, status }
    }

    fn is_active(&self) -> bool {
        self.stored.status == BriefingStatus::Active
    }
}

pub struct HubConfig {
    /// How long a finished briefing stays fetchable through `wait`/`status`.
    pub finished_ttl: Duration,
    /// How long an unanswered briefing stays open.
    pub active_ttl: Duration,
    /// On-disk mirror; `None` keeps everything in memory.
    pub store: Option<Store>,
}

impl HubConfig {
    pub const FINISHED_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    pub const ACTIVE_TTL: Duration = Duration::from_secs(28 * 24 * 60 * 60);
    /// The same defaults as CLI flag text (a test in main.rs keeps them in step).
    pub const FINISHED_TTL_TEXT: &str = "7d";
    pub const ACTIVE_TTL_TEXT: &str = "28d";
}

impl Default for HubConfig {
    fn default() -> Self {
        Self { finished_ttl: Self::FINISHED_TTL, active_ttl: Self::ACTIVE_TTL, store: None }
    }
}

pub struct Hub {
    config: HubConfig,
    /// Every record, mirrored on disk. Each change is written to the store under this lock
    /// before it is applied, so revisions reach disk in order and nothing unsaved is observed.
    records: Mutex<HashMap<String, Record>>,
    /// Unix seconds of the last sweep; sweeps are rate-limited because each one scans the
    /// store directory for stray temp files.
    last_sweep: AtomicU64,
}

/// A new briefing id: 22 alphanumeric characters (~131 bits), unguessable because it is also the
/// browser link's capability, and never starting with `-`, so it cannot parse as a CLI flag.
pub fn random_id() -> String {
    const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    const LEN: usize = 22;
    let mut id = String::with_capacity(LEN);
    let mut buf = [0u8; 32];
    while id.len() < LEN {
        rand::rng().fill_bytes(&mut buf);
        // Rejection sampling: 248 = 4 * 62, so `b % 62` stays uniform.
        let chars = buf.iter().filter(|&&b| b < 248).map(|&b| ALPHABET[usize::from(b % 62)] as char);
        id.extend(chars.take(LEN - id.len()));
    }
    id
}

pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

pub const SWEEP_EVERY: Duration = Duration::from_secs(60);

impl Hub {
    /// A hub serving `config`, with every record its store already holds.
    pub fn new(config: HubConfig) -> Self {
        let stored = config.store.as_ref().map(Store::list).unwrap_or_default();
        let records = stored.into_iter().map(|s| (s.id.clone(), Record::new(s))).collect();
        let hub = Self { config, records: Mutex::new(records), last_sweep: AtomicU64::new(0) };
        hub.sweep();
        hub
    }

    /// Write `stored` to the store, if there is one. Call while holding the records lock and
    /// before applying the change, so a failed write leaves the hub as it was.
    fn persist(&self, stored: &StoredRecord) -> Result<(), HubError> {
        let Some(store) = &self.config.store else {
            return Ok(());
        };
        store.save(stored).map_err(|error| {
            tracing::error!(%error, id = stored.id, "could not write briefing record");
            HubError::Storage(error.to_string())
        })
    }

    /// Register a presentation and return its id once it is saved.
    pub fn create(&self, presentation: Briefing, origin: Origin) -> Result<String, HubError> {
        self.sweep_if_due();
        let stored = StoredRecord {
            schema_version: crate::migrate::SCHEMA_VERSION,
            id: random_id(),
            presentation,
            status: BriefingStatus::Active,
            created_at: now_secs(),
            finished_at: None,
            origin,
            draft_revision: 0,
            draft: None,
            result: None,
        };
        let id = stored.id.clone();
        let mut records = self.records.lock().unwrap();
        self.persist(&stored)?;
        records.insert(id.clone(), Record::new(stored));
        Ok(id)
    }

    fn with_record<R>(&self, id: &str, read: impl FnOnce(&Record) -> R) -> Option<R> {
        self.records.lock().unwrap().get(id).map(read)
    }

    /// Whether `id` names a briefing this hub serves.
    pub fn has(&self, id: &str) -> bool {
        self.records.lock().unwrap().contains_key(id)
    }

    /// How many briefings are still waiting for the user.
    pub fn active_count(&self) -> usize {
        self.records.lock().unwrap().values().filter(|r| r.is_active()).count()
    }

    /// The JSON the browser page fetches: the presentation plus id, status, and draft.
    pub fn page_payload(&self, id: &str) -> Option<Value> {
        let records = self.records.lock().unwrap();
        let stored = &records.get(id)?.stored;
        let mut payload = serde_json::to_value(&stored.presentation).ok()?;
        let object = payload.as_object_mut()?;
        object.insert("id".into(), Value::String(stored.id.clone()));
        object.insert("status".into(), Value::String(stored.status.to_string()));
        object.insert("draftRevision".into(), Value::from(stored.draft_revision));
        object.insert("draft".into(), stored.draft.clone().unwrap_or(Value::Null));
        object.insert("keptFor".into(), Value::String(crate::guidance::human(self.config.finished_ttl)));
        // Who asked, for the page header.
        if let Ok(Value::Object(origin)) = serde_json::to_value(&stored.origin) {
            object.extend(origin);
        }
        Some(payload)
    }

    /// Save the browser's draft. `base` is the revision the browser last saw; a mismatch
    /// returns the newer draft instead of overwriting it.
    pub fn save_draft(&self, id: &str, base: Option<u64>, draft: Value) -> Result<DraftSave, HubError> {
        let mut records = self.records.lock().unwrap();
        let record = records.get_mut(id).ok_or(HubError::NotFound)?;
        let stored = &record.stored;
        if stored.status != BriefingStatus::Active {
            return Err(HubError::AlreadyFinished(stored.status));
        }
        if let Some(base) = base
            && base != stored.draft_revision
            && let Some(existing) = &stored.draft
        {
            return Ok(DraftSave::Stale { revision: stored.draft_revision, draft: existing.clone() });
        }
        let mut next = stored.clone();
        next.draft_revision += 1;
        next.draft = Some(draft);
        self.persist(&next)?;
        let revision = next.draft_revision;
        record.stored = next;
        Ok(DraftSave::Saved { revision })
    }

    fn finish(&self, id: &str, result: BriefingResponse, status: BriefingStatus) -> Result<(), HubError> {
        let mut records = self.records.lock().unwrap();
        let record = records.get_mut(id).ok_or(HubError::NotFound)?;
        if !record.is_active() {
            return Err(HubError::AlreadyFinished(record.stored.status));
        }
        let mut next = record.stored.clone();
        next.result = Some(result);
        next.finished_at = Some(now_secs());
        next.status = status;
        self.persist(&next)?;
        record.stored = next;
        // Waiters wake only once the outcome is on disk.
        record.status.send_replace(status);
        Ok(())
    }

    /// Finish briefing `id` with what its page submitted (`complete` or `cancel`), parsed
    /// strictly against the briefing.
    pub fn submit(&self, id: &str, body: &Value, cancelled: bool) -> Result<(), HubError> {
        let presentation = self
            .with_record(id, |r| match r.stored.status {
                BriefingStatus::Active => Ok(r.stored.presentation.clone()),
                status => Err(HubError::AlreadyFinished(status)),
            })
            .ok_or(HubError::NotFound)??;
        let feedback =
            parse_submission(body, &presentation, cancelled).map_err(|e| HubError::Invalid(e.to_string()))?;
        let status = if cancelled { BriefingStatus::Cancelled } else { BriefingStatus::Completed };
        self.finish(id, feedback, status)
    }

    /// Agent-side cancellation. `Ok(false)` when there was no open briefing to cancel.
    pub fn cancel(&self, id: &str) -> Result<bool, HubError> {
        match self.finish(id, BriefingResponse::default(), BriefingStatus::Cancelled) {
            Ok(()) => Ok(true),
            Err(HubError::NotFound | HubError::AlreadyFinished(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn status(&self, id: &str) -> Option<BriefingStatus> {
        self.with_record(id, |r| r.stored.status)
    }

    pub fn info(&self, id: &str) -> Option<BriefingInfo> {
        self.with_record(id, |r| info(&r.stored))
    }

    pub fn list(&self) -> Vec<BriefingInfo> {
        let mut infos: Vec<BriefingInfo> = self.records.lock().unwrap().values().map(|r| info(&r.stored)).collect();
        infos.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
        infos
    }

    fn snapshot(&self, id: &str) -> Result<(watch::Receiver<BriefingStatus>, Outcome), HubError> {
        let records = self.records.lock().unwrap();
        let record = records.get(id).ok_or(HubError::NotFound)?;
        Ok((record.status.subscribe(), Outcome::of(record.stored.status, record.stored.result.clone())))
    }

    /// Wait up to `timeout` for the briefing to finish.
    pub async fn wait(&self, id: &str, timeout: Duration) -> Result<Outcome, HubError> {
        let (mut rx, outcome) = self.snapshot(id)?;
        if outcome != Outcome::Pending {
            return Ok(outcome);
        }
        match tokio::time::timeout(timeout, rx.wait_for(|s| *s != BriefingStatus::Active)).await {
            Ok(Ok(_)) => Ok(self.snapshot(id)?.1),
            Ok(Err(_)) => Err(HubError::NotFound),
            Err(_) => Ok(Outcome::Pending),
        }
    }

    fn sweep_if_due(&self) {
        if now_secs().saturating_sub(self.last_sweep.load(Ordering::Relaxed)) >= SWEEP_EVERY.as_secs() {
            self.sweep();
        }
    }

    /// Drop expired records from memory and disk. The registry holds every record (this hub
    /// owns the store and loaded it at startup), so it decides what expired. Safe any time.
    pub fn sweep(&self) {
        let now = now_secs();
        self.last_sweep.store(now, Ordering::Relaxed);
        let mut records = self.records.lock().unwrap();
        records.retain(|id, record| {
            let stored = &record.stored;
            let keep = match stored.finished_at {
                Some(finished) => now.saturating_sub(finished) < self.config.finished_ttl.as_secs(),
                None => now.saturating_sub(stored.created_at) < self.config.active_ttl.as_secs(),
            };
            if !keep {
                if let Some(store) = &self.config.store {
                    store.remove(id);
                }
                if stored.finished_at.is_none() {
                    record.status.send_replace(BriefingStatus::Cancelled);
                }
            }
            keep
        });
        drop(records);
        if let Some(store) = &self.config.store {
            store.remove_stale_temp_files();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::demo;
    use serde_json::json;

    /// A complete submission for the demo carrying one note.
    fn notes(note: &str) -> Value {
        let mut body = crate::response::blank_submission(&demo());
        body["notes"] = json!([note]);
        body
    }

    #[tokio::test]
    async fn create_submit_wait_roundtrip() {
        let finished_ttl = Duration::from_secs(2 * 86_400);
        let hub = Hub::new(HubConfig { finished_ttl, ..HubConfig::default() });
        let created = hub.create(demo(), Origin::source("test")).unwrap();
        assert_eq!(hub.status(&created), Some(BriefingStatus::Active));

        let page = hub.page_payload(&created).unwrap();
        assert_eq!(page["status"], "active");
        assert_eq!(page["draft"], Value::Null);
        assert_eq!(page["keptFor"], crate::guidance::human(finished_ttl));
        assert!(hub.page_payload("nope").is_none());

        assert_eq!(hub.wait(&created, Duration::from_millis(20)).await, Ok(Outcome::Pending));

        hub.submit(&created, &notes("great"), false).unwrap();
        match hub.wait(&created, Duration::from_secs(1)).await.unwrap() {
            Outcome::Completed { feedback } => assert_eq!(feedback.notes, vec!["great"]),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(hub.submit(&created, &json!({}), false), Err(HubError::AlreadyFinished(BriefingStatus::Completed)));
        assert_eq!(hub.cancel(&created), Ok(false));
        assert_eq!(hub.cancel("missing"), Ok(false));
        assert_eq!(hub.wait("missing", Duration::from_millis(1)).await, Err(HubError::NotFound));
        assert_eq!(hub.info(&created).unwrap().origin.source.as_deref(), Some("test"));
    }

    #[test]
    fn drafts_are_revisioned() {
        let hub = Hub::new(HubConfig::default());
        let created = hub.create(demo(), Origin::default()).unwrap();
        let draft = json!({"current": 1, "state": {"questions": {"c0-0": {"selected": ["A"], "answer": ""}, "c1-0": {"selected": [], "answer": ""}}, "annotations": [{}]}, "updatedAt": 5});
        assert_eq!(hub.save_draft(&created, Some(0), draft.clone()), Ok(DraftSave::Saved { revision: 1 }));
        assert_eq!(hub.save_draft(&created, None, draft.clone()), Ok(DraftSave::Saved { revision: 2 }));
        match hub.save_draft(&created, Some(1), json!({})).unwrap() {
            DraftSave::Stale { revision: 2, draft: existing } => assert_eq!(existing, draft),
            other => panic!("unexpected {other:?}"),
        }
        let page = hub.page_payload(&created).unwrap();
        assert_eq!(page["draftRevision"], 2);
        assert_eq!(page["draft"]["current"], 1);
        let summary = hub.info(&created).unwrap().draft.unwrap();
        assert_eq!(summary.screen, 2);
        assert_eq!(summary.annotations, 1);
        assert_eq!(summary.answered, 1);
        assert_eq!(summary.screens, demo().chunks.len() as u64);
        assert!(!summary.review);
        let review = draft_summary(&demo(), &json!({"current": summary.screens}));
        assert!(review.review);
        assert_eq!(review.screen, summary.screens);
        hub.cancel(&created).unwrap();
        assert!(matches!(hub.save_draft(&created, None, json!({})), Err(HubError::AlreadyFinished(_))));
    }

    #[tokio::test]
    async fn wake_waiter_on_cancel() {
        let hub = std::sync::Arc::new(Hub::new(HubConfig::default()));
        let created = hub.create(demo(), Origin::default()).unwrap();
        let waiter = {
            let hub = hub.clone();
            let id = created.clone();
            tokio::spawn(async move { hub.wait(&id, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(hub.cancel(&created), Ok(true));
        assert_eq!(waiter.await.unwrap().unwrap(), Outcome::cancelled());
        assert_eq!(hub.page_payload(&created).unwrap()["status"], "cancelled");
    }

    #[test]
    fn sweep_expires_records() {
        let hub = Hub::new(HubConfig { finished_ttl: Duration::ZERO, active_ttl: Duration::ZERO, store: None });
        let created = hub.create(demo(), Origin::default()).unwrap();
        hub.sweep();
        assert!(hub.status(&created).is_none());
        assert!(hub.page_payload(&created).is_none());
    }

    #[tokio::test]
    async fn a_restarted_hub_reloads_its_records() {
        let dir = tempfile::tempdir().unwrap();
        let config = || HubConfig { store: Some(Store::open(dir.path()).unwrap()), ..HubConfig::default() };

        // The first hub creates a briefing and the user saves a draft, then it stops.
        let first = Hub::new(config());
        let id = first.create(demo(), Origin::source("first")).unwrap();
        first.save_draft(&id, None, json!({"current": 1, "state": {}, "updatedAt": 1})).unwrap();
        drop(first);

        // The next one serves it with the draft intact and takes the submission.
        let second = Hub::new(config());
        assert_eq!(second.info(&id).unwrap().origin.source.as_deref(), Some("first"));
        assert_eq!(second.page_payload(&id).unwrap()["draft"]["current"], 1);
        assert_eq!(second.active_count(), 1);
        second.submit(&id, &notes("done"), false).unwrap();
        assert_eq!(second.active_count(), 0);
        drop(second);

        // And a third returns the stored result without the user doing anything.
        let third = Hub::new(config());
        match third.wait(&id, Duration::from_millis(1)).await.unwrap() {
            Outcome::Completed { feedback } => assert_eq!(feedback.notes, vec!["done"]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failed_write_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let records = dir.path().join("records");
        let hub = std::sync::Arc::new(Hub::new(HubConfig {
            store: Some(Store::open(&records).unwrap()),
            ..HubConfig::default()
        }));
        let id = hub.create(demo(), Origin::default()).unwrap();
        let waiter = {
            let (hub, id) = (hub.clone(), id.clone());
            tokio::spawn(async move { hub.wait(&id, Duration::from_millis(200)).await })
        };

        // The store goes away: every change fails, and none of them happens.
        std::fs::remove_dir_all(&records).unwrap();
        assert!(matches!(hub.create(demo(), Origin::default()), Err(HubError::Storage(_))));
        assert_eq!(hub.list().len(), 1);
        let draft = json!({"current": 1, "state": {}, "updatedAt": 1});
        assert!(matches!(hub.save_draft(&id, None, draft.clone()), Err(HubError::Storage(_))));
        assert_eq!(hub.page_payload(&id).unwrap()["draftRevision"], 0);
        assert!(matches!(hub.submit(&id, &notes("lost"), false), Err(HubError::Storage(_))));
        assert!(matches!(hub.cancel(&id), Err(HubError::Storage(_))));
        assert_eq!(hub.status(&id), Some(BriefingStatus::Active));
        assert_eq!(waiter.await.unwrap(), Ok(Outcome::Pending), "no waiter sees an unsaved outcome");

        // Once it is back, the same changes go through.
        std::fs::create_dir_all(&records).unwrap();
        assert_eq!(hub.save_draft(&id, None, draft), Ok(DraftSave::Saved { revision: 1 }));
        hub.submit(&id, &notes("kept"), false).unwrap();
        assert_eq!(hub.status(&id), Some(BriefingStatus::Completed));
    }

    #[tokio::test]
    async fn a_waiter_wakes_only_once_the_outcome_is_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = || Store::open(dir.path()).unwrap();
        let hub = std::sync::Arc::new(Hub::new(HubConfig { store: Some(store()), ..HubConfig::default() }));
        let id = hub.create(demo(), Origin::default()).unwrap();
        assert_eq!(store().load(&id).unwrap().status, BriefingStatus::Active, "created on disk");
        let waiter = {
            let (hub, id) = (hub.clone(), id.clone());
            tokio::spawn(async move { hub.wait(&id, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        hub.submit(&id, &notes("durable"), false).unwrap();
        assert!(matches!(waiter.await.unwrap(), Ok(Outcome::Completed { .. })));
        let stored = store().load(&id).unwrap();
        assert_eq!(stored.status, BriefingStatus::Completed);
        assert_eq!(stored.result.unwrap().notes, vec!["durable"]);
    }

    #[test]
    fn sweep_deletes_expired_records_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = || Store::open(dir.path()).unwrap();
        let config = |active_ttl| HubConfig { store: Some(store()), active_ttl, ..HubConfig::default() };
        let id = Hub::new(config(HubConfig::ACTIVE_TTL)).create(demo(), Origin::default()).unwrap();
        assert!(store().load(&id).is_some());
        // A hub with a zero TTL loads it, finds it expired, and removes the file.
        let hub = Hub::new(config(Duration::ZERO));
        assert!(hub.status(&id).is_none());
        assert!(store().load(&id).is_none());
    }

    #[test]
    fn ids_are_alphanumeric() {
        let id = random_id();
        assert_eq!(id.len(), 22);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()), "{id}");
    }
}
