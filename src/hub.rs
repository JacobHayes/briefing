//! Registry of presentations awaiting briefing.
//!
//! Every presentation has two unguessable identifiers: the `id` used by the agent side
//! (CLI / MCP / hub API) and the `token` embedded in the browser URL. Records live in
//! memory and, when a [`Store`] is configured, are mirrored to disk so another process can
//! adopt them (`briefing await <id>` after the creator died) and so results survive until
//! the agent fetches them.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use crate::content::Briefing;
use crate::response::{BriefingResponse, parse_browser_result};
use crate::store::{Store, StoredRecord, now_secs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BriefingStatus {
    Active,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    Pending,
    Done(BriefingResponse),
}

impl PartialEq for BriefingResponse {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}
impl Eq for BriefingResponse {}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HubError {
    #[error("briefing not found")]
    NotFound,
    #[error("briefing already {0:?}")]
    AlreadyFinished(BriefingStatus),
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

#[derive(Debug, Clone, Serialize)]
pub struct CreatedBriefing {
    pub id: String,
    pub token: String,
}

/// Where the user is in a briefing, derived from the saved draft.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftSummary {
    /// 1-based screen the user is on (chunks, then decisions, then the review screen).
    pub screen: u64,
    pub screens: u64,
    pub annotations: u64,
    pub section_notes: u64,
    pub decisions: u64,
    /// Unix milliseconds, as reported by the browser.
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BriefingInfo {
    pub id: String,
    pub title: String,
    pub status: BriefingStatus,
    pub age_secs: u64,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Filled in by whoever knows the public origin (backend or hub API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<DraftSummary>,
    /// True when this process loaded the record from disk rather than creating it.
    #[serde(default)]
    pub adopted: bool,
    /// True when the record is only on disk (listed, but not served by this process).
    #[serde(default)]
    pub on_disk_only: bool,
}

struct Record {
    id: String,
    token: String,
    presentation: Briefing,
    status: watch::Sender<BriefingStatus>,
    result: Option<BriefingResponse>,
    created_at: u64,
    finished_at: Option<u64>,
    source: Option<String>,
    url: Option<String>,
    draft_revision: u64,
    draft: Option<Value>,
    adopted: bool,
}

impl Record {
    fn stored(&self) -> StoredRecord {
        StoredRecord {
            id: self.id.clone(),
            token: self.token.clone(),
            presentation: self.presentation.clone(),
            status: *self.status.borrow(),
            created_at: self.created_at,
            finished_at: self.finished_at,
            source: self.source.clone(),
            url: self.url.clone(),
            draft_revision: self.draft_revision,
            draft: self.draft.clone(),
            result: self.result.clone(),
        }
    }

    fn from_stored(stored: StoredRecord) -> Record {
        let (status, _) = watch::channel(stored.status);
        Record {
            id: stored.id,
            token: stored.token,
            presentation: stored.presentation,
            status,
            result: stored.result,
            created_at: stored.created_at,
            finished_at: stored.finished_at,
            source: stored.source,
            url: stored.url,
            draft_revision: stored.draft_revision,
            draft: stored.draft,
            adopted: true,
        }
    }

    fn info(&self) -> BriefingInfo {
        BriefingInfo {
            id: self.id.clone(),
            title: self.presentation.title.clone(),
            status: *self.status.borrow(),
            age_secs: now_secs().saturating_sub(self.created_at),
            created_at: self.created_at,
            finished_at: self.finished_at,
            source: self.source.clone(),
            url: self.url.clone(),
            draft: self.draft.as_ref().map(|draft| draft_summary(&self.presentation, draft)),
            adopted: self.adopted,
            on_disk_only: false,
        }
    }
}

pub fn draft_summary(presentation: &Briefing, draft: &Value) -> DraftSummary {
    let state = &draft["state"];
    let count_map = |value: &Value, keep: &dyn Fn(&Value) -> bool| {
        value.as_object().map(|m| m.values().filter(|v| keep(v)).count()).unwrap_or(0) as u64
    };
    let non_empty = |v: &Value, key: &str| v[key].as_str().is_some_and(|s| !s.trim().is_empty());
    DraftSummary {
        screen: draft["current"].as_u64().unwrap_or(0) + 1,
        screens: (presentation.chunks.len() + presentation.decisions.len()) as u64 + 1,
        annotations: state["annotations"].as_array().map(|a| a.len()).unwrap_or(0) as u64,
        section_notes: count_map(&state["chunks"], &|c| {
            non_empty(c, "note") || non_empty(c, "checkpoint") || c["status"].as_str() == Some("revisit")
        }),
        decisions: count_map(&state["decisions"], &|d| non_empty(d, "selected") || non_empty(d, "note")),
        updated_at: draft["updatedAt"].as_u64().unwrap_or(0),
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
    pub const FINISHED_TTL: Duration = Duration::from_secs(6 * 60 * 60);
    pub const ACTIVE_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);

    /// Default TTLs plus the default on-disk store.
    pub fn with_default_store() -> Self {
        Self { store: Store::open_default(), ..Self::default() }
    }
}

impl Default for HubConfig {
    fn default() -> Self {
        Self { finished_ttl: Self::FINISHED_TTL, active_ttl: Self::ACTIVE_TTL, store: None }
    }
}

pub struct Hub {
    config: HubConfig,
    records: Mutex<HashMap<String, Record>>,
    tokens: Mutex<HashMap<String, String>>,
}

pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// How often a waiter re-reads the on-disk record, in case another process finished it.
const RECONCILE_EVERY: Duration = Duration::from_secs(2);

impl Hub {
    pub fn new(config: HubConfig) -> Self {
        let hub = Self { config, records: Mutex::new(HashMap::new()), tokens: Mutex::new(HashMap::new()) };
        hub.sweep();
        hub
    }

    pub fn store(&self) -> Option<&Store> {
        self.config.store.as_ref()
    }

    fn persist(&self, record: &Record) {
        if let Some(store) = &self.config.store
            && let Err(error) = store.save(&record.stored())
        {
            tracing::warn!(%error, id = record.id, "could not write briefing record");
        }
    }

    pub fn create(&self, presentation: Briefing, source: Option<String>) -> CreatedBriefing {
        self.sweep();
        let id = random_token(12);
        let token = random_token(24);
        let (status, _) = watch::channel(BriefingStatus::Active);
        let record = Record {
            id: id.clone(),
            token: token.clone(),
            presentation,
            status,
            result: None,
            created_at: now_secs(),
            finished_at: None,
            source,
            url: None,
            draft_revision: 0,
            draft: None,
            adopted: false,
        };
        self.persist(&record);
        self.records.lock().unwrap().insert(id.clone(), record);
        self.tokens.lock().unwrap().insert(token.clone(), id.clone());
        CreatedBriefing { id, token }
    }

    /// Remember the public URL a briefing is served at (shown by `status` and the dashboard,
    /// also after the serving process is gone).
    pub fn set_url(&self, id: &str, url: &str) {
        let mut records = self.records.lock().unwrap();
        if let Some(record) = records.get_mut(id) {
            record.url = Some(url.to_string());
            self.persist(record);
        }
    }

    fn insert_adopted(&self, stored: StoredRecord) {
        let record = Record::from_stored(stored);
        self.tokens.lock().unwrap().insert(record.token.clone(), record.id.clone());
        self.records.lock().unwrap().insert(record.id.clone(), record);
    }

    /// Load `id` from disk if this process does not know it. Returns true when it adopted it.
    pub fn ensure_loaded(&self, id: &str) -> bool {
        if self.records.lock().unwrap().contains_key(id) {
            return false;
        }
        let Some(stored) = self.config.store.as_ref().and_then(|store| store.load(id)) else {
            return false;
        };
        tracing::info!(id, status = ?stored.status, "adopted briefing record from disk");
        self.insert_adopted(stored);
        true
    }

    fn id_for_token(&self, token: &str) -> Option<String> {
        if let Some(id) = self.tokens.lock().unwrap().get(token).cloned() {
            return Some(id);
        }
        let stored = self.config.store.as_ref()?.find_by_token(token)?;
        let id = stored.id.clone();
        self.insert_adopted(stored);
        Some(id)
    }

    /// If another process finished this briefing on disk, apply that here.
    fn reconcile(&self, id: &str) {
        let Some(store) = &self.config.store else {
            return;
        };
        let active = self.records.lock().unwrap().get(id).is_some_and(|r| *r.status.borrow() == BriefingStatus::Active);
        if !active {
            return;
        }
        if let Some(stored) = store.load(id)
            && stored.status != BriefingStatus::Active
        {
            let result = stored.result.unwrap_or_else(|| parse_browser_result(&Value::Null, true));
            let _ = self.finish(id, result, stored.status);
        }
    }

    /// The JSON the browser page fetches: the presentation plus id, status, and draft.
    pub fn page_payload(&self, token: &str) -> Option<Value> {
        let id = self.id_for_token(token)?;
        let records = self.records.lock().unwrap();
        let record = records.get(&id)?;
        let mut payload = serde_json::to_value(&record.presentation).ok()?;
        let object = payload.as_object_mut()?;
        object.insert("id".into(), Value::String(record.id.clone()));
        object.insert("status".into(), serde_json::to_value(*record.status.borrow()).ok()?);
        object.insert("draftRevision".into(), Value::from(record.draft_revision));
        object.insert("draft".into(), record.draft.clone().unwrap_or(Value::Null));
        if object.get("mode").is_none_or(Value::is_null) {
            object.insert("mode".into(), Value::String("briefing".into()));
        }
        if !object.contains_key("decisions") {
            object.insert("decisions".into(), Value::Array(vec![]));
        }
        Some(payload)
    }

    /// Save the browser's draft. `base` is the revision the browser last saw; a mismatch
    /// returns the newer draft instead of overwriting it.
    pub fn save_draft(&self, token: &str, base: Option<u64>, draft: Value) -> Result<DraftSave, HubError> {
        let id = self.id_for_token(token).ok_or(HubError::NotFound)?;
        let mut records = self.records.lock().unwrap();
        let record = records.get_mut(&id).ok_or(HubError::NotFound)?;
        let current = *record.status.borrow();
        if current != BriefingStatus::Active {
            return Err(HubError::AlreadyFinished(current));
        }
        if let Some(base) = base
            && base != record.draft_revision
            && let Some(existing) = &record.draft
        {
            return Ok(DraftSave::Stale { revision: record.draft_revision, draft: existing.clone() });
        }
        record.draft_revision += 1;
        record.draft = Some(draft);
        self.persist(record);
        Ok(DraftSave::Saved { revision: record.draft_revision })
    }

    fn finish(&self, id: &str, result: BriefingResponse, status: BriefingStatus) -> Result<(), HubError> {
        let mut records = self.records.lock().unwrap();
        let record = records.get_mut(id).ok_or(HubError::NotFound)?;
        let current = *record.status.borrow();
        if current != BriefingStatus::Active {
            return Err(HubError::AlreadyFinished(current));
        }
        record.result = Some(result);
        record.finished_at = Some(now_secs());
        record.status.send_replace(status);
        self.persist(record);
        Ok(())
    }

    /// Browser submission (`complete` or `cancel`) for the presentation behind `token`.
    pub fn submit_by_token(&self, token: &str, body: &Value, cancelled: bool) -> Result<(), HubError> {
        let id = self.id_for_token(token).ok_or(HubError::NotFound)?;
        let status = if cancelled { BriefingStatus::Cancelled } else { BriefingStatus::Completed };
        self.finish(&id, parse_browser_result(body, cancelled), status)
    }

    /// Agent-side cancellation. Returns false when the briefing was not active.
    pub fn cancel(&self, id: &str) -> bool {
        self.ensure_loaded(id);
        self.finish(id, parse_browser_result(&Value::Null, true), BriefingStatus::Cancelled).is_ok()
    }

    pub fn status(&self, id: &str) -> Option<BriefingStatus> {
        self.ensure_loaded(id);
        self.records.lock().unwrap().get(id).map(|r| *r.status.borrow())
    }

    pub fn info(&self, id: &str) -> Option<BriefingInfo> {
        self.ensure_loaded(id);
        self.reconcile(id);
        self.records.lock().unwrap().get(id).map(Record::info)
    }

    /// Everything this process knows plus on-disk records from other processes.
    pub fn list(&self) -> Vec<BriefingInfo> {
        let mut infos: Vec<BriefingInfo> = self.records.lock().unwrap().values().map(Record::info).collect();
        if let Some(store) = &self.config.store {
            for stored in store.list() {
                if infos.iter().any(|i| i.id == stored.id) {
                    continue;
                }
                let mut info = Record::from_stored(stored).info();
                info.adopted = false;
                info.on_disk_only = true;
                infos.push(info);
            }
        }
        infos.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
        infos
    }

    pub fn token_for(&self, id: &str) -> Option<String> {
        self.ensure_loaded(id);
        self.records.lock().unwrap().get(id).map(|r| r.token.clone())
    }

    fn snapshot(&self, id: &str) -> Result<(watch::Receiver<BriefingStatus>, Option<BriefingResponse>), HubError> {
        let records = self.records.lock().unwrap();
        let record = records.get(id).ok_or(HubError::NotFound)?;
        Ok((record.status.subscribe(), record.result.clone()))
    }

    /// Wait up to `timeout` for the briefing to finish. Periodically re-reads the on-disk
    /// record so a submission served by another process is picked up too.
    pub async fn wait(&self, id: &str, timeout: Duration) -> Result<WaitOutcome, HubError> {
        self.ensure_loaded(id);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.reconcile(id);
            let (mut rx, result) = self.snapshot(id)?;
            if *rx.borrow() != BriefingStatus::Active {
                return Ok(WaitOutcome::Done(result.unwrap_or_default()));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(WaitOutcome::Pending);
            }
            let slice = if self.config.store.is_some() { remaining.min(RECONCILE_EVERY) } else { remaining };
            match tokio::time::timeout(slice, rx.wait_for(|s| *s != BriefingStatus::Active)).await {
                Ok(Ok(_)) => {
                    let (_, result) = self.snapshot(id)?;
                    return Ok(WaitOutcome::Done(result.unwrap_or_default()));
                }
                Ok(Err(_)) => return Err(HubError::NotFound),
                Err(_) => continue,
            }
        }
    }

    pub fn active_count(&self) -> usize {
        self.records.lock().unwrap().values().filter(|r| *r.status.borrow() == BriefingStatus::Active).count()
    }

    /// Drop expired records (memory and disk). Called on every create; safe to call any time.
    pub fn sweep(&self) {
        let now = now_secs();
        {
            let mut records = self.records.lock().unwrap();
            let mut tokens = self.tokens.lock().unwrap();
            records.retain(|_, record| {
                let keep = match record.finished_at {
                    Some(finished) => now.saturating_sub(finished) < self.config.finished_ttl.as_secs(),
                    None => now.saturating_sub(record.created_at) < self.config.active_ttl.as_secs(),
                };
                if !keep {
                    if record.finished_at.is_none() {
                        record.status.send_replace(BriefingStatus::Cancelled);
                    }
                    tokens.remove(&record.token);
                }
                keep
            });
        }
        if let Some(store) = &self.config.store {
            store.sweep(self.config.finished_ttl, self.config.active_ttl);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::demo;
    use serde_json::json;

    #[tokio::test]
    async fn create_submit_wait_roundtrip() {
        let hub = Hub::new(HubConfig::default());
        let created = hub.create(demo(), Some("test".into()));
        assert_eq!(hub.status(&created.id), Some(BriefingStatus::Active));

        let page = hub.page_payload(&created.token).unwrap();
        assert_eq!(page["status"], "active");
        assert_eq!(page["draft"], Value::Null);
        assert!(hub.page_payload("nope").is_none());

        assert_eq!(hub.wait(&created.id, Duration::from_millis(20)).await, Ok(WaitOutcome::Pending));

        hub.submit_by_token(&created.token, &json!({"overallNote": "great"}), false).unwrap();
        match hub.wait(&created.id, Duration::from_secs(1)).await.unwrap() {
            WaitOutcome::Done(result) => assert_eq!(result.overall_note, "great"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            hub.submit_by_token(&created.token, &json!({}), false),
            Err(HubError::AlreadyFinished(BriefingStatus::Completed))
        );
        assert!(!hub.cancel(&created.id));
        assert_eq!(hub.wait("missing", Duration::from_millis(1)).await, Err(HubError::NotFound));
        assert_eq!(hub.info(&created.id).unwrap().source.as_deref(), Some("test"));
    }

    #[test]
    fn drafts_are_revisioned() {
        let hub = Hub::new(HubConfig::default());
        let created = hub.create(demo(), None);
        let draft = json!({"current": 2, "state": {"chunks": {"0": {"note": "hi", "checkpoint": "", "status": ""}}, "decisions": {}, "annotations": [{}], "overallNote": ""}, "updatedAt": 5});
        assert_eq!(hub.save_draft(&created.token, Some(0), draft.clone()), Ok(DraftSave::Saved { revision: 1 }));
        assert_eq!(hub.save_draft(&created.token, None, draft.clone()), Ok(DraftSave::Saved { revision: 2 }));
        match hub.save_draft(&created.token, Some(1), json!({})).unwrap() {
            DraftSave::Stale { revision: 2, draft: existing } => assert_eq!(existing, draft),
            other => panic!("unexpected {other:?}"),
        }
        let page = hub.page_payload(&created.token).unwrap();
        assert_eq!(page["draftRevision"], 2);
        assert_eq!(page["draft"]["current"], 2);
        let summary = hub.info(&created.id).unwrap().draft.unwrap();
        assert_eq!(summary.screen, 3);
        assert_eq!(summary.annotations, 1);
        assert_eq!(summary.section_notes, 1);
        assert_eq!(summary.screens, (demo().chunks.len() + demo().decisions.len() + 1) as u64);
        hub.cancel(&created.id);
        assert!(matches!(hub.save_draft(&created.token, None, json!({})), Err(HubError::AlreadyFinished(_))));
    }

    #[tokio::test]
    async fn wake_waiter_on_cancel() {
        let hub = std::sync::Arc::new(Hub::new(HubConfig::default()));
        let created = hub.create(demo(), None);
        let waiter = {
            let hub = hub.clone();
            let id = created.id.clone();
            tokio::spawn(async move { hub.wait(&id, Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(hub.cancel(&created.id));
        match waiter.await.unwrap().unwrap() {
            WaitOutcome::Done(result) => assert!(result.cancelled),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(hub.page_payload(&created.token).unwrap()["status"], "cancelled");
    }

    #[test]
    fn sweep_expires_records() {
        let hub = Hub::new(HubConfig { finished_ttl: Duration::ZERO, active_ttl: Duration::ZERO, store: None });
        let created = hub.create(demo(), None);
        hub.sweep();
        assert!(hub.status(&created.id).is_none());
        assert!(hub.page_payload(&created.token).is_none());
    }

    #[tokio::test]
    async fn records_survive_via_store() {
        let dir = tempfile::tempdir().unwrap();
        let config = || HubConfig { store: Some(Store::open(dir.path()).unwrap()), ..HubConfig::default() };

        // Process A creates a briefing and the user saves a draft, then A dies.
        let a = Hub::new(config());
        let created = a.create(demo(), Some("a".into()));
        a.set_url(&created.id, "http://a.example/briefing/x");
        a.save_draft(&created.token, None, json!({"current": 1, "state": {}, "updatedAt": 1})).unwrap();
        drop(a);

        // Process B adopts it by id (agent side) and by token (browser side).
        let b = Hub::new(config());
        let info = b.info(&created.id).unwrap();
        assert!(info.adopted);
        assert_eq!(info.url.as_deref(), Some("http://a.example/briefing/x"));
        assert_eq!(info.draft.unwrap().screen, 2);
        assert_eq!(b.page_payload(&created.token).unwrap()["draft"]["current"], 1);

        // A third process (the one the browser talks to) submits; B's waiter sees it via disk.
        let c = Hub::new(config());
        c.submit_by_token(&created.token, &json!({"overallNote": "done"}), false).unwrap();
        match b.wait(&created.id, Duration::from_secs(1)).await.unwrap() {
            WaitOutcome::Done(result) => assert_eq!(result.overall_note, "done"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(b.status(&created.id), Some(BriefingStatus::Completed));

        // A brand-new process can fetch the result with no live record at all.
        let d = Hub::new(config());
        assert!(matches!(d.wait(&created.id, Duration::from_millis(1)).await, Ok(WaitOutcome::Done(_))));
        let listed = d.list();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].on_disk_only);

        // Listing without adopting reports disk-only records.
        let e = Hub::new(config());
        assert!(e.list()[0].on_disk_only);
    }
}
